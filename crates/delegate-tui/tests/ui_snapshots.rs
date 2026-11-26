use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use delegate_tui::app::{App, Overlay, WriteAction};
use delegate_tui::data::{parse_lane_json, parse_status_jsonl, parse_status_jsonl_at};
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
    let fixed_now = parse_iso_utc_epoch("2026-11-22T10:43:00Z");
    let repo_a = parse_status_jsonl_at(
        Path::new("/home/dev/demo-api"),
        "~/demo-api",
        REPO_A_JSONL,
        fixed_now,
    );
    let repo_b = parse_status_jsonl_at(
        Path::new("/home/dev/web-app"),
        "~/web-app",
        REPO_B_JSONL,
        fixed_now,
    );
    let lane = parse_lane_json(
        r#"{"maxHeavy":6,"heavyJobs":94,"threads":96,"budget":94,"poolTokens":88,"heavyJobsExplicit":false,"concurrencySummary":"线程 96 − 2 = 94 · 令牌 88/88 空闲 31","pool":{"target":88,"total":88,"free":31,"borrowed":57,"leases":[{"rank":1,"used":61,"share":62},{"rank":2,"used":31,"share":31}]},"local":[{"label":"demo-api: cargo test","since":"2026-11-22T10:40:00Z","running":true,"rank":1,"used":61,"share":62},{"label":"web-app: cargo build","since":"2026-11-22T10:41:00Z","running":true,"rank":2,"used":31,"share":31}],"remoteMaxHeavy":4,"remote":[]}"#,
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
        ..AppSnapshot::default()
    };
    let mut app = App::new(
        snap,
        PathBuf::from("delegate"),
        PathBuf::from("/home/dev/demo-api"),
        5,
    );
    // Cache Markdown result for api-r2 specifically (regardless of default sort order)
    if let Some(run) = app
        .snapshot
        .repos
        .iter()
        .flat_map(|r| r.runs.iter())
        .find(|r| r.run == "20261122-103000-api-r2")
        .cloned()
    {
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
    use delegate_tui::app::ViewTab;

    let mut app = make_sample_app();
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Runs);
    // Urgent run (移动端样式修正) sorts first in ByStatus; move down once to select 接口鉴权 (with cached result)
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    let screen = render_to_string(&mut app, 160, 34);

    // Rounded borders & unified 5-tab top bar with fixed pulse counts
    assert!(screen.contains('╭') && screen.contains('╰'), "{screen}");
    assert!(screen.contains("delegate"), "{screen}");
    assert!(screen.contains("1 总览") && screen.contains("3 运行"), "{screen}");
    assert!(screen.contains("待主控 2"), "{screen}");
    assert!(screen.contains("在跑 1"), "{screen}");
    assert!(screen.contains("排队 1"), "{screen}");

    // Default ByStatus grouping: cross-repo status headers + folded merged section
    assert!(screen.contains("待主控处理 (2)"), "{screen}");
    assert!(screen.contains("运行与返工 (1)"), "{screen}");
    assert!(screen.contains("排队与待续派 (1)"), "{screen}");
    assert!(screen.contains("▸ 已合未清 1 条"), "{screen}");

    // Collapsed chain, relative time ("3 分钟前"), mini diff bar, badges, selected stripe
    assert!(screen.contains('▎'), "{screen}");
    assert!(screen.contains("（第2轮）"), "{screen}");
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
    assert!(screen.contains("验收 ✓ cargo test -p api"), "{screen}");
    assert!(!screen.contains("最近命令"), "accept command should not duplicate under 最近命令: {screen}");
    assert!(screen.contains("改动分布"), "{screen}");
    assert!(screen.contains("crates/api"), "{screen}");
    assert!(screen.contains("下一步"), "{screen}");

    // Toggle 'v' to ByRepo grouping -> multi-repo headers & per-repo groups appear
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    let by_repo = render_to_string(&mut app, 160, 34);
    assert!(by_repo.contains("~/demo-api"), "{by_repo}");
    assert!(by_repo.contains("~/web-app"), "{by_repo}");
    assert!(by_repo.contains("待处理 (1)"), "{by_repo}");
    assert!(by_repo.contains("运行中 (1)"), "{by_repo}");
    assert!(by_repo.contains("排队 (1)"), "{by_repo}");

    // Bottom bar mini lane meter + pool tokens + per-command shares + host load + time
    assert!(screen.contains("Lane ██░░░░ 2/6"), "{screen}");
    assert!(screen.contains("令牌 88/88 空闲 31"), "{screen}");
    assert!(screen.contains("#1 61/62 · #2 31/31"), "{screen}");
    assert!(!screen.contains("demo-api: cargo test"), "active label should not crowd bottom bar: {screen}");
    assert!(screen.contains("负载 1.8 1.4"), "{screen}");
    assert!(screen.contains("10:52:00"), "{screen}");
}

#[test]
fn snapshot_narrow_80_columns_uses_readable_stacked_layout() {
    let mut app = make_sample_app();
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    let screen = render_to_string(&mut app, 80, 30);

    assert!(screen.contains("运行列表 (活跃 4 · 按状态聚合"), "{screen}");
    assert!(screen.contains("接口鉴权"), "{screen}");
    assert!(screen.contains("（第2轮）"), "{screen}");
    assert!(screen.contains("3 分钟前") || screen.contains("3分前"), "{screen}");
    assert!(screen.contains("结论摘要"), "{screen}");
    assert!(screen.contains("Lane ██░░░░ 2/6"), "{screen}");
    assert!(screen.contains("令牌 88/88 闲31"), "{screen}");
    assert!(screen.contains("v分组") || screen.contains("1-5/[]"), "{screen}");
}

#[test]
fn snapshot_overlays_result_diff_and_command_modal() {
    let mut app = make_sample_app();
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    // Move to api-r2 (index 1 under 待主控处理)
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));

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
        refreshed_at: "11:00:00".to_string(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("."), 5);
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let empty = render_to_string(&mut app, 80, 24);
    assert!(empty.contains("暂无委派运行") || empty.contains("暂无委派任务"), "{empty}");
    // No decorative filler in empty detail pane
    assert!(!empty.contains("选择一个任务以查看详情"), "{empty}");

    let tiny = render_to_string(&mut app, 16, 6);
    assert!(tiny.contains("窗口过小"), "{tiny}");
}

#[test]
fn snapshot_events_tab_liveness_activity_and_dag_queue() {
    use delegate_tui::data::{parse_dag_queue_jsonl, parse_events_jsonl};
    let raw_runs = r#"
{"run":"20261123-090000-live","name":"事件流实现","state":"running","mode":"write","modelName":"neon","elapsedSeconds":180,"ageSeconds":180,"activity":{"kind":"running_command","cmd":"cargo test -p delegate","elapsedSeconds":45,"label":"在跑命令：cargo test -p delegate（45秒）"}}
{"run":"20261123-090100-warn","name":"卡住观察","state":"running","mode":"write","modelName":"sol","elapsedSeconds":900,"ageSeconds":900,"activity":{"kind":"thinking","label":"在思考（卡住提醒：已 15 分）"},"stallWarning":{"kind":"idle","seconds":900,"detail":"已 900 秒无新动作"}}
"#;
    let raw_events = r#"
{"seq":1,"at":"2026-11-23T09:00:00Z","run":"20261123-090000-live","repo":"/home/dev/demo-api","kind":"started","payload":{"name":"事件流实现","mode":"write","model":"neon"}}
{"seq":2,"at":"2026-11-23T09:05:00Z","run":"20261123-090100-warn","repo":"/home/dev/demo-api","kind":"stall_warning","payload":{"name":"卡住观察","stallKind":"idle","detail":"已 900 秒无新动作"}}
{"seq":3,"at":"2026-11-23T09:06:00Z","run":"","repo":"/home/dev/demo-api","kind":"queued","payload":{"scope":"dag","queueName":"step-2","after":["20261123-090000-live"]}}
"#;
    let raw_queue = r#"
{"name":"step-2","status":"pending","after":["20261123-090000-live"],"repo":"/home/dev/demo-api","promptFile":"/tmp/step2.md","createdAt":"2026-11-23T09:06:00Z"}
"#;
    let repo = parse_status_jsonl(Path::new("/home/dev/demo-api"), "~/demo-api", raw_runs);
    let snap = AppSnapshot {
        repos: vec![repo],
        events: parse_events_jsonl(raw_events),
        events_has_more: true,
        events_earliest_seq: Some(500),
        dag_queue: parse_dag_queue_jsonl(raw_queue),
        refreshed_at: "09:06:10".into(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/demo-api"), 5);

    // Switch to Runs tab ('3'): shows full liveness badges ("跑命令", "卡住警"), detail "正在做什么", and DAG queue section
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let runs_screen = render_to_string(&mut app, 140, 32);
    assert!(runs_screen.contains("跑命令"), "{runs_screen}");
    assert!(runs_screen.contains("卡住警"), "{runs_screen}");
    assert!(runs_screen.contains("正在做什么"), "{runs_screen}");
    assert!(runs_screen.contains("排队与待续派 (1)"), "{runs_screen}");
    assert!(runs_screen.contains("step-2"), "{runs_screen}");
    assert!(runs_screen.contains("5 事件"), "{runs_screen}");

    // Press '5' to switch to Events tab (and verify tail-window hint line!)
    app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
    let events_screen = render_to_string(&mut app, 140, 32);
    assert!(events_screen.contains("事件时间线 (显示 3 条 · 跟随最新)"), "{events_screen}");
    assert!(events_screen.contains("#1"), "{events_screen}");
    assert!(events_screen.contains("卡警"), "{events_screen}");
    assert!(events_screen.contains("step-2"), "{events_screen}");
    assert!(events_screen.contains("事件 #3"), "{events_screen}");
    assert!(
        events_screen.contains("更早的事件：delegate events --since 300"),
        "tail window hint missing:\n{events_screen}"
    );
}

#[test]
fn snapshot_review_states_and_review_events() {
    use delegate_tui::data::parse_events_jsonl;
    let raw_runs = r#"
{"run":"20261123-100000-r-ing","name":"审中任务","state":"running","mode":"write","modelName":"neon","elapsedSeconds":60,"ageSeconds":60,"review":{"status":"reviewing","round":1,"reworkRounds":0}}
{"run":"20261123-100100-r-rw","name":"返工任务","state":"running","mode":"write","modelName":"neon","elapsedSeconds":120,"ageSeconds":120,"review":{"status":"reworking","verdict":"REWORK","round":2,"reworkRounds":2,"needsAttention":false}}
{"run":"20261123-100200-r-pass","name":"通过任务","state":"delivered","mode":"write","modelName":"neon","elapsedSeconds":90,"ageSeconds":150,"accept":{"command":"cargo test","ok":true},"review":{"status":"done","verdict":"PASS","round":1,"reworkRounds":0}}
{"run":"20261123-100300-r-ctrl","name":"主审任务","state":"delivered","mode":"write","modelName":"neon","elapsedSeconds":100,"ageSeconds":110,"accept":{"command":"cargo test","ok":true},"review":{"status":"done","verdict":"NEEDS_CONTROLLER","round":1,"reworkRounds":0,"needsAttention":true,"upgradedBy":"controllerReviewPaths","controllerReview":{"paths":["skills/delegate/SKILL.md"],"hash":"sha256:abcdef012345"}},"sign":{"required":true,"signed":false,"valid":false,"invalidated":false,"hash":"sha256:abcdef012345","paths":["skills/delegate/SKILL.md"]}}
"#;
    let raw_events = r#"
{"seq":10,"at":"2026-11-23T10:00:00Z","run":"20261123-100000-r-ing","repo":"/home/dev/demo-api","kind":"review_started","payload":{"name":"审中任务","round":1,"incremental":false}}
{"seq":11,"at":"2026-11-23T10:01:00Z","run":"20261123-100100-r-rw","repo":"/home/dev/demo-api","kind":"review_verdict","payload":{"name":"返工任务","round":2,"verdict":"REWORK"}}
{"seq":12,"at":"2026-11-23T10:01:01Z","run":"20261123-100100-r-rw","repo":"/home/dev/demo-api","kind":"rework_sent","payload":{"name":"返工任务","round":2,"reworkRounds":2}}
{"seq":13,"at":"2026-11-23T10:03:00Z","run":"20261123-100300-r-ctrl","repo":"/home/dev/demo-api","kind":"needs_attention","payload":{"name":"主审任务","reason":"controllerReviewPaths","detail":"命中受控路径"}}
"#;
    let repo = parse_status_jsonl(Path::new("/home/dev/demo-api"), "~/demo-api", raw_runs);
    let snap = AppSnapshot {
        repos: vec![repo],
        events: parse_events_jsonl(raw_events),
        refreshed_at: "10:04:00".into(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/demo-api"), 5);
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let screen = render_to_string(&mut app, 150, 34);
    assert!(screen.contains("需主审"), "{screen}");
    assert!(screen.contains("NEEDS_CONTROLLER"), "{screen}");
    assert!(screen.contains("PASS"), "{screen}");
    assert!(screen.contains("审查中"), "{screen}");
    assert!(screen.contains("返工2轮"), "{screen}");
    assert!(!screen.contains("REWORK 第2轮"), "rework must not duplicate English REWORK:\n{screen}");
    assert!(screen.contains("controllerReviewPaths"), "{screen}");
    assert!(screen.contains("sha256:abcdef012345"), "{screen}");

    app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
    let ev_screen = render_to_string(&mut app, 150, 34);
    assert!(ev_screen.contains("开审"), "{ev_screen}");
    assert!(ev_screen.contains("审结"), "{ev_screen}");
    assert!(ev_screen.contains("返工"), "{ev_screen}");
    assert!(ev_screen.contains("关注"), "{ev_screen}");
}

#[test]
fn snapshot_merges_and_candidates_tabs_and_events() {
    use delegate_tui::data::{parse_candidates_jsonl, parse_events_jsonl, parse_merges_jsonl};
    let raw_merges = r#"
{"mergeRun":"20261123-110000-merge","name":"merge","repo":"/home/dev/demo-api","baseHead":"1111111abcdef","head":"2222222abcdef","state":"delivered","startedAt":"2026-11-23T11:00:00Z","finishedAt":"2026-11-23T11:02:00Z","mergedCount":1,"skippedCount":1,"autoMergeCount":1,"allAutoMerge":true,"applied":false,"reportPath":"/tmp/merge-report.md","mergeAccept":{"command":"cargo test","ok":true,"skipped":false},"items":[{"run":"20261123-100000-docs","name":"docs","status":"MERGED","commit":"2222222abcdef","title":"docs: update guide","acceptOk":true,"autoMerge":true,"reason":""},{"run":"20261123-100500-conflict","name":"conflict","status":"SKIPPED","commit":"","title":"","acceptOk":null,"autoMerge":false,"reason":"语义冲突 / 冲突标记：src/lib.rs"}]}
"#;
    let raw_candidates = r#"
{"id":"r-a1b2c3d4","repo":"/home/dev/demo-api","repoName":"demo-api","title":"补齐 capabilities 缺口测试","evidence":["crates/api/src/lib.rs:42","collect#2:L15"],"suggestion":"为未覆盖的 endpoint 补单测","cost":"低（15 分钟）","briefReady":true,"briefNote":"","fingerprint":"sha256:a1b2c3d4","firstSeenAt":"2026-11-23T02:30:00Z","lastSeenAt":"2026-11-23T02:30:00Z","pickedAt":"","boardPath":"/tmp/board.md"}
"#;
    let raw_events = r#"
{"seq":20,"at":"2026-11-23T11:00:00Z","run":"20261123-110000-merge","repo":"/home/dev/demo-api","kind":"merge_started","payload":{"name":"merge","count":2}}
{"seq":21,"at":"2026-11-23T11:01:00Z","run":"20261123-110000-merge","repo":"/home/dev/demo-api","kind":"merge_item","payload":{"sourceRun":"20261123-100000-docs","status":"MERGED","autoMerge":true}}
{"seq":22,"at":"2026-11-23T11:02:00Z","run":"20261123-110000-merge","repo":"/home/dev/demo-api","kind":"merge_done","payload":{"mergedCount":1,"skippedCount":1,"autoMergeCount":1,"mergeAcceptOk":true}}
{"seq":23,"at":"2026-11-23T02:30:00Z","run":"radar-demo-api","repo":"/home/dev/demo-api","kind":"radar_done","payload":{"added":1,"total":1}}
"#;
    let snap = AppSnapshot {
        events: parse_events_jsonl(raw_events),
        merges: parse_merges_jsonl(raw_merges),
        candidates: parse_candidates_jsonl(raw_candidates),
        refreshed_at: "11:03:00".into(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/demo-api"), 5);

    // Top bar has only the 5 tabs (no standalone 3合入 / 4候选 tabs)
    let home = render_to_string(&mut app, 148, 32);
    assert!(!home.contains("3合入"), "{home}");
    assert!(!home.contains("4候选"), "{home}");

    // Switch to Runs tab ('3') -> Merges folded inside Runs; Space expands it and 'a' opens apply --keep-commits modal
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let runs_folded = render_to_string(&mut app, 148, 32);
    assert!(runs_folded.contains("▸ 合入批次 1 条"), "{runs_folded}");
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    let merges_screen = render_to_string(&mut app, 148, 32);
    assert!(merges_screen.contains("20261123-110000-merge"), "{merges_screen}");
    assert!(merges_screen.contains("可自主合入"), "{merges_screen}");
    assert!(merges_screen.contains("SKIPPED"), "{merges_screen}");
    assert!(merges_screen.contains("delegate apply --keep-commits 20261123-110000-merge"), "{merges_screen}");

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    let apply_modal = render_to_string(&mut app, 148, 32);
    assert!(apply_modal.contains("apply --keep-commits 20261123-110000-merge"), "{apply_modal}");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    // Switch to Work tab ('2') -> Radar candidates folded inside Work; Space expands and 'p' opens radar pick modal
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    let work_folded = render_to_string(&mut app, 148, 32);
    assert!(work_folded.contains("▸ 侦察候选 1 条"), "{work_folded}");
    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    let cand_screen = render_to_string(&mut app, 148, 32);
    assert!(cand_screen.contains("r-a1b2c3d4"), "{cand_screen}");
    assert!(cand_screen.contains("补齐 capabilities 缺口测试"), "{cand_screen}");
    assert!(cand_screen.contains("crates/api/src/lib.rs:42"), "{cand_screen}");
    assert!(cand_screen.contains("delegate radar pick r-a1b2c3d4"), "{cand_screen}");

    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
    let pick_modal = render_to_string(&mut app, 148, 32);
    assert!(pick_modal.contains("radar pick r-a1b2c3d4"), "{pick_modal}");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    // Switch to Events tab ('5') and verify merge/radar event badges
    app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
    let ev_screen = render_to_string(&mut app, 148, 32);
    assert!(ev_screen.contains("合始"), "{ev_screen}");
    assert!(ev_screen.contains("合项"), "{ev_screen}");
    assert!(ev_screen.contains("合结"), "{ev_screen}");
    assert!(ev_screen.contains("侦察"), "{ev_screen}");
}

fn make_work_sample_app(card_path: Option<&Path>) -> App {
    use delegate_tui::data::parse_work_jsonl;

    let raw_runs = r#"
{"run":"20261123-100000-r33","name":"6.33 快修","workId":"W033","state":"running","mode":"write","modelName":"neon","elapsedSeconds":720,"ageSeconds":720,"activity":{"kind":"running_command","cmd":"scripts/verify.sh","elapsedSeconds":30,"label":"在跑命令"}}
{"run":"20261123-100500-r29","name":"导出工具","workId":"W029","state":"running","mode":"write","modelName":"sol","elapsedSeconds":180,"ageSeconds":180,"review":{"status":"reviewing","round":1,"reworkRounds":0}}
{"run":"20261123-094000-r10","name":"鉴权改动","workId":"W010","state":"delivered","mode":"write","modelName":"neon","elapsedSeconds":300,"ageSeconds":500,"changes":{"files":2,"added":50,"deleted":10},"pendingChanges":{"files":2,"added":50,"deleted":10},"accept":{"command":"cargo test","ok":true,"exitCode":0},"review":{"status":"done","verdict":"PASS","round":1,"reworkRounds":0}}
"#;
    let brief_str = card_path
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "docs/owner/directions/streaming.md".to_string());
    let raw_work = format!(
        r#"
{{"id":"D002","title":"身体流式动作","repo":"/home/dev/app","repoLabel":"app","state":"active","brief":"{brief}","owner":{{"kind":"direction","ask":"身体按意图实时生成连贯动作","recommendation":"先跑 2 天验证再定","ifUndecided":"沿用固定片段","cost":"2 天"}},"createdAt":"2026-11-23T01:00:00Z","updatedAt":"2026-11-23T01:00:00Z","stateSince":"2026-11-23T01:00:00Z","next":"work show D002"}}
{{"id":"Q18","title":"删掉旧星痕","repo":"/home/dev/app","repoLabel":"app","state":"attention","owner":{{"kind":"act","ask":"在管理页删掉两条旧星痕","recommendation":"管理员私聊通知即可","ifUndecided":"她还会以为不能看视频","cost":"2 分钟"}},"createdAt":"2026-11-23T02:00:00Z","updatedAt":"2026-11-23T02:00:00Z","stateSince":"2026-11-23T02:00:00Z","next":"work close Q18 --evidence \"所有者：已处理\""}}
{{"id":"Q20","title":"开启每日摘要","repo":"/home/dev/app","repoLabel":"app","state":"attention","owner":{{"kind":"nod","ask":"是否默认开启每日摘要？","recommendation":"建议照做，零额外成本","ifUndecided":"保持手工触发","cost":"0"}},"createdAt":"2026-11-23T02:05:00Z","updatedAt":"2026-11-23T02:05:00Z","stateSince":"2026-11-23T02:05:00Z","next":"work close Q20 --evidence \"所有者：同意\""}}
{{"id":"Q21","title":"模型路由方案","repo":"/home/dev/app","repoLabel":"app","state":"attention","brief":"{brief}","owner":{{"kind":"card","ask":"看卡权衡路由方案 A/B","recommendation":"建议选方案 A","ifUndecided":"暂用默认路由","cost":"月增 $20"}},"createdAt":"2026-11-23T02:10:00Z","updatedAt":"2026-11-23T02:10:00Z","stateSince":"2026-11-23T02:10:00Z","next":"work close Q21 --evidence \"所有者：选 A\""}}
{{"id":"Q19","title":"买官方直连 key","repo":"/home/dev/app","repoLabel":"app","state":"done","closedKind":"dropped","closeReason":"现有渠道够用，不买","owner":{{"kind":"nod","ask":"买官方直连 key？","recommendation":"不建议买","ifUndecided":"继续用现有渠道","cost":"$100/月"}},"createdAt":"2026-11-22T02:00:00Z","updatedAt":"2026-11-23T03:00:00Z","stateSince":"2026-11-23T03:00:00Z"}}
{{"id":"W010","title":"接口鉴权交付","repo":"/home/dev/demo-api","repoLabel":"demo-api","state":"attention","doneWhen":"合入并跑通鉴权回归","runs":["20261123-094000-r10"],"focus":"待合 +2f 验✓ 审PASS","next":"apply r10","createdAt":"2026-11-23T01:00:00Z","updatedAt":"2026-11-23T04:00:00Z","stateSince":"2026-11-23T04:00:00Z"}}
{{"id":"W011","title":"合入后核对日志","repo":"/home/dev/demo-api","repoLabel":"demo-api","state":"verify","doneWhen":"核对生产日志无报错","followups":[{{"id":1,"kind":"note","text":"观察 10 分钟错误率","done":false}}],"focus":"待办 1 条","next":"work close W011 --evidence \"…\"","createdAt":"2026-11-23T01:00:00Z","updatedAt":"2026-11-23T04:00:00Z","stateSince":"2026-11-23T04:00:00Z"}}
{{"id":"W033","title":"6.33 审计快修","repo":"/home/dev/demo-api","repoLabel":"demo-api","state":"active","doneWhen":"verify 全绿","runs":["20261123-100000-r33"],"focus":"run r33 运行中 12m","next":"wait r33","createdAt":"2026-11-23T02:00:00Z","updatedAt":"2026-11-23T05:00:00Z","stateSince":"2026-11-23T05:00:00Z"}}
{{"id":"W034","title":"6.34 分层验收","repo":"/home/dev/demo-api","repoLabel":"demo-api","state":"blocked","waitOn":["work_closed:W033"],"waitOnStatus":[{{"condition":"work_closed:W033","satisfied":false,"detail":"等 W033 关闭（当前 active）","pointer":"delegate work show W033"}}],"focus":"等 W033 关闭","next":"work show W034","createdAt":"2026-11-23T02:10:00Z","updatedAt":"2026-11-23T05:00:00Z","stateSince":"2026-11-23T05:00:00Z"}}
{{"id":"W035","title":"6.35 预合并与快进 apply","repo":"/home/dev/demo-api","repoLabel":"demo-api","state":"blocked","waitOn":["work_closed:W034","owner:Q20"],"waitOnStatus":[{{"condition":"work_closed:W034","satisfied":false,"detail":"等 W034 关闭（当前 blocked）","pointer":"delegate work show W034"}},{{"condition":"owner:Q20","satisfied":false,"detail":"等所有者 Q20","pointer":"delegate work show Q20"}}],"focus":"等 W034 关闭","next":"work show W035","createdAt":"2026-11-23T02:20:00Z","updatedAt":"2026-11-23T05:00:00Z","stateSince":"2026-11-23T05:00:00Z"}}
{{"id":"W029","title":"导出工具基础层","repo":"/home/dev/app","repoLabel":"app","state":"active","runs":["20261123-100500-r29"],"focus":"run r29 运行中","next":"wait r29","createdAt":"2026-11-23T02:00:00Z","updatedAt":"2026-11-23T05:00:00Z","stateSince":"2026-11-23T05:00:00Z"}}
{{"id":"W031","title":"无损搬家上层流程","repo":"/home/dev/app","repoLabel":"app","state":"blocked","waitOn":["run_applied:20261123-100500-r29"],"waitOnStatus":[{{"condition":"run_applied:20261123-100500-r29","satisfied":false,"detail":"等 r29 合入","pointer":"delegate show 20261123-100500-r29"}}],"focus":"等 r29 合入","next":"work show W031","createdAt":"2026-11-23T02:30:00Z","updatedAt":"2026-11-23T05:00:00Z","stateSince":"2026-11-23T05:00:00Z"}}
{{"id":"W040","title":"清理旧兼容列","repo":"/home/dev/app","repoLabel":"app","state":"ready","focus":"可派发 2h","next":"start --work W040 \"…\"","createdAt":"2026-11-23T01:00:00Z","updatedAt":"2026-11-23T01:00:00Z","stateSince":"2026-11-23T01:00:00Z"}}
{{"id":"W041","title":"跟进所有者问题","repo":"/home/dev/app","repoLabel":"app","state":"blocked","waitOn":["owner:Q12"],"waitOnStatus":[{{"condition":"owner:Q12","satisfied":false,"detail":"等所有者 Q12","pointer":"delegate work show Q12"}}],"focus":"等所有者 Q12","next":"work show W041","createdAt":"2026-11-23T01:00:00Z","updatedAt":"2026-11-23T01:00:00Z","stateSince":"2026-11-23T01:00:00Z"}}
{{"id":"W042","title":"夜间压测计划","repo":"/home/dev/app","repoLabel":"app","state":"deferred","deferReason":"等低峰窗口","deferUntil":"2026-11-24T00:00:00Z","focus":"暂缓至 2026-11-24","next":"work show W042","createdAt":"2026-11-23T01:00:00Z","updatedAt":"2026-11-23T01:00:00Z","stateSince":"2026-11-23T01:00:00Z"}}
{{"id":"W001","title":"今天完成的基础重构","repo":"/home/dev/app","repoLabel":"app","state":"done","closedKind":"closed","closeEvidence":"abc1234 测试全绿","createdAt":"2026-11-23T00:30:00Z","updatedAt":"2026-11-23T04:00:00Z","stateSince":"2026-11-23T04:00:00Z"}}
"#,
        brief = brief_str
    );

    let repo = parse_status_jsonl(Path::new("/home/dev/demo-api"), "~/demo-api", raw_runs);
    let snap = AppSnapshot {
        repos: vec![repo],
        work: parse_work_jsonl(&raw_work),
        today_ymd: "2026-11-23".to_string(),
        refreshed_at: "10:15:00".to_string(),
        ..AppSnapshot::default()
    };
    App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/demo-api"), 5)
}

#[test]
fn work_view_renders_grouping_dependency_chains_live_status_at_120_80_50_and_empty() {
    use delegate_tui::app::ViewTab;

    let mut app = make_work_sample_app(None);
    // Press '2' to enter Work view (old 'w' tab key removed)
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Work);

    // 1. Wide 160 columns: unified 5-tab top bar, fixed pulse right side, 54:46 dual-pane
    let s160 = render_to_string(&mut app, 160, 34);
    let top160 = s160.lines().next().unwrap_or("");
    assert!(top160.contains("1 总览") && top160.contains("2 工作") && top160.contains("3 运行"), "{top160}");
    assert!(!top160.trim_end().ends_with('·'), "top bar must not leave trailing separator: {top160}");
    assert!(top160.contains("等你 4"), "{top160}");
    assert!(s160.contains("工作表 (推进 13"), "{s160}");
    assert!(s160.contains("今日完成 1 / 共 1"), "{s160}");
    assert!(s160.contains("定时/暂缓 1"), "{s160}");

    // Owner items pinned at top in 动手 -> 点头 -> 看卡 -> 方向 order (search inside Work table body, after alert strip)
    let work_body = &s160[s160.find("▾ 等你 (4)").expect("owner group")..];
    let pos_q18 = work_body.find("Q18").expect("Q18");
    let pos_q20 = work_body.find("Q20").expect("Q20");
    let pos_q21 = work_body.find("Q21").expect("Q21");
    let pos_d002 = work_body.find("D002").expect("D002");
    assert!(pos_q18 < pos_q20 && pos_q20 < pos_q21 && pos_q21 < pos_d002, "{s160}");

    // Group order: 等你 -> 需处理 -> 推进中 -> 就绪 -> 依赖等待 -> 暂缓折叠 -> 已完成折叠
    let pos_owner = s160.find("▾ 等你 (4)").expect("owner group");
    let pos_need = s160.find("▾ 需处理 (2)").expect("need group");
    let pos_prog = s160.find("▾ 推进中 (2 · 阻下游 3)").expect("prog group with downstream");
    let pos_ready = s160.find("▾ 就绪 (1)").expect("ready group");
    let pos_wait = s160.find("▾ 依赖等待 (1)").expect("wait group");
    let pos_defer = s160.find("▸ 暂缓 1 条").expect("defer fold");
    let pos_done_fold = s160.find("▸ 已完成 1 条").expect("done fold");
    assert!(
        pos_owner < pos_need
            && pos_need < pos_prog
            && pos_prog < pos_ready
            && pos_ready < pos_wait
            && pos_wait < pos_defer
            && pos_defer < pos_done_fold,
        "{s160}"
    );
    // Done and deferred items are folded by default
    assert!(!s160.contains("今天完成的基础重构"), "{s160}");
    assert!(!s160.contains("夜间压测计划"), "{s160}");

    // Dependency chain indentation: W033 -> W034 -> W035 (+1) and W029 -> W031
    assert!(s160.contains("└─ ◌ W034"), "{s160}");
    assert!(s160.contains("   └─ ◌ W035"), "{s160}");
    assert!(s160.contains("+1"), "{s160}");
    assert!(s160.contains("└─ ◌ W031"), "{s160}");

    // Dual-pane right side renders selected Q18 card automatically
    assert!(s160.contains("工作卡 · Q18"), "{s160}");
    assert!(s160.contains("主控建议：管理员私聊通知即可"), "{s160}");

    // Live run one-liners in 180-wide layout
    let s180 = render_to_string(&mut app, 180, 34);
    assert!(s180.contains("运行中 12 分钟 · 在跑验收") || s180.contains("在跑验收"), "{s180}");
    assert!(s180.contains("审查中"), "{s180}");
    assert!(s180.contains("待合入 · 验收✓ 审查 PASS") || s180.contains("待合入"), "{s180}");
    assert!(s180.contains("等 W033 关闭"), "{s180}");
    assert!(s180.contains("等所有者 Q12"), "{s180}");

    // 2. 80 columns (< 110 narrow: full-width single pane tree, unified top bar)
    let s80 = render_to_string(&mut app, 80, 32);
    assert_lines_fit_width(&s80, 80);
    let top80 = s80.lines().next().unwrap_or("");
    assert!(top80.contains("1总览") && top80.contains("2工作"), "{top80}");
    assert!(s80.contains("└─ ◌ W034"), "{s80}");
    assert!(s80.contains("+1"), "{s80}");

    // 3. 50 columns (mobile/compact layout: 2-line items, compact ↳ prefix)
    let s50 = render_to_string(&mut app, 50, 48);
    assert_lines_fit_width(&s50, 50);
    assert!(s50.contains("工作表 (推进 13"), "{s50}");
    assert!(s50.contains("↳ ◌ W034"), "{s50}");
    assert!(s50.contains(" ↳ ◌ W035"), "{s50}");
    assert!(s50.contains("等待 +1"), "{s50}");

    // 4. Dropped filter ('x' key) surfaces Q19
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    let s_drop = render_to_string(&mut app, 100, 20);
    assert!(s_drop.contains("已否 (1)"), "{s_drop}");
    assert!(s_drop.contains("Q19"), "{s_drop}");
    assert!(s_drop.contains("现有渠道够用"), "{s_drop}");
    // Clear filter with Esc
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    // 5. Empty worktable
    let empty_snap = AppSnapshot {
        refreshed_at: "11:00:00".into(),
        ..AppSnapshot::default()
    };
    let mut empty_app = App::new(empty_snap, PathBuf::from("delegate"), PathBuf::from("."), 5);
    empty_app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    let s_empty = render_to_string(&mut empty_app, 120, 20);
    assert!(s_empty.contains("当前无开放工作项"), "{s_empty}");
    assert!(s_empty.contains("等你 0"), "{s_empty}");
}

#[test]
fn work_card_markdown_rendering_and_bidirectional_run_linkage() {
    use delegate_tui::app::ViewTab;

    // Create a temporary owner direction card markdown file
    let tmp = std::env::temp_dir().join(format!(
        "delegate-tui-card-test-{}.md",
        std::process::id()
    ));
    std::fs::write(
        &tmp,
        "# Q21 路由方案权衡\n\n**方案 A** 延迟低，见 [`router.rs`](file:///tmp/router.rs)。\n\n- 成本可控\n",
    )
    .unwrap();

    let mut app = make_work_sample_app(Some(&tmp));
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Runs);
    // Run view initially selects W010's run (or first run) — verify detail pane shows linked work item ID!
    let runs_screen = render_to_string(&mut app, 140, 32);
    assert!(runs_screen.contains("[W010]") || runs_screen.contains("[W033]"), "{runs_screen}");
    assert!(runs_screen.contains("工作项:"), "{runs_screen}");

    // Press 'w' in Run view -> jumps straight to Work view selecting that run's work item!
    let expected_wid = app
        .snapshot
        .work_id_for_run(app.selected_run().unwrap())
        .unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Work);
    assert_eq!(
        app.selected_work_item().map(|w| w.id.as_str()),
        Some(expected_wid.as_str())
    );

    // Press 't' in Work view -> jumps back to Runs view selecting that item's run!
    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Runs);
    assert_eq!(
        app.snapshot.work_id_for_run(app.selected_run().unwrap()).as_deref(),
        Some(expected_wid.as_str())
    );

    // Jump to owner card item Q21 -> in wide 140x30 right pane shows Markdown card immediately,
    // and in narrow 100x30 pressing Enter opens L2 Work Card overlay cleanly!
    assert!(app.jump_to_work_id("Q21"));
    let wide_card = render_to_string(&mut app, 140, 30);
    assert!(wide_card.contains("工作卡 · Q21"), "{wide_card}");
    assert!(wide_card.contains("主控建议：建议选方案 A"), "{wide_card}");
    assert!(wide_card.contains("方案 A"), "{wide_card}");
    assert!(!wide_card.contains("**"), "raw markdown bold leaked: {wide_card}");

    // Draw narrow first so app knows viewport < 110, then Enter opens L2 Work Card overlay
    let _ = render_to_string(&mut app, 100, 30);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let card_screen = render_to_string(&mut app, 100, 30);
    assert!(card_screen.contains("工作卡 · Q21"), "{card_screen}");
    assert!(card_screen.contains("主控建议：建议选方案 A"), "{card_screen}");
    assert!(card_screen.contains("不定会怎样：暂用默认路由"), "{card_screen}");
    assert!(card_screen.contains("成本：月增 $20"), "{card_screen}");
    assert!(card_screen.contains("Q21 路由方案权衡"), "{card_screen}");
    assert!(card_screen.contains("方案 A"), "{card_screen}");
    assert!(card_screen.contains("router.rs"), "{card_screen}");
    assert!(!card_screen.contains("**"), "raw markdown bold leaked: {card_screen}");
    assert!(!card_screen.contains("file:///"), "raw markdown link leaked: {card_screen}");

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn premerge_status_renders_in_run_rows_detail_and_work_live_column() {
    use delegate_tui::data::parse_work_jsonl;

    let raw_runs = [
        r#"{"run":"20261123-100001-ready","name":"ready-task","state":"delivered","mode":"write","model":"p/write","ageSeconds":40,"elapsedSeconds":20,"changes":{"files":2,"added":10,"deleted":2},"accept":{"ok":true,"command":"cargo test"},"premerge":{"status":"ready","baseHead":"abcdef1234567890","baseShort":"abcdef1","changedCount":2},"workId":"W101"}"#,
        r#"{"run":"20261123-100002-running","name":"merging-task","state":"delivered","mode":"write","model":"p/write","ageSeconds":30,"elapsedSeconds":15,"changes":{"files":1,"added":5,"deleted":1},"accept":{"ok":true,"command":"cargo test"},"premerge":{"status":"running","baseHead":"abcdef1234567890","baseShort":"abcdef1"},"workId":"W102"}"#,
        r#"{"run":"20261123-100003-stale","name":"stale-task","state":"delivered","mode":"write","model":"p/write","ageSeconds":20,"elapsedSeconds":10,"changes":{"files":1,"added":3,"deleted":0},"accept":{"ok":true,"command":"cargo test"},"premerge":{"status":"stale","baseHead":"1111111234567890","baseShort":"1111111"},"workId":"W103"}"#,
        r#"{"run":"20261123-100004-conf","name":"conflict-task","state":"delivered","mode":"write","model":"p/write","ageSeconds":10,"elapsedSeconds":8,"changes":{"files":1,"added":4,"deleted":2},"accept":{"ok":true,"command":"cargo test"},"premerge":{"status":"conflict","baseHead":"abcdef1234567890","baseShort":"abcdef1","conflicts":["src/lib.rs"],"conflictCount":1,"log":"/tmp/run/premerge.log"},"workId":"W104"}"#,
    ]
    .join("\n");

    let repo = parse_status_jsonl(Path::new("/home/dev/demo-api"), "demo-api", &raw_runs);
    let raw_work = [
        r#"{"id":"W101","title":"Fast forward ready","repo":"/home/dev/demo-api","repoLabel":"demo-api","state":"attention","runs":["20261123-100001-ready"],"focus":"待合入","createdAt":"2026-11-23T01:00:00Z","updatedAt":"2026-11-23T02:00:00Z"}"#,
        r#"{"id":"W102","title":"Premerge running","repo":"/home/dev/demo-api","repoLabel":"demo-api","state":"attention","runs":["20261123-100002-running"],"focus":"待合入","createdAt":"2026-11-23T01:00:00Z","updatedAt":"2026-11-23T02:00:00Z"}"#,
        r#"{"id":"W103","title":"Main advanced","repo":"/home/dev/demo-api","repoLabel":"demo-api","state":"attention","runs":["20261123-100003-stale"],"focus":"待合入","createdAt":"2026-11-23T01:00:00Z","updatedAt":"2026-11-23T02:00:00Z"}"#,
        r#"{"id":"W104","title":"Premerge conflict","repo":"/home/dev/demo-api","repoLabel":"demo-api","state":"attention","runs":["20261123-100004-conf"],"focus":"待合入","createdAt":"2026-11-23T01:00:00Z","updatedAt":"2026-11-23T02:00:00Z"}"#,
    ]
    .join("\n");
    let work = parse_work_jsonl(&raw_work);

    let snap = AppSnapshot {
        repos: vec![repo],
        work,
        refreshed_at: "12:00:00".into(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("."), 5);

    // 1. Runs tab ('3'): run rows show premerge tags AND detail pane shows full premerge line + log
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let runs_screen = render_to_string(&mut app, 190, 30);
    assert!(runs_screen.contains("快进✓ abcdef1") || runs_screen.contains("快进✓ abcd"), "{runs_screen}");
    assert!(runs_screen.contains("预合并中"), "{runs_screen}");
    assert!(runs_screen.contains("快进✗ 主干已前进") || runs_screen.contains("快进✗ 主干"), "{runs_screen}");
    assert!(runs_screen.contains("预合并冲突:src/lib.rs") || runs_screen.contains("预合并冲突：src/lib.rs"), "{runs_screen}");
    assert!(runs_screen.contains("预合并: 预合并冲突：src/lib.rs"), "{runs_screen}");
    assert!(runs_screen.contains("/tmp/run/premerge.log"), "{runs_screen}");

    // 2. Work tab ('2'): shows live premerge status for every linked work item
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    let work_screen = render_to_string(&mut app, 180, 28);
    assert!(work_screen.contains("快进 ✓（基于 abcdef1）"), "{work_screen}");
    assert!(work_screen.contains("预合并中"), "{work_screen}");
    assert!(work_screen.contains("快进 ✗ 主干已前进"), "{work_screen}");
    assert!(work_screen.contains("预合并冲突：src/lib.rs"), "{work_screen}");
}

#[test]
fn snapshot_accounts_tab_active_marker_and_inplace_confirm_modal() {
    use delegate_tui::app::{AppAction, ViewTab};
    use delegate_tui::data::parse_agy_accounts_json;

    let raw_accounts = r#"{
        "root": "/tmp/fake-home/.config/delegate/agy-accounts",
        "active": "alice",
        "liveEmail": "alice@example.test",
        "liveDrifted": false,
        "runningAgy": [
            {"pid": 4242, "run": "20261124-030000-demo", "cmdline": "agy --model gemini-4-neon"}
        ],
        "accounts": [
            {
                "id": "alice",
                "email": "alice@example.test",
                "label": "primary",
                "active": true,
                "addedAt": "2026-11-23T01:00:00Z",
                "lastUsedAt": "2026-11-24T02:30:00Z",
                "lastSyncedAt": "2026-11-24T02:30:00Z",
                "tokenExpiresAt": "2026-11-24T03:30:00Z",
                "files": ["oauth_creds.json", "google_accounts.json", "jetski-standalone-oauth-token"],
                "fileCount": 3,
                "source": "import-agy-swap"
            },
            {
                "id": "bob",
                "email": "bob@example.test",
                "label": "backup",
                "active": false,
                "addedAt": "2026-11-23T02:00:00Z",
                "lastUsedAt": "2026-11-23T18:15:00Z",
                "lastSyncedAt": "2026-11-23T18:15:00Z",
                "tokenExpiresAt": "",
                "files": ["oauth_creds.json", "google_accounts.json"],
                "fileCount": 2,
                "source": "login"
            }
        ]
    }"#;
    let agy_accounts = parse_agy_accounts_json(raw_accounts);
    assert_eq!(agy_accounts.accounts.len(), 2);
    assert_eq!(agy_accounts.running_agy.len(), 1);

    let snap = AppSnapshot {
        agy_accounts,
        refreshed_at: "03:40:00".into(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("."), 5);

    // Press '4' to open Quota tab (merged Accounts + Analytics)
    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Quota);
    let screen = render_to_string(&mut app, 140, 34);
    assert!(screen.contains("4 额度"), "{screen}");
    assert!(screen.contains("agy 账号槽位 (2"), "{screen}");
    assert!(screen.contains("★ 活跃"), "{screen}");
    // Default masked display: 3-char prefix + `…`, no email column, one-line proc without cmdline
    assert!(screen.contains("ali…"), "{screen}");
    assert!(screen.contains("bob"), "{screen}");
    assert!(!screen.contains("alice@example.test"), "email hidden by default:\n{screen}");
    assert!(!screen.contains("bob@example.test"), "email hidden by default:\n{screen}");
    assert!(screen.contains("在跑 agy 进程 (1)"), "{screen}");
    assert!(screen.contains("4242 · demo"), "{screen}");
    assert!(!screen.contains("pid=4242"), "old pid= format should be replaced:\n{screen}");
    assert!(screen.contains("i 显隐全称") || screen.contains("i 全称"), "bottom bar should advertise `i`:\n{screen}");

    // Move selection down to 'bob': Enter MUST NOT open switch modal (advisory-only design; only 'u' opens it)
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(app.selected_account_item().map(|a| a.id.as_str()), Some("bob"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.overlay, Overlay::None, "Enter on 4 额度 must NOT trigger account switch modal");

    // Press 'u' to open in-place confirmation modal
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
    let modal_screen = render_to_string(&mut app, 130, 30);
    assert!(modal_screen.contains("确认切换 agy 账号"), "{modal_screen}");
    assert!(modal_screen.contains("bob (bob@example.test)"), "{modal_screen}");
    assert!(modal_screen.contains("1 个在跑 agy 进程"), "{modal_screen}");
    assert!(modal_screen.contains("继续用原账号"), "{modal_screen}");
    assert!(modal_screen.contains("Enter/y 确认切换"), "{modal_screen}");

    // Cancel with 'n'
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.overlay, Overlay::None);

    // Open with 'u' and confirm with 'y' -> emits SwitchAgyAccount("bob")
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
    let act = app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(act, AppAction::SwitchAgyAccount("bob".to_string()));
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.switching_account.as_deref(), Some("bob"));
}

#[test]
fn snapshot_accounts_tab_shows_5h_7d_usage_and_manual_quota_estimates() {
    use delegate_tui::app::ViewTab;
    use delegate_tui::data::parse_agy_accounts_json;

    let raw_accounts = r#"{
        "root": "/tmp/fake-home/.config/delegate/agy-accounts",
        "active": "work",
        "liveEmail": "work@example.test",
        "liveDrifted": false,
        "runningAgy": [],
        "accounts": [
            {
                "id": "work",
                "email": "work@example.test",
                "label": "",
                "active": true,
                "addedAt": "2026-11-20T01:00:00Z",
                "lastUsedAt": "2026-11-24T03:50:00Z",
                "lastSyncedAt": "2026-11-24T03:50:00Z",
                "tokenExpiresAt": "",
                "files": ["oauth_creds.json"],
                "fileCount": 1,
                "source": "login",
                "usage": {
                    "fiveHour": {
                        "input": 500000,
                        "output": 100000,
                        "cacheRead": 14400000,
                        "total": 15000000,
                        "author": {"total": 12000000},
                        "reviewer": {"total": 3000000},
                        "rounds": 4,
                        "runs": 2,
                        "liveCounted": 0,
                        "uncountedRunning": 1
                    },
                    "sevenDay": {
                        "input": 2000000,
                        "output": 500000,
                        "cacheRead": 47500000,
                        "total": 50000000,
                        "author": {"total": 40000000},
                        "reviewer": {"total": 10000000},
                        "rounds": 12,
                        "runs": 6,
                        "liveCounted": 0,
                        "uncountedRunning": 1
                    }
                },
                "quotaNote": {
                    "weekly": 95.41,
                    "weeklyAt": "2026-11-24T03:40:00Z",
                    "fiveHour": 85.10,
                    "fiveHourAt": "2026-11-24T03:40:00Z"
                },
                "quotaEstimate": {
                    "weekly": {
                        "remainingPct": 95.41,
                        "tokensPerPct": 10893246,
                        "estimatedRemainingTokens": 1039324600,
                        "estimatedRemainingHours": 346.4,
                        "burnRatePerHour": 3000000,
                        "method": "window"
                    },
                    "fiveHour": {
                        "remainingPct": 85.10,
                        "tokensPerPct": 1006711,
                        "estimatedRemainingTokens": 85671140,
                        "estimatedRemainingHours": 28.6,
                        "burnRatePerHour": 3000000,
                        "method": "window"
                    }
                }
            }
        ]
    }"#;
    let agy_accounts = parse_agy_accounts_json(raw_accounts);
    let snap = AppSnapshot {
        agy_accounts,
        refreshed_at: "04:00:00".into(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("."), 5);
    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Quota);

    let screen = render_to_string(&mut app, 150, 38);
    assert!(screen.contains("周剩") && screen.contains("5h剩"), "{screen}");
    assert!(screen.contains("15.0M"), "{screen}");
    assert!(screen.contains("近 5 小时: 输入 500.0K"), "{screen}");
    assert!(screen.contains("运行中未计 1"), "{screen}");
    assert!(screen.contains("近 7 天: 输入 2.0M"), "{screen}");
    assert!(screen.contains("本周剩余 95.41%"), "{screen}");
    assert!(screen.contains("5 小时剩余 85.1%"), "{screen}");
    assert!(screen.contains("按当前用量，5 小时额度约 1 天 4 小时后用完"), "{screen}");
    assert!(screen.contains("按当前用量，本周额度约 14 天 10 小时后用完"), "{screen}");
    assert!(!screen.contains("不自动换号") || screen.contains("只提醒不自动换号"), "{screen}");
    assert!(!screen.contains("不调外部额度接口"), "{screen}");
    assert!(!screen.contains("每 1%"), "{screen}");
    assert!(!screen.contains("推算"), "{screen}");
}

fn make_rich_quota_and_charts_app() -> App {
    use delegate_tui::data::parse_snapshot_json;

    let raw = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-11-24T10:50:00Z",
        "seq": 88,
        "repos": [
            {
                "path": "/home/dev/demo-api",
                "runsDir": "/home/dev/demo-api/.local/run/delegate",
                "runs": [
                    {
                        "run": "20261124-102000-auth-fix",
                        "name": "鉴权修复",
                        "state": "delivered",
                        "mode": "write",
                        "modelName": "neon",
                        "elapsedSeconds": 420,
                        "ageSeconds": 600
                    },
                    {
                        "run": "20261124-104000-worker",
                        "name": "并发队列优化",
                        "state": "running",
                        "mode": "write",
                        "modelName": "neon",
                        "elapsedSeconds": 240,
                        "ageSeconds": 240
                    }
                ],
                "cleanedNotes": []
            }
        ],
        "events": [],
        "lane": {
            "maxHeavy": 6,
            "heavyJobs": 94,
            "threads": 96,
            "budget": 94,
            "pool": {"target": 88, "total": 88, "free": 56, "borrowed": 32, "leases": [{"rank": 1, "used": 32, "share": 88}]},
            "local": [{"label": "demo-api: cargo test", "since": "2026-11-24T10:45:00Z", "running": true, "rank": 1, "used": 32, "share": 88}],
            "remote": []
        },
        "dagQueue": [],
        "merges": [],
        "candidates": [],
        "agyAccounts": {
            "root": "/home/dev/.config/delegate/agy-accounts",
            "active": "alpha",
            "liveEmail": "alpha@example.test",
            "liveDrifted": false,
            "quotaUpdatedAt": "2026-11-24T10:47:00Z",
            "quotaAgeSeconds": 180,
            "runningAgy": [
                {"pid": 9102, "run": "20261124-104000-worker", "cmdline": "agy --model gemini-4-neon"}
            ],
            "accounts": [
                {
                    "id": "alpha",
                    "email": "alpha@example.test",
                    "label": "主力",
                    "active": true,
                    "addedAt": "2026-11-18T01:00:00Z",
                    "lastUsedAt": "2026-11-24T10:40:00Z",
                    "lastSyncedAt": "2026-11-24T10:47:00Z",
                    "tokenExpiresAt": "2026-11-24T11:40:00Z",
                    "files": ["oauth_creds.json", "google_accounts.json", "jetski-standalone-oauth-token"],
                    "fileCount": 3,
                    "source": "login",
                    "quota": {
                        "fetchedAt": "2026-11-24T10:47:00Z",
                        "fetchedEpoch": 1795488420,
                        "ageSeconds": 180,
                        "ok": true,
                        "entries": [
                            {
                                "pool": "Gemini Models",
                                "window": "fiveHour",
                                "windowLabel": "Five Hour Limit Remaining",
                                "remainingPct": 82.0,
                                "resetAt": "2026-11-24T14:50:00Z",
                                "resetInSeconds": 14400
                            },
                            {
                                "pool": "Gemini Models",
                                "window": "weekly",
                                "windowLabel": "Weekly Limit Remaining",
                                "remainingPct": 91.0,
                                "resetAt": "2026-11-30T20:09:03Z",
                                "resetInSeconds": 551943
                            },
                            {
                                "pool": "Claude and GPT models",
                                "window": "fiveHour",
                                "windowLabel": "Five Hour Limit Remaining",
                                "remainingPct": 44.0,
                                "resetAt": "2026-11-24T13:20:00Z",
                                "resetInSeconds": 9000
                            },
                            {
                                "pool": "Claude and GPT models",
                                "window": "weekly",
                                "windowLabel": "Weekly Limit Remaining",
                                "remainingPct": 76.0,
                                "resetAt": "2026-11-30T20:09:03Z",
                                "resetInSeconds": 551943
                            }
                        ],
                        "unknownLines": ["Preview Tier: experimental pool active"]
                    },
                    "usage": {
                        "oneHour": {"input": 300000, "output": 80000, "cacheRead": 2120000, "total": 2500000, "author": {"total": 2000000}, "reviewer": {"total": 500000}, "rounds": 3, "runs": 2},
                        "fiveHour": {"input": 1200000, "output": 300000, "cacheRead": 10500000, "total": 12000000, "author": {"total": 9600000}, "reviewer": {"total": 2400000}, "rounds": 9, "runs": 5},
                        "sevenDay": {"input": 4800000, "output": 1200000, "cacheRead": 48000000, "total": 54000000, "author": {"total": 42000000}, "reviewer": {"total": 12000000}, "rounds": 36, "runs": 18},
                        "hourly24h": [0, 0, 400000, 900000, 1200000, 800000, 300000, 0, 200000, 600000, 1500000, 2200000, 3100000, 2800000, 1900000, 1400000, 2600000, 3800000, 2900000, 1800000, 2100000, 2400000, 3200000, 2500000]
                    },
                    "quotaNote": {
                        "weekly": 95.0,
                        "weeklyAt": "2026-11-23T08:00:00Z",
                        "fiveHour": 90.0,
                        "fiveHourAt": "2026-11-23T08:00:00Z"
                    },
                    "quotaEstimate": {
                        "fiveHour": {
                            "remainingPct": 44.0,
                            "tokensPerPct": 666667,
                            "estimatedRemainingTokens": 29333348,
                            "estimatedRemainingHours": 11.73,
                            "burnRatePerHour": 2500000,
                            "method": "delta",
                            "source": "real",
                            "burnRateSource": "5h",
                            "pool": "Claude and GPT models"
                        },
                        "weekly": {
                            "remainingPct": 91.0,
                            "tokensPerPct": 6000000,
                            "estimatedRemainingTokens": 546000000,
                            "estimatedRemainingHours": 218.4,
                            "burnRatePerHour": 2500000,
                            "method": "window",
                            "source": "real",
                            "burnRateSource": "24h",
                            "pool": "Gemini Models"
                        }
                    }
                },
                {
                    "id": "beta",
                    "email": "beta@example.test",
                    "label": "备用",
                    "active": false,
                    "addedAt": "2026-11-19T02:00:00Z",
                    "lastUsedAt": "2026-11-23T19:20:00Z",
                    "lastSyncedAt": "2026-11-24T10:47:00Z",
                    "tokenExpiresAt": "2026-11-24T12:00:00Z",
                    "files": ["oauth_creds.json", "google_accounts.json"],
                    "fileCount": 2,
                    "source": "import-agy-swap",
                    "quota": {
                        "fetchedAt": "2026-11-24T10:47:00Z",
                        "fetchedEpoch": 1795488420,
                        "ageSeconds": 180,
                        "ok": true,
                        "entries": [
                            {
                                "pool": "Gemini Models",
                                "window": "fiveHour",
                                "windowLabel": "Five Hour Limit Remaining",
                                "remainingPct": 100.0,
                                "resetAt": "2026-11-24T15:47:00Z",
                                "resetInSeconds": 18000
                            },
                            {
                                "pool": "Gemini Models",
                                "window": "weekly",
                                "windowLabel": "Weekly Limit Remaining",
                                "remainingPct": 98.0,
                                "resetAt": "2026-12-01T00:00:00Z",
                                "resetInSeconds": 565800
                            }
                        ],
                        "unknownLines": []
                    },
                    "usage": {
                        "oneHour": {"input": 0, "output": 0, "cacheRead": 0, "total": 0, "author": {"total": 0}, "reviewer": {"total": 0}},
                        "fiveHour": {"input": 100000, "output": 20000, "cacheRead": 680000, "total": 800000, "author": {"total": 800000}, "reviewer": {"total": 0}},
                        "sevenDay": {"input": 600000, "output": 150000, "cacheRead": 5450000, "total": 6200000, "author": {"total": 5000000}, "reviewer": {"total": 1200000}},
                        "hourly24h": [0, 0, 0, 0, 500000, 900000, 600000, 0, 0, 0, 0, 0, 0, 400000, 1200000, 0, 0, 0, 0, 0, 500000, 300000, 0, 0]
                    }
                }
            ]
        },
        "analytics": {
            "hourly24h": {
                "total": [0, 0, 400000, 900000, 1700000, 1700000, 900000, 0, 200000, 600000, 1500000, 2200000, 3100000, 3200000, 3100000, 1400000, 2600000, 3800000, 2900000, 1800000, 2600000, 2700000, 3200000, 2500000],
                "byAccount": {
                    "alpha": [0, 0, 400000, 900000, 1200000, 800000, 300000, 0, 200000, 600000, 1500000, 2200000, 3100000, 2800000, 1900000, 1400000, 2600000, 3800000, 2900000, 1800000, 2100000, 2400000, 3200000, 2500000],
                    "beta": [0, 0, 0, 0, 500000, 900000, 600000, 0, 0, 0, 0, 0, 0, 400000, 1200000, 0, 0, 0, 0, 0, 500000, 300000, 0, 0]
                }
            },
            "byPurpose": {
                "write": 36500000,
                "review": 13200000,
                "rework": 7800000,
                "readOnly": 2700000
            },
            "byModel": [
                {"model": "gemini-4-neon", "tokens": 41200000},
                {"model": "claude-sonnet-4", "tokens": 14600000},
                {"model": "sol", "tokens": 4400000}
            ],
            "daily7d": [
                {"ymd": "2026-11-18", "date": "11-18", "mergedCount": 3, "medianMergeSecs": 1320, "avgMergeSecs": 1400, "totalChains": 4, "firstRoundPass": 3, "firstRoundPassPct": 75},
                {"ymd": "2026-11-19", "date": "11-19", "mergedCount": 4, "medianMergeSecs": 1140, "avgMergeSecs": 1200, "totalChains": 5, "firstRoundPass": 4, "firstRoundPassPct": 80},
                {"ymd": "2026-11-20", "date": "11-20", "mergedCount": 3, "medianMergeSecs": 900, "avgMergeSecs": 960, "totalChains": 3, "firstRoundPass": 3, "firstRoundPassPct": 100},
                {"ymd": "2026-11-21", "date": "11-21", "mergedCount": 2, "medianMergeSecs": 1500, "avgMergeSecs": 1560, "totalChains": 3, "firstRoundPass": 2, "firstRoundPassPct": 67},
                {"ymd": "2026-11-22", "date": "11-22", "mergedCount": 4, "medianMergeSecs": 960, "avgMergeSecs": 1020, "totalChains": 4, "firstRoundPass": 3, "firstRoundPassPct": 75},
                {"ymd": "2026-11-23", "date": "11-23", "mergedCount": 5, "medianMergeSecs": 1080, "avgMergeSecs": 1120, "totalChains": 5, "firstRoundPass": 4, "firstRoundPassPct": 80},
                {"ymd": "2026-11-24", "date": "11-24", "mergedCount": 3, "medianMergeSecs": 840, "avgMergeSecs": 900, "totalChains": 3, "firstRoundPass": 3, "firstRoundPassPct": 100}
            ]
        }
    }"#;
    let mut snap = parse_snapshot_json(raw, Some(Path::new("/home/dev")), None).unwrap();
    snap.host_load = Some(HostLoad {
        load1_tenths: 14,
        load5_tenths: 12,
        load15_tenths: 10,
        mem_used_tenths_gib: Some(210),
        mem_total_tenths_gib: Some(640),
    });
    snap.refreshed_at = "10:50:00".to_string();
    App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/demo-api"), 5)
}

fn assert_lines_fit_width(screen: &str, max_width: usize) {
    for (idx, line) in screen.lines().enumerate() {
        let w = display_width(line);
        assert!(
            w <= max_width,
            "line {} exceeded width {} (got {}): {:?}\n--- FULL SCREEN ---\n{}",
            idx + 1,
            max_width,
            w,
            line,
            screen
        );
    }
}

#[test]
fn snapshot_overview_and_accounts_real_quota_charts_120x40_and_80x24() {
    use delegate_tui::app::ViewTab;

    let mut app = make_rich_quota_and_charts_app();

    let forbidden = [
        "仅作提醒",
        "仅提醒",
        "仅提示",
        "不自动换号",
        "不做自动轮换",
        "真实额度",
        "真实额度优先",
        "agy /usage",
        "不调外部额度接口",
        "紧凑文字",
        "就地确认",
        "每1%≈",
        "每 1% ≈",
        "5h推算",
        "周推算",
        "近1h速",
        "[原始]",
    ];

    // 1. Overview 150x44 (wide 5-card overview: attention, running, work chains, quota summary, recent deliveries)
    app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Overview);
    let ov_wide = render_to_string(&mut app, 150, 44);
    assert_lines_fit_width(&ov_wide, 150);
    assert!(ov_wide.contains("1 总览"), "{ov_wide}");
    assert!(ov_wide.contains("额度与消耗"), "{ov_wide}");
    assert!(ov_wide.contains("3 分钟前更新"), "{ov_wide}");
    assert!(ov_wide.contains("Gemini 5h"), "{ov_wide}");
    assert!(ov_wide.contains("Gemini 本周"), "{ov_wide}");
    assert!(ov_wide.contains("82%"), "{ov_wide}");
    assert!(ov_wide.contains("91%"), "{ov_wide}");
    assert!(ov_wide.contains("7d 用途"), "{ov_wide}");
    assert!(ov_wide.contains("7d 走势"), "{ov_wide}");
    assert!(!ov_wide.contains("Preview Tier: experimental pool active"), "unknown line should not show in overview: {ov_wide}");
    for bad in forbidden {
        assert!(!ov_wide.contains(bad), "forbidden copy {bad:?} in ov_wide:\n{ov_wide}");
    }

    // 2. Overview 80x24 (narrow single-column flow, zero misalignment)
    let ov_narrow = render_to_string(&mut app, 80, 24);
    assert_lines_fit_width(&ov_narrow, 80);
    assert!(ov_narrow.contains("1总览"), "{ov_narrow}");
    assert!(ov_narrow.contains("alp…"), "{ov_narrow}");
    assert!(!ov_narrow.contains("alpha"), "account id should be masked by default:\n{ov_narrow}");
    assert!(!ov_narrow.contains("Preview Tier: experimental pool active"), "{ov_narrow}");
    for bad in forbidden {
        assert!(!ov_narrow.contains(bad), "forbidden copy {bad:?} in ov_narrow:\n{ov_narrow}");
    }

    // 3. Quota tab ('4') 150x44 (merged Accounts + Pools + 24h Sparkline + 7d Distribution & Efficiency)
    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Quota);
    let acct_wide = render_to_string(&mut app, 150, 44);
    assert_lines_fit_width(&acct_wide, 150);
    assert!(acct_wide.contains("4 额度"), "{acct_wide}");
    assert!(acct_wide.contains("alp… 额度池与 24h 走势 · 3 分钟前更新"), "{acct_wide}");
    assert!(!acct_wide.contains("alpha@example.test"), "email hidden by default: {acct_wide}");
    assert!(!acct_wide.contains("主力"), "label hidden by default: {acct_wide}");
    assert!(acct_wide.contains("5 小时") && acct_wide.contains("本周"), "{acct_wide}");
    assert!(acct_wide.contains("Preview Tier: experimental pool active"), "{acct_wide}");
    assert!(acct_wide.contains("Claude/GPT 5 小时额度在重置前用不"), "{acct_wide}");
    assert!(acct_wide.contains("Gemini 本周额度在重置前用不完"), "{acct_wide}");
    assert!(acct_wide.contains("手动记录: 本周剩余 95% · 5 小时剩余 90%"), "{acct_wide}");
    assert!(acct_wide.contains("近 24 小时每小时 Token 走势"), "{acct_wide}");
    assert!(acct_wide.contains("在跑 agy 进程 (1)"), "{acct_wide}");
    assert!(acct_wide.contains("近 7 天 Token 分布"), "{acct_wide}");
    assert!(acct_wide.contains("gemini-4-neon"), "{acct_wide}");
    assert!(acct_wide.contains("claude-sonnet-4"), "{acct_wide}");
    assert!(acct_wide.contains("近 7 天交付效率走势"), "{acct_wide}");
    assert!(acct_wide.contains("11-18") && acct_wide.contains("11-24"), "{acct_wide}");
    for bad in forbidden {
        if bad == "不自动换号" {
            continue;
        }
        assert!(!acct_wide.contains(bad), "forbidden copy {bad:?} in acct_wide:\n{acct_wide}");
    }

    // 4. Quota tab 80x30 (narrow stacked layout, zero misalignment)
    let acct_narrow = render_to_string(&mut app, 80, 30);
    assert_lines_fit_width(&acct_narrow, 80);
    assert!(acct_narrow.contains("agy 账号槽位 (2") || acct_narrow.contains("agy 账号 (2"), "{acct_narrow}");
    assert!(acct_narrow.contains("alp… · 3 分钟前更新"), "{acct_narrow}");
    assert!(!acct_narrow.contains("alpha@example.test"), "email hidden by default: {acct_narrow}");
    assert!(acct_narrow.contains("5 小时"), "{acct_narrow}");
    for bad in forbidden {
        if bad == "不自动换号" {
            continue;
        }
        assert!(!acct_narrow.contains(bad), "forbidden copy {bad:?} in acct_narrow:\n{acct_narrow}");
    }

    if std::env::var("PRINT_TUI_SNAPSHOTS").as_deref() == Ok("1") {
        println!("\n=== OVERVIEW 150x44 ===\n{ov_wide}");
        println!("\n=== OVERVIEW 80x24 ===\n{ov_narrow}");
        println!("\n=== QUOTA 150x44 ===\n{acct_wide}");
        println!("\n=== QUOTA 80x30 ===\n{acct_narrow}");
    }
}

#[test]
fn quota_estimate_reports_depletion_when_earlier_than_reset() {
    use delegate_tui::app::ViewTab;

    let mut app = make_rich_quota_and_charts_app();
    // Make fiveHour run out in 90 minutes (before 2h30m reset) and weekly in 3 days (before 6d9h reset).
    if let Some(alpha) = app.snapshot.agy_accounts.accounts.first_mut() {
        if let Some(est) = alpha.quota_estimate.as_mut() {
            if let Some(fh) = est.five_hour.as_mut() {
                fh.estimated_remaining_minutes = Some(90);
            }
            if let Some(wk) = est.weekly.as_mut() {
                wk.estimated_remaining_minutes = Some(3 * 24 * 60);
            }
        }
    }

    // Overview keeps the quota card depletion note + `4 额度(!)` tab badge, but does NOT show the top alert strip.
    app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Overview);
    let ov = render_to_string(&mut app, 150, 40);
    assert_lines_fit_width(&ov, 150);
    assert!(
        !ov.contains("▲ 告警"),
        "depletion before reset must not trigger top alert strip:\n{ov}"
    );
    assert!(ov.contains("4 额度(!)"), "tab badge should stay:\n{ov}");
    assert!(ov.contains("⚠ 约 3d0h") && ov.contains("⚠ 约 1h3"), "{ov}");

    // Quota tab ('4') shows both windows clearly without truncation, and no top alert strip.
    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    let acct = render_to_string(&mut app, 150, 44);
    assert_lines_fit_width(&acct, 150);
    assert!(
        !acct.contains("▲ 告警"),
        "quota tab must not show top alert strip for depletion:\n{acct}"
    );
    assert!(acct.contains("按最近 5 小时的平均用量，Claude/GPT 5 小时额度约 1 小时 30"), "{acct}");
    assert!(acct.contains("分钟后用完"), "{acct}");
    assert!(
        acct.contains("按最近 24 小时的平均用量，Gemini 本周额度约 3 天后用完"),
        "{acct}"
    );
}

#[test]
fn work_view_renders_stale_badges_colors_missing_brief_and_top_bar_count() {
    use delegate_tui::app::ViewTab;
    use delegate_tui::data::parse_snapshot_json;
    use ratatui::style::Color;

    // generatedAt = 2026-11-24T12:00:00Z
    let raw = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-11-24T12:00:00Z",
        "seq": 10,
        "repos": [],
        "work": [
            {
                "id": "Q01",
                "title": "所有者项不老化不标缺说明",
                "repo": "/repo/demo",
                "state": "attention",
                "owner": {"kind": "nod", "ask": "所有者项不老化"},
                "stateSince": "2026-11-22T00:00:00Z"
            },
            {
                "id": "W001",
                "title": "一倍老化待闭环",
                "repo": "/repo/demo",
                "state": "verify",
                "doneWhen": "验证",
                "stateSince": "2026-11-24T07:00:00Z",
                "focus": "待闭环"
            },
            {
                "id": "W002",
                "title": "两倍老化待闭环",
                "repo": "/repo/demo",
                "state": "verify",
                "doneWhen": "验证",
                "stateSince": "2026-11-24T03:00:00Z",
                "focus": "待闭环"
            },
            {
                "id": "W003",
                "title": "一倍老化就绪",
                "repo": "/repo/demo",
                "state": "ready",
                "brief": "docs/w3.md",
                "stateSince": "2026-11-24T02:00:00Z",
                "focus": "可派发 10h"
            },
            {
                "id": "W004",
                "title": "两倍老化缺说明就绪",
                "repo": "/repo/demo",
                "state": "ready",
                "stateSince": "2026-11-23T18:00:00Z",
                "focus": "可派发"
            },
            {
                "id": "W005",
                "title": "新鲜缺说明就绪",
                "repo": "/repo/demo",
                "state": "ready",
                "stateSince": "2026-11-24T11:30:00Z",
                "focus": "可派发"
            }
        ],
        "events": [],
        "lane": {},
        "dagQueue": [],
        "merges": [],
        "candidates": []
    }"#;

    let snap = parse_snapshot_json(raw, None, None).unwrap();
    let counts = snap.work_counts();
    assert_eq!(counts.stale, 4, "W001/W002/W003/W004 should be stale, not Q01 or W005");

    let mut app = App::new(
        snap,
        PathBuf::from("delegate"),
        PathBuf::from("/repo/demo"),
        2,
    );
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Work);

    // Move selection away from W001..W005 (keep on Q01 at top) so fg colors are not reversed.
    // Use 160 cols (54:46 dual pane) so left pane shows `老化 4` + all stale badges cleanly.
    let backend = TestBackend::new(160, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer().clone();

    let mut rows_text = Vec::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        let mut x = 0u16;
        while x < buf.area.width {
            let sym = buf[(x, y)].symbol();
            line.push_str(sym);
            let w = display_width(sym).max(1) as u16;
            x = x.saturating_add(w);
        }
        rows_text.push(line);
    }
    let screen = rows_text.join("\n");

    // Work pane title shows 老化 4
    assert!(screen.contains("老化 4"), "work pane missing 老化 4:\n{screen}");

    // Check row tail markers and colors (scan left pane area only so we hit the tail badge, not the right detail card)
    let left_limit = (buf.area.width as usize * 54) / 100;
    let find_cell_fg = |row_needle: &str, token_first_char: &str| -> Option<Color> {
        for y in 0..buf.area.height {
            if !rows_text[y as usize].contains(row_needle) {
                continue;
            }
            for x in (0..left_limit.min(buf.area.width as usize)).rev() {
                if buf[(x as u16, y)].symbol() == token_first_char {
                    return Some(buf[(x as u16, y)].fg);
                }
            }
        }
        None
    };

    assert!(screen.contains("W001") && screen.contains("停 5h"), "{screen}");
    assert!(screen.contains("W002") && screen.contains("停 9h"), "{screen}");
    assert!(screen.contains("W003") && screen.contains("停 10h"), "{screen}");
    let w003_line = rows_text.iter().find(|l| l.contains("W003")).unwrap();
    assert_eq!(w003_line.matches("10h").count(), 1, "duration must not appear twice: {w003_line}");
    assert!(screen.contains("W004") && screen.contains("停 18h") && screen.contains("缺说明"), "{screen}");
    assert!(screen.contains("W005") && screen.contains("缺说明"), "{screen}");
    // Q01 row must not have 停 or 缺说明
    let q01_line = rows_text.iter().find(|l| l.contains("Q01")).unwrap();
    assert!(!q01_line.contains("停 ") && !q01_line.contains("缺说明"), "{q01_line}");

    // 1x threshold -> Yellow (STATUS_PENDING), 2x threshold -> Red (STATUS_FAILED)
    assert_eq!(find_cell_fg("W001", "停"), Some(Color::Yellow));
    assert_eq!(find_cell_fg("W002", "停"), Some(Color::Red));
    assert_eq!(find_cell_fg("W003", "停"), Some(Color::Yellow));
    assert_eq!(find_cell_fg("W004", "停"), Some(Color::Red));
    assert_eq!(find_cell_fg("W005", "缺"), Some(Color::Yellow));

    // Compact layout (<70 cols) also shows 老化 4, 停 Nh, 缺说明 without overflow
    let compact = render_to_string(&mut app, 60, 30);
    assert_lines_fit_width(&compact, 60);
    assert!(compact.contains("老化 4"), "{compact}");
    assert!(compact.contains("停 5h"), "{compact}");
    assert!(compact.contains("停 9h"), "{compact}");
    assert!(compact.contains("缺说明"), "{compact}");

    // When no stale items exist, Work pane does NOT show 老化
    let fresh_raw = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-11-24T12:00:00Z",
        "seq": 11,
        "repos": [],
        "work": [
            {
                "id": "W010",
                "title": "新鲜带说明",
                "repo": "/repo/demo",
                "state": "ready",
                "brief": "docs/w10.md",
                "stateSince": "2026-11-24T11:00:00Z",
                "focus": "可派发"
            }
        ],
        "events": [],
        "lane": {},
        "dagQueue": [],
        "merges": [],
        "candidates": []
    }"#;
    let fresh_snap = parse_snapshot_json(fresh_raw, None, None).unwrap();
    let mut fresh_app = App::new(
        fresh_snap,
        PathBuf::from("delegate"),
        PathBuf::from("/repo/demo"),
        2,
    );
    fresh_app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    let fresh_screen = render_to_string(&mut fresh_app, 120, 20);
    assert!(!fresh_screen.contains("老化"), "{fresh_screen}");
    assert!(!fresh_screen.contains("缺说明"), "{fresh_screen}");
}

#[test]
fn accounts_default_masked_and_toggle_full_with_i_while_y_copies_full_id() {
    use delegate_tui::app::{AppAction, ViewTab};
    use delegate_tui::data::parse_agy_accounts_json;

    let raw_accounts = r#"{
        "root": "/tmp/fake-home/.config/delegate/agy-accounts",
        "active": "guimondbuchinsky",
        "liveEmail": "guimondbuchinsky@gmail.com",
        "liveDrifted": false,
        "runningAgy": [
            {"pid": 33057, "run": "20261124-155318-call-orphan", "cmdline": "/home/user/.local/libexec/agy --input-format stream-json --output-format stream-json"},
            {"pid": 2896311, "cmdline": "/bin/sh /home/user/.local/bin/agy --agent aia-lead"}
        ],
        "accounts": [
            {
                "id": "guimondbuchinsky",
                "email": "guimondbuchinsky@gmail.com",
                "label": "Buchinsky Guimond",
                "active": true,
                "lastUsedAt": "2026-11-24T00:08:00Z",
                "fileCount": 6,
                "usage": {
                    "oneHour": {"total": 56500000},
                    "fiveHour": {"total": 496400000},
                    "sevenDay": {"total": 557000000},
                    "hourly24h": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,10000000,20000000,30000000,40000000,50000000,60000000,50000000,56500000]
                },
                "quota": {
                    "fetchedAt": "2026-11-24T08:00:00Z",
                    "ageSeconds": 60,
                    "ok": true,
                    "entries": [
                        {"pool": "Gemini", "window": "weekly", "windowLabel": "Weekly Limit Remaining", "remainingPct": 87.0, "resetInSeconds": 558000},
                        {"pool": "Claude/GPT", "window": "fiveHour", "windowLabel": "5-Hour Limit Remaining", "remainingPct": 100.0, "resetInSeconds": 15300}
                    ]
                }
            },
            {
                "id": "jzy896370029",
                "email": "jzy896370029@gmail.com",
                "label": "",
                "active": false,
                "lastUsedAt": "",
                "fileCount": 4,
                "usage": {"hourly24h": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}
            }
        ]
    }"#;

    let agy_accounts = parse_agy_accounts_json(raw_accounts);
    let snap = AppSnapshot {
        agy_accounts,
        refreshed_at: "16:17:40".into(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("."), 5);

    // Overview ('1'): accounts masked (`gui…`, `jzy…`), no full id / label leaked
    app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Overview);
    let ov = render_to_string(&mut app, 140, 40);
    assert!(ov.contains("gui…"), "{ov}");
    assert!(ov.contains("jzy…"), "{ov}");
    assert!(!ov.contains("guimondbuchinsky"), "full account name leaked on overview:\n{ov}");
    assert!(!ov.contains("jzy896370029"), "full account name leaked on overview:\n{ov}");
    assert!(!ov.contains("Buchinsky Guimond"), "label leaked on overview:\n{ov}");

    // Quota tab ('4'): default masked, no email column, no label, one-line procs without cmdline
    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Quota);
    let masked = render_to_string(&mut app, 140, 40);
    assert!(masked.contains("gui…"), "{masked}");
    assert!(masked.contains("jzy…"), "{masked}");
    assert!(!masked.contains("guimondbuchinsky"), "full id leaked:\n{masked}");
    assert!(!masked.contains("gmail.com"), "email leaked:\n{masked}");
    assert!(!masked.contains("Buchinsky Guimond"), "label leaked:\n{masked}");
    assert!(!masked.contains("邮箱"), "email column header should be removed:\n{masked}");
    assert!(masked.contains("i 显隐全称") || masked.contains("i 全称"), "footer should show `i`:\n{masked}");
    assert!(masked.contains("33057 · call-orphan"), "{masked}");
    assert!(masked.contains("2896311 · aia-lead"), "{masked}");
    assert!(!masked.contains("--input-format"), "cmdline should not be printed:\n{masked}");

    // `y` still copies the full account id even while masked
    let y_act = app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(
        y_act,
        AppAction::CopyClipboard("delegate agy account use guimondbuchinsky".to_string())
    );

    // Press `i` -> selected account reveals full id, email, and label; other account stays masked
    app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
    let revealed = render_to_string(&mut app, 140, 40);
    assert!(revealed.contains("guimondbuchinsky"), "{revealed}");
    assert!(revealed.contains("guimondbuchinsky@gmail.com"), "{revealed}");
    assert!(revealed.contains("Buchinsky Guimond"), "{revealed}");
    assert!(revealed.contains("jzy…"), "{revealed}");
    assert!(!revealed.contains("jzy896370029"), "unselected account must stay masked:\n{revealed}");

    // Press `i` again -> restores masked default
    app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
    let remasked = render_to_string(&mut app, 140, 40);
    assert!(!remasked.contains("guimondbuchinsky"), "second `i` should hide full id:\n{remasked}");
    assert!(!remasked.contains("gmail.com"), "second `i` should hide email:\n{remasked}");
    assert!(!remasked.contains("Buchinsky Guimond"), "second `i` should hide label:\n{remasked}");
}

#[test]
fn quota_row_has_space_between_bar_and_text_and_never_overflows_at_80_and_120_cols() {
    use delegate_tui::app::ViewTab;
    use delegate_tui::data::parse_agy_accounts_json;

    let raw_accounts = r#"{
        "root": "/tmp/fake-home/.config/delegate/agy-accounts",
        "active": "guimondbuchinsky",
        "liveEmail": "guimondbuchinsky@gmail.com",
        "liveDrifted": false,
        "runningAgy": [],
        "accounts": [
            {
                "id": "guimondbuchinsky",
                "email": "guimondbuchinsky@gmail.com",
                "active": true,
                "fileCount": 6,
                "quota": {
                    "fetchedAt": "2026-11-24T08:00:00Z",
                    "ageSeconds": 60,
                    "ok": true,
                    "entries": [
                        {"pool": "Gemini", "window": "weekly", "windowLabel": "Weekly Limit Remaining", "remainingPct": 87.0, "resetInSeconds": 558000},
                        {"pool": "Gemini", "window": "fiveHour", "windowLabel": "5-Hour Limit Remaining", "remainingPct": 87.0, "resetInSeconds": 10260},
                        {"pool": "Claude/GPT", "window": "weekly", "windowLabel": "Weekly Limit Remaining", "remainingPct": 90.0, "resetInSeconds": 237600},
                        {"pool": "Claude/GPT", "window": "fiveHour", "windowLabel": "5-Hour Limit Remaining", "remainingPct": 100.0, "resetInSeconds": 15300}
                    ]
                }
            }
        ]
    }"#;

    let agy_accounts = parse_agy_accounts_json(raw_accounts);
    let snap = AppSnapshot {
        agy_accounts,
        refreshed_at: "16:17:40".into(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("."), 5);

    // 140x40 Overview ('1') & Quota ('4'): bar has 1 space before `剩余`/pct, and nothing trails after `重置`
    for tab_key in ['1', '4'] {
        app.handle_key(KeyEvent::new(KeyCode::Char(tab_key), KeyModifiers::NONE));
        let screen = render_to_string(&mut app, 140, 40);
        assert_lines_fit_width(&screen, 140);
        assert!(
            !screen.contains("█剩余") && !screen.contains("░剩余"),
            "bar must be separated from text by a space:\n{screen}"
        );
        for line in screen.lines() {
            if let Some(pos) = line.find("后重置") {
                let after = &line[pos + "后重置".len()..];
                let before_border = after.split('│').next().unwrap_or(after);
                assert!(
                    !before_border.chars().any(|c| ('▏'..='█').contains(&c) || c == '░'),
                    "bar overflowed after text on line: {line}"
                );
            }
        }
    }

    // 80x24 Overview & Quota: fits cleanly within 80 cols with no overflow
    app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Overview);
    let ov80 = render_to_string(&mut app, 80, 24);
    assert_lines_fit_width(&ov80, 80);

    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Quota);
    let acct80 = render_to_string(&mut app, 80, 24);
    assert_lines_fit_width(&acct80, 80);
    assert!(!acct80.contains("█剩余"), "{acct80}");
}

#[test]
fn runs_time_uses_finished_at_and_model_shows_alias_dot_thinking() {
    use delegate_tui::data::parse_snapshot_json;

    let raw = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-11-24T08:17:00Z",
        "repos": [{
            "path": "/repo/demo",
            "runs": [
                {
                    "run": "20261124-114742-work-hygiene",
                    "name": "work-hygiene",
                    "state": "answered",
                    "mode": "read-only",
                    "modelName": "neon-med",
                    "ageSeconds": 144,
                    "elapsedSeconds": 142,
                    "finishedAt": "2026-11-24T03:50:06Z"
                },
                {
                    "run": "20261124-150000-hydro-check",
                    "name": "hydro-check",
                    "state": "answered",
                    "mode": "read-only",
                    "modelName": "hydrogen",
                    "thinking": "high",
                    "ageSeconds": 60,
                    "elapsedSeconds": 58,
                    "finishedAt": "2026-11-24T06:17:00Z"
                }
            ],
            "cleanedNotes": []
        }],
        "work": [],
        "events": [],
        "lane": {},
        "dagQueue": [],
        "merges": [],
        "candidates": []
    }"#;

    let snap = parse_snapshot_json(raw, None, None).unwrap();
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/repo/demo"), 5);
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let screen = render_to_string(&mut app, 140, 30);

    // finishedAt is 4h27m and 2h ago — must NOT show `刚刚` from stale ageSeconds
    assert!(!screen.contains("刚刚"), "stale ageSeconds caused 刚刚:\n{screen}");
    assert!(screen.contains("4时前") || screen.contains("4 小时前"), "{screen}");
    assert!(screen.contains("2时前") || screen.contains("2 小时前"), "{screen}");

    // Model column shows `neon·中` and `hyd…·高` (truncating only alias, preserving `·高`)
    assert!(screen.contains("neon·中"), "{screen}");
    assert!(screen.contains("hyd…·高"), "{screen}");
}

#[test]
fn work_owner_missing_advice_tag_and_waiting_work_section_on_q_card() {
    use delegate_tui::data::parse_snapshot_json;

    let raw = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-11-24T12:00:00Z",
        "seq": 12,
        "repos": [],
        "work": [
            {
                "id": "Q01",
                "title": "旧空壳点头项",
                "repo": "/repo/demo",
                "repoLabel": "demo",
                "state": "attention",
                "owner": {"kind": "nod", "ask": "旧空壳点头项"},
                "stateSince": "2026-11-24T10:00:00Z",
                "focus": "等你点头"
            },
            {
                "id": "Q02",
                "title": "路由切换方案",
                "repo": "/repo/demo",
                "repoLabel": "demo",
                "state": "attention",
                "owner": {
                    "kind": "nod",
                    "ask": "是否切到方案 A？",
                    "recommendation": "建议选 A",
                    "ifUndecided": "维持旧路由",
                    "cost": "半天"
                },
                "stateSince": "2026-11-24T10:00:00Z",
                "focus": "等你点头"
            },
            {
                "id": "D001",
                "title": "方向调研",
                "repo": "/repo/demo",
                "repoLabel": "demo",
                "state": "active",
                "owner": {"kind": "direction", "ask": "方向调研"},
                "stateSince": "2026-11-24T10:00:00Z",
                "focus": "方向"
            },
            {
                "id": "W001",
                "title": "落地新路由",
                "repo": "/repo/demo",
                "repoLabel": "demo",
                "state": "blocked",
                "doneWhen": "灰度与单测全绿",
                "brief": "docs/plan/routing.md",
                "waitOn": ["owner:Q02"],
                "stateSince": "2026-11-24T10:05:00Z",
                "focus": "等所有者 Q02"
            }
        ],
        "events": [],
        "lane": {},
        "dagQueue": [],
        "merges": [],
        "candidates": []
    }"#;

    let snap = parse_snapshot_json(raw, None, None).unwrap();
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/repo/demo"), 2);
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));

    let list_screen = render_to_string(&mut app, 150, 24);
    let q01_line = list_screen
        .lines()
        .find(|l| l.contains("Q01") && !l.starts_with('╭'))
        .expect("Q01 row");
    let q02_line = list_screen
        .lines()
        .find(|l| l.contains("Q02") && !l.starts_with('╭'))
        .expect("Q02 row");
    let d001_line = list_screen
        .lines()
        .find(|l| l.contains("D001") && !l.starts_with('╭'))
        .expect("D001 row");
    assert!(q01_line.contains("缺建议"), "incomplete Q01 must show 缺建议: {q01_line}");
    assert!(!q02_line.contains("缺建议"), "complete Q02 must not show 缺建议: {q02_line}");
    assert!(!d001_line.contains("缺建议"), "direction D001 must not show 缺建议: {d001_line}");

    // Move selection to Q02 -> right pane immediately shows 在等它的工作 with W001 details
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let card_screen = render_to_string(&mut app, 170, 32);
    assert!(card_screen.contains("在等它的工作"), "{card_screen}");
    assert!(card_screen.contains("[W001] 落地新路由"), "{card_screen}");
    assert!(card_screen.contains("完成条件：灰度与单测全绿"), "{card_screen}");
    assert!(
        card_screen.contains("说明：docs/plan/") && card_screen.contains("routing.md"),
        "{card_screen}"
    );
}

#[test]
fn tui_eta_overview_runs_work_and_compact_phone_6col_badges() {
    use delegate_tui::data::parse_snapshot_json;

    let raw = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-11-25T03:00:00Z",
        "seq": 50,
        "repos": [
            {
                "path": "/repo/demo",
                "runsDir": "/repo/demo/.local/run/delegate",
                "runs": [
                    {
                        "run": "20261125-025000-live-a",
                        "name": "live-a",
                        "state": "running",
                        "agent": "pi",
                        "mode": "write",
                        "modelName": "neon",
                        "elapsedSeconds": 600,
                        "ageSeconds": 600,
                        "eta": {
                            "stage": "agent_work",
                            "stageLabel": "模型工作中",
                            "stoppedClock": false,
                            "shortText": "预计 03:40 交付（p75 04:10）",
                            "compactBadge": "~03:40",
                            "detailText": "预估：03:40 交付（p75 04:10）〔模型工作中 10m，剩 ~40m〕",
                            "toDelivered": {"p50Clock": "03:40", "p75Clock": "04:10", "anomaly": false},
                            "isPrior": false
                        }
                    },
                    {
                        "run": "20261125-023000-blocked-b",
                        "name": "blocked-b",
                        "state": "delivered",
                        "agent": "pi",
                        "mode": "write",
                        "modelName": "neon",
                        "elapsedSeconds": 300,
                        "ageSeconds": 1800,
                        "question": true,
                        "eta": {
                            "stage": "blocked",
                            "stageLabel": "停表阻塞",
                            "stoppedClock": true,
                            "shortText": "卡在主控·提问 12m",
                            "compactBadge": "卡主控",
                            "detailText": "停表：卡在主控·提问 12m",
                            "blocker": {"kind": "controller_question", "who": "主控", "label": "卡在主控·提问 12m", "compact": "卡主控"},
                            "isPrior": false
                        }
                    }
                ],
                "cleanedNotes": []
            }
        ],
        "work": [
            {
                "id": "W012",
                "title": "实现缓存层",
                "repo": "/repo/demo",
                "repoLabel": "demo",
                "state": "active",
                "runs": ["20261125-025000-live-a"],
                "focus": "在跑",
                "eta": {
                    "stoppedClock": false,
                    "shortText": "预计 04:30 合入（p75 05:00）",
                    "focusBadge": "~04:30",
                    "compactBadge": "~04:30",
                    "remaining": {"p50Clock": "04:30", "p75Clock": "05:00"},
                    "isPrior": false
                }
            },
            {
                "id": "W013",
                "title": "接入下游监控",
                "repo": "/repo/demo",
                "repoLabel": "demo",
                "state": "blocked",
                "waitOn": ["work_applied:W012"],
                "focus": "等 W012",
                "eta": {
                    "stoppedClock": false,
                    "shortText": "等 W012·预计 05:10 合入（p75 06:00）",
                    "focusBadge": "等W012(~05:10)",
                    "compactBadge": "~05:10",
                    "remaining": {"p50Clock": "05:10", "p75Clock": "06:00"},
                    "isPrior": false
                }
            },
            {
                "id": "W014",
                "title": "等待所有者拍板",
                "repo": "/repo/demo",
                "repoLabel": "demo",
                "state": "blocked",
                "waitOn": ["owner:Q03"],
                "focus": "等所有者 Q03",
                "eta": {
                    "stoppedClock": true,
                    "shortText": "卡在所有者 [Q03]·解锁后约 1.2–1.7h",
                    "focusBadge": "卡所有者Q03",
                    "compactBadge": "卡所有",
                    "blocker": {"kind": "owner", "who": "所有者", "label": "卡在所有者 [Q03]", "compact": "卡所有"},
                    "isPrior": false
                }
            }
        ],
        "events": [],
        "lane": {},
        "dagQueue": [],
        "merges": [],
        "candidates": [],
        "eta": {
            "wave": {
                "activeCount": 1,
                "runningCount": 1,
                "queuedCount": 0,
                "blockedCount": 1,
                "earliestP50Clock": "03:40",
                "waveP50Clock": "03:40",
                "waveP75Clock": "04:10",
                "isPrior": false,
                "blockers": [
                    {"run": "20261125-023000-blocked-b", "name": "blocked-b", "kind": "controller_question", "compact": "卡主控", "label": "卡在主控·提问 12m"}
                ]
            },
            "work": {
                "writeChains": 2,
                "depth": 2,
                "activeCount": 1,
                "devWallRange": "1.5–2.2h",
                "earliestFinishP50Clock": "05:10",
                "earliestFinishP75Clock": "06:00",
                "externalWaitCount": 0,
                "ownerBlocked": [{"id": "W014"}],
                "blockers": [],
                "headline": "推进 1.5–2.2h（2写链/深2，p50 05:10 / p75 06:00） + 卡所有者 1",
                "isPrior": false
            },
            "queue": {"writeChains": 0, "depth": 0, "activeCount": 0, "devWallRange": "<5m", "ownerBlocked": [], "blockers": [], "headline": ""}
        }
    }"#;

    let snap = parse_snapshot_json(raw, None, None).expect("snapshot with eta parses");
    assert_eq!(snap.eta.wave.active_count, 1);
    assert_eq!(snap.eta.wave.blocked_count, 1);
    assert_eq!(snap.eta.work.write_chains, 2);

    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/repo/demo"), 2);

    // 1. Overview wide (140x40): running & wave ETA card with wave clock + blocked item in attention card
    app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    let ov_wide = render_to_string(&mut app, 140, 40);
    assert_lines_fit_width(&ov_wide, 140);
    assert!(ov_wide.contains("正在运行与波次预估"), "{ov_wide}");
    assert!(ov_wide.contains("本波 03:40"), "{ov_wide}");
    assert!(ov_wide.contains("blocked-b"), "{ov_wide}");

    // 2. Overview narrow (80x24): shows running wave ETA & attention without overflow
    let ov_narrow = render_to_string(&mut app, 80, 24);
    assert_lines_fit_width(&ov_narrow, 80);
    assert!(ov_narrow.contains("正在运行"), "{ov_narrow}");
    assert!(ov_narrow.contains("live-a"), "{ov_narrow}");

    // 3. Runs wide (140x32): shows ~03:40 and 卡主控 + detail ETA line
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let runs_wide = render_to_string(&mut app, 140, 32);
    assert_lines_fit_width(&runs_wide, 140);
    assert!(runs_wide.contains("预估"), "{runs_wide}");
    assert!(runs_wide.contains("~03:40"), "{runs_wide}");
    assert!(runs_wide.contains("卡主控"), "{runs_wide}");
    assert!(runs_wide.contains("停表：卡在主控·提问 12m"), "{runs_wide}");

    // 4. Runs narrow/phone (60x24 < 70 cols): fits width and embeds <= 6 col badges
    let runs_phone = render_to_string(&mut app, 60, 24);
    assert_lines_fit_width(&runs_phone, 60);
    assert!(runs_phone.contains("~03:40"), "{runs_phone}");
    assert!(runs_phone.contains("卡主控"), "{runs_phone}");

    // 5. Work wide (180x32): shows wide focusBadge & pane title finish clock
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    let work_wide = render_to_string(&mut app, 180, 32);
    assert_lines_fit_width(&work_wide, 180);
    assert!(work_wide.contains("05:10"), "{work_wide}");
    assert!(work_wide.contains("~04:30"), "{work_wide}");
    assert!(work_wide.contains("等 W012 合入 (~05:10)"), "{work_wide}");
    assert!(!work_wide.contains("等 W012 合入 等W012"), "duplicate wait badge:\n{work_wide}");
    assert!(work_wide.contains("卡所有者Q03"), "{work_wide}");

    // 6. Work compact/phone (60x24 < 70 cols): strictly <= 6 col badges only!
    let work_phone = render_to_string(&mut app, 60, 24);
    assert_lines_fit_width(&work_phone, 60);
    assert!(work_phone.contains("~04:30"), "{work_phone}");
    assert!(work_phone.contains("~05:10"), "{work_phone}");
    assert!(work_phone.contains("卡所有"), "{work_phone}");
    assert!(!work_phone.contains("卡所有者Q03"), "compact phone view must use 6-col badge:\n{work_phone}");
    assert!(!work_phone.contains("等W012(~05:10)"), "compact phone view must use 6-col badge:\n{work_phone}");
}

#[test]
fn tui_polish_1125_fixes_collected_time_narrow_names_tier_dedup_local_time_lane_fold_and_procs() {
    use delegate_tui::app::ViewTab;
    use delegate_tui::data::parse_snapshot_json;
    use delegate_tui::model::shorten_running_cmd;

    // 1. Unit assertions for running-command shortening (Item 3) & UTF-8 safety
    assert_eq!(
        shorten_running_cmd("/home/user/.local/bin/delegate lane cargo xtask infra-test"),
        "cargo xtask infra-test"
    );
    assert_eq!(
        shorten_running_cmd("bash -lc 'TMPDIR=/tmp /home/user/.local/bin/uv run pytest -q'"),
        "uv run pytest"
    );
    assert_eq!(shorten_running_cmd("SCRATCH=/home/user/.gemini/foo"), "");
    assert_eq!(
        delegate_tui::model::parse_iso_utc_epoch("2026-11-25T05:18:中文"),
        None
    );

    // 2. Full snapshot covering Items 1, 3, 4, 5, 6, 7, 8, 9, 10c
    let snap_json = r#"{
        "generatedAt": "2026-11-25T04:15:00Z",
        "repos": [
            {
                "path": "/home/user/norvane",
                "runs": [
                    {
                        "run": "20261125-015401-org-step3-switcher",
                        "name": "org-step3-switcher",
                        "state": "delivered",
                        "mode": "write",
                        "tier": "write",
                        "modelName": "neon",
                        "ageSeconds": 25200,
                        "finishedAgoSeconds": 25200,
                        "changes": {"files": 38, "added": 1200, "deleted": 100},
                        "accept": {"ok": true},
                        "eta": {"compactBadge": "卡主控", "stoppedClock": true}
                    },
                    {
                        "run": "20261124-222515-org-step3-storage",
                        "name": "org-step3-storage",
                        "state": "delivered",
                        "mode": "write",
                        "tier": "write",
                        "modelName": "neon",
                        "ageSeconds": 39600,
                        "finishedAgoSeconds": 39600,
                        "changes": {"files": 32, "added": 900, "deleted": 80},
                        "accept": {"ok": true}
                    },
                    {
                        "run": "20261125-114020-gate-fix-1125",
                        "name": "gate-fix-1125",
                        "state": "running",
                        "mode": "write",
                        "tier": "write",
                        "modelName": "neon",
                        "ageSeconds": 1800,
                        "activity": {
                            "kind": "running_command",
                            "label": "跑命令",
                            "cmd": "/home/user/.local/bin/delegate lane cargo xtask infra-test"
                        },
                        "eta": {"compactBadge": "~12:45", "stoppedClock": false}
                    },
                    {
                        "run": "20261125-115613-phys2-t02-throughput",
                        "name": "phys2-t02-throughput",
                        "state": "delivered",
                        "mode": "write",
                        "tier": "write",
                        "modelName": "neon",
                        "ageSeconds": 1200,
                        "finishedAgoSeconds": 900
                    },
                    {
                        "run": "20261125-115816-7ddc",
                        "name": "reply to phys2-t02-throughput",
                        "parent": "20261125-115613-phys2-t02-throughput",
                        "state": "delivered",
                        "mode": "write",
                        "tier": "write",
                        "modelName": "neon",
                        "ageSeconds": 600,
                        "finishedAgoSeconds": 300
                    },
                    {
                        "run": "20261125-120500-9abc",
                        "name": "reply to phys2-t02-throughput",
                        "parent": "20261125-115816-7ddc",
                        "state": "running",
                        "mode": "write",
                        "tier": "write",
                        "modelName": "neon",
                        "ageSeconds": 120
                    }
                ],
                "cleanedNotes": [
                    {"run": "20261120-010000-old-note", "name": "old-note", "state": "cleaned", "cleaned": true}
                ]
            }
        ],
        "events": [
            {"seq": 100, "at": "2026-11-25T04:05:00Z", "kind": "lane_acquired", "repo": "norvane", "run": "20261125-115816-7ddc", "payload": {"label": "cargo test"}},
            {"seq": 101, "at": "2026-11-25T04:06:00Z", "kind": "lane_released", "repo": "norvane", "run": "20261125-115816-7ddc", "payload": {"label": "cargo test"}},
            {"seq": 102, "at": "2026-11-25T04:14:00Z", "kind": "delivered", "repo": "norvane", "run": "20261125-115816-7ddc", "payload": {"state": "delivered"}}
        ],
        "work": [
            {
                "id": "W043",
                "repo": "/home/user/norvane",
                "title": "小智搬去 alpha",
                "state": "blocked",
                "focus": "暂缓至 2026-11-25T05:18",
                "waitOn": ["after_time:2026-11-25T05:18:00Z"],
                "eta": {"focusBadge": "等13:18", "compactBadge": "等13:18", "stoppedClock": true}
            },
            {
                "id": "W110",
                "repo": "/home/user/norvane",
                "title": "物理阶段 2",
                "state": "blocked",
                "waitOn": ["run_delivered:20261125-115816-7ddc"],
                "waitOnStatus": [
                    {"cond": "run_delivered:20261125-115816-7ddc", "satisfied": false, "detail": "等 20261125-115816-7ddc 交付"}
                ]
            }
        ],
        "agyAccounts": {
            "active": "guimondbuchinsky",
            "runningAgy": [
                {"pid": 2119879, "run": "20261125-015401-org-step3-switcher", "runName": "org-step3-switcher", "role": "reviewer", "round": "round-1", "cmdline": "agy"},
                {"pid": 539385, "run": "20261125-114020-gate-fix-1125", "runName": "gate-fix-1125", "role": "author", "cmdline": "agy"}
            ],
            "accounts": [
                {
                    "id": "guimondbuchinsky",
                    "email": "gui@example.test",
                    "active": true,
                    "lastUsedAt": "2026-11-25T04:10:00Z",
                    "fileCount": 6,
                    "quota": {
                        "fetchedAt": "2026-11-25T04:14:00Z",
                        "ageSeconds": 60,
                        "ok": true,
                        "entries": [
                            {"pool": "Claude/GPT", "window": "fiveHour", "windowLabel": "5-Hour Limit Remaining", "remainingPct": 100.0, "resetInSeconds": 17820}
                        ]
                    }
                }
            ]
        }
    }"#;

    let snap = parse_snapshot_json(snap_json, Some(Path::new("/home/user")), None).unwrap();
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/user/norvane"), 5);

    // Tab 3 (Runs) wide (180x45):
    // - Item 1: 已收取 1 条 without bogus 213503982334601 天前
    // - Item 3: shortened running cmd [cargo xtask infra-test] without /home/user/.local/bin
    // - Item 5: detail pane shows `neon · write` once (no `write · write`)
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let runs_wide = render_to_string(&mut app, 180, 45);
    assert!(runs_wide.contains("已收取 1 条"), "{runs_wide}");
    assert!(!runs_wide.contains("213503982334601"), "bogus u64::MAX days leaked:\n{runs_wide}");
    assert!(runs_wide.contains("[cargo"), "short cmd missing:\n{runs_wide}");
    assert!(!runs_wide.contains("/home/user"), "raw HOME path leaked:\n{runs_wide}");
    assert!(runs_wide.contains("neon · write"), "{runs_wide}");
    assert!(!runs_wide.contains("write · write"), "duplicate tier/mode:\n{runs_wide}");

    // Tab 3 narrow (100x30): Item 4 keeps both org-step3-switcher and org-step3-storage distinguishable
    let runs_narrow = render_to_string(&mut app, 100, 30);
    assert!(runs_narrow.contains("org-step3-switcher"), "narrow truncated switcher:\n{runs_narrow}");
    assert!(runs_narrow.contains("org-step3-storage"), "narrow truncated storage:\n{runs_narrow}");

    // Tab 4 (Quota) narrow (100x30): Item 10c ensures space between `5 小时` and `剩余`
    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    let q_narrow = render_to_string(&mut app, 100, 30);
    assert!(!q_narrow.contains("5 小时剩余"), "quota label glued to 剩余:\n{q_narrow}");
    assert!(q_narrow.contains("5 小时") && q_narrow.contains("剩余 100%"), "{q_narrow}");

    // Tab 5 (Events): Item 8 hides lane_acquired/released by default, shows chain name, local time
    app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Events);
    let ev_screen = render_to_string(&mut app, 140, 30);
    assert!(ev_screen.contains("事件时间线 (显示 1 条 · 已隐2槽位"), "{ev_screen}");
    assert!(!ev_screen.contains("占槽"), "lane events should be hidden by default:\n{ev_screen}");
    assert!(ev_screen.contains("phys2-t02-throughput"), "non-leaf reply run should resolve to chain name:\n{ev_screen}");
    assert!(!ev_screen.contains("[norvane] 20261125-115816-7ddc"), "non-leaf reply run id leaked in event list:\n{ev_screen}");
    assert!(!ev_screen.contains("2026-11-25T04:14:00Z"), "detail should use local time:\n{ev_screen}");
    // Toggle 'l' shows lane events
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    let ev_with_lane = render_to_string(&mut app, 140, 30);
    assert!(ev_with_lane.contains("事件时间线 (显示 3 条 · 含槽位"), "{ev_with_lane}");
    assert!(ev_with_lane.contains("占槽"), "{ev_with_lane}");

    // Tab 2 (Work): Item 6 (single local time, no UTC ISO, TimedWait isolated) & Item 7 (run name instead of run id)
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    let work_screen = render_to_string(&mut app, 180, 30);
    assert!(!work_screen.contains("2026-11-25T05:18"), "UTC ISO leaked in work row:\n{work_screen}");
    assert!(!work_screen.contains("等13:18"), "duplicate ETA badge leaked in work row:\n{work_screen}");
    assert!(work_screen.contains("定时等待"), "pure after_time item should be in TimedWait bucket:\n{work_screen}");
    assert!(work_screen.contains("等 phys2-t02-throughput 交付"), "run name missing:\n{work_screen}");
    let w110_row = work_screen
        .lines()
        .find(|l| l.contains("W110") && !l.starts_with('╭'))
        .expect("W110 row");
    assert!(!w110_row.contains("20261125-115816-7ddc"), "non-leaf reply run id leaked in work row:\n{w110_row}");

    // Tab 4 (Quota): Item 9 shows run name + role (`审查 round-1`, `作者`)
    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    let acct_screen = render_to_string(&mut app, 150, 35);
    assert!(acct_screen.contains("2119879 · org-step3-switcher（审查 round-1）"), "{acct_screen}");
    assert!(acct_screen.contains("539385 · gate-fix-1125（作者）"), "{acct_screen}");
}

#[test]
fn quota_identity_mismatch_marks_unupdated_in_overview_and_accounts() {
    let mut app = make_rich_quota_and_charts_app();
    if let Some(alpha) = app.snapshot.agy_accounts.accounts.first_mut() {
        if let Some(q) = alpha.quota.as_mut() {
            q.ok = false;
            q.identity_mismatch = true;
            q.error = Some("身份不符，未更新".to_string());
        }
    }

    // Overview ('1') wide (140x40) & narrow (80x24)
    app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    let ov_wide = render_to_string(&mut app, 140, 40);
    assert!(ov_wide.contains("身份不符，未更新"), "overview wide missing mismatch badge:\n{ov_wide}");
    let ov_narrow = render_to_string(&mut app, 80, 24);
    assert!(ov_narrow.contains("身份不符，未更新"), "overview narrow missing mismatch badge:\n{ov_narrow}");

    // Quota ('4') wide (140x40) & narrow (80x24)
    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    let acct_wide = render_to_string(&mut app, 140, 40);
    assert!(acct_wide.contains("身份不符，未更新"), "quota wide missing mismatch badge:\n{acct_wide}");
    let acct_narrow = render_to_string(&mut app, 80, 24);
    assert!(acct_narrow.contains("身份不符，未更新"), "quota narrow missing mismatch badge:\n{acct_narrow}");
}

#[test]
fn tui_1126_fixes_ephemeral_work_rework_charts_quota_alert_and_stale_folding() {
    use delegate_tui::data::parse_snapshot_json;

    let snap_json = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-11-25T19:35:00Z",
        "seq": 100,
        "repos": [
            {
                "path": "/home/user/norvane",
                "runsDir": "/home/user/norvane/.local/run/delegate",
                "runs": [
                    {
                        "run": "20261125-193000-rw-task",
                        "name": "返工测试任务",
                        "state": "running",
                        "mode": "write",
                        "modelName": "neon",
                        "elapsedSeconds": 300,
                        "ageSeconds": 300,
                        "review": {"status": "reworking", "verdict": "REWORK", "round": 1, "reworkRounds": 1}
                    },
                    {
                        "run": "20261125-193400-cmd-task",
                        "name": "命令测试任务",
                        "state": "running",
                        "mode": "write",
                        "modelName": "neon",
                        "elapsedSeconds": 60,
                        "ageSeconds": 60,
                        "activity": {"kind": "running_command", "cmd": "cargo test", "elapsedSeconds": 10, "label": "在跑命令：cargo test（10秒）"}
                    }
                ],
                "cleanedNotes": []
            },
            {
                "path": "/home/user/whatsapp-bridge",
                "runsDir": "/home/user/whatsapp-bridge/.local/run/delegate",
                "runs": [
                    {
                        "run": "20261115-100000-old-wa",
                        "name": "十天前旧任务",
                        "state": "delivered",
                        "mode": "write",
                        "modelName": "neon",
                        "ageSeconds": 864000,
                        "finishedAgoSeconds": 864000,
                        "diff": {"files": 1}
                    }
                ],
                "cleanedNotes": []
            }
        ],
        "work": [
            {
                "id": "W119",
                "title": "推进链第一步",
                "repo": "/home/user/norvane",
                "repoLabel": "norvane",
                "state": "attention",
                "focus": "预合并中",
                "eta": {"focusBadge": "~03:42", "compactBadge": "~03:42", "stoppedClock": false}
            },
            {
                "id": "W120",
                "title": "推进链第二步",
                "repo": "/home/user/norvane",
                "repoLabel": "norvane",
                "state": "blocked",
                "waitOn": ["work_closed:W119"],
                "waitOnStatus": [{"condition": "work_closed:W119", "satisfied": false, "detail": "等 W119 关闭"}],
                "eta": {"focusBadge": "等W119(~04:40)", "compactBadge": "~04:40", "stoppedClock": false}
            },
            {
                "id": "R:cmd-task",
                "ephemeral": true,
                "title": "命令测试任务",
                "repo": "/home/user/norvane",
                "repoLabel": "norvane",
                "state": "active",
                "focus": "运行中 1m",
                "runs": ["20261125-193400-cmd-task"],
                "eta": {"compactBadge": "~04:00", "stoppedClock": false}
            }
        ],
        "dagQueue": [
            {"name": "agy-usage", "status": "dispatched", "repo": "/home/user/aia-skills", "runId": "20261124-040538-agy-usage"}
        ],
        "agyAccounts": {
            "active": "gui",
            "accounts": [
                {
                    "id": "gui",
                    "email": "gui@example.test",
                    "active": true,
                    "quota": {
                        "fetchedAt": "2026-11-25T19:34:50Z",
                        "ok": true,
                        "ageSeconds": 10,
                        "entries": [
                            {"pool": "Gemini Models", "window": "weekly", "windowLabel": "Weekly Limit Remaining", "remainingPct": 42.0, "resetInSeconds": 432000}
                        ]
                    },
                    "quotaEstimate": {
                        "weekly": {"pool": "Gemini Models", "remainingPct": 42.0, "tokensPerPct": 10000000, "estimatedRemainingHours": 11.0, "burnRateSource": "24h"}
                    }
                }
            ]
        },
        "analytics": {
            "byPurpose": {"write": 2540000000, "review": 631900000, "rework": 231600000, "readOnly": 89400000},
            "byModel": [{"model": "neon", "tokens": 3492900000}],
            "daily7d": [
                {"ymd": "2026-11-20", "date": "11-20", "mergedCount": 12, "medianMergeSecs": 12780, "totalChains": 12, "firstRoundPass": 12, "firstRoundPassPct": 100},
                {"ymd": "2026-11-21", "date": "11-21", "mergedCount": 18, "medianMergeSecs": 5580, "totalChains": 18, "firstRoundPass": 10, "firstRoundPassPct": 55}
            ]
        },
        "eta": {
            "wave": {"activeCount": 2, "runningCount": 2, "queuedCount": 0, "blockedCount": 0, "earliestP50Clock": "03:42", "waveP50Clock": "04:00", "blockers": []},
            "work": {"writeChains": 2, "depth": 2, "devWallRange": "1.2–2.0h", "earliestFinishP50Clock": "04:40", "deferredCount": 5, "earliestDeferredClock": "09:12", "ownerBlocked": [], "blockers": []}
        }
    }"#;

    let snap = parse_snapshot_json(snap_json, Some(Path::new("/home/user")), None).unwrap();
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/user/norvane"), 5);

    // 1. Runs tab ('3'): `跑命令` untruncated, `返工1轮` without English REWORK,
    //    quota depletion stays out of top alert strip (keeps `4 额度(!)` badge), dispatched queue and stale (>7d) pending folded by default!
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let tasks = render_to_string(&mut app, 180, 40);
    assert!(tasks.contains("跑命令"), "expected full 跑命令:\n{tasks}");
    assert!(tasks.contains("返工1轮"), "expected Chinese rework badge:\n{tasks}");
    assert!(!tasks.contains("REWORK"), "English REWORK must not appear:\n{tasks}");
    assert!(
        !tasks.contains("▲ 告警"),
        "quota depletion must not render top alert strip:\n{tasks}"
    );
    assert!(tasks.contains("4 额度(!)"), "quota tab badge missing:\n{tasks}");
    assert!(tasks.contains("已派队列 1 条"), "dispatched queue should fold by default:\n{tasks}");
    assert!(!tasks.contains("已派 agy-usage"), "dispatched entry should be hidden while folded:\n{tasks}");
    assert!(tasks.contains("更早陈旧 1 条") || tasks.contains("陈旧仓库 1 个"), "stale item should fold by default:\n{tasks}");
    assert!(!tasks.contains("十天前旧任务"), "stale run should be hidden while folded:\n{tasks}");

    // Toggle to ByRepo ('v') with show_all = true (`--all`) -> both dispatched queue and stale repo expand.
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    app.show_all = true;
    app.rebuild_rows(None);
    let tasks_all = render_to_string(&mut app, 180, 40);
    assert!(tasks_all.contains("agy-usage"), "show_all should reveal dispatched queue:\n{tasks_all}");
    assert!(tasks_all.contains("whatsapp-bridge"), "show_all should reveal stale repo:\n{tasks_all}");

    // 2. Work tab ('2'): includes ephemeral `R:cmd-task`, `2 工作(3)`, merged wait sentence,
    //    advancing ETA (~04:40), and no top alert strip for quota depletion.
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    let work = render_to_string(&mut app, 180, 40);
    assert!(work.contains("2 工作(3)"), "work tab count must include ephemeral R: row:\n{work}");
    assert!(work.contains("推进中 (1 · 含未挂工作项运行 1)"), "work bucket must show ephemeral sub-label:\n{work}");
    assert!(work.contains("R:cmd-task"), "ephemeral run must appear on work page:\n{work}");
    assert!(work.contains("等 W119 关闭 (~04:40)"), "wait text must be merged into one phrase:\n{work}");
    assert!(!work.contains("等 W119 关闭 等W119"), "duplicate wait phrase:\n{work}");
    assert!(work.contains("活跃链预计 04:40"), "advancing ETA missing:\n{work}");
    assert!(
        !work.contains("▲ 告警"),
        "quota depletion must not render top alert strip on work tab:\n{work}"
    );

    // 3. Quota tab ('4'): horizontal Token Distribution bars + horizontal 7d efficiency bars, zero `m█`/`%█` collision,
    //    and account-page depletion reminder remains visible!
    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    let quota = render_to_string(&mut app, 180, 45);
    assert!(!quota.contains("▲ 告警"), "quota tab must not show top alert strip:\n{quota}");
    assert!(quota.contains("用完"), "quota tab must still show depletion reminder:\n{quota}");
    assert!(quota.contains("近 7 天 Token 分布") && quota.contains("3.49B"), "{quota}");
    assert!(quota.contains("写任务") && quota.contains("72%"), "{quota}");
    assert!(quota.contains("自动审查") && quota.contains("18%"), "{quota}");
    assert!(quota.contains("退回返工") && quota.contains("6%"), "{quota}");
    assert!(quota.contains("只读侦察") && quota.contains("2%"), "{quota}");
    assert!(quota.contains("213m"), "{quota}");
    assert!(!quota.contains("213m█"), "value must not collide with bar block:\n{quota}");
    assert!(!quota.contains("█100%█"), "percent must not collide with bar block:\n{quota}");
}

#[test]
fn overview_five_cards_dynamic_height_and_recent_deliveries_180x50_and_100x30() {
    use delegate_tui::data::parse_snapshot_json;

    // 1. Empty attention & empty running -> both compress to 3 lines, donating height to work chains & recentDeliveries
    let mut work_items = Vec::new();
    for i in 1..=14 {
        work_items.push(format!(
            r#"{{"id":"W{i:03}","title":"推进任务 {i:02}","repo":"/home/dev/demo-api","repoLabel":"demo-api","state":"ready","focus":"可派发"}}"#
        ));
    }
    let mut deliveries = Vec::new();
    for i in 1..=12 {
        let hh = 2 + (i / 2);
        let mm = (i % 2) * 30;
        deliveries.push(format!(
            r#"{{"run":"20261126-{hh:02}{mm:02}00-deliv-{i}","name":"已清理交付-{i:02}","repo":"demo-api","sourceRoot":"/home/dev/demo-api","appliedAt":"2026-11-26T{hh:02}:{mm:02}:00Z","appliedEpoch":{},"appliedHead":"abcdef123456","changesFiles":3,"added":45,"deleted":12,"elapsedSeconds":720,"rounds":1,"conclusion":"已完成清理后交付回归验证-{i:02}","cleaned":true}}"#,
            1795572000 + i * 1800
        ));
    }
    let raw = format!(
        r#"{{
            "schemaVersion": 1,
            "generatedAt": "2026-11-26T10:00:00Z",
            "seq": 200,
            "repos": [],
            "work": [{}],
            "events": [],
            "lane": {{}},
            "dagQueue": [],
            "merges": [],
            "candidates": [],
            "recentDeliveries": [{}]
        }}"#,
        work_items.join(","),
        deliveries.join(",")
    );

    let snap = parse_snapshot_json(&raw, Some(Path::new("/home/dev")), None).unwrap();
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/demo-api"), 5);

    // Default tab is Overview
    let screen180 = render_to_string(&mut app, 180, 50);
    assert_lines_fit_width(&screen180, 180);
    // Healthy state: NO alert strip line
    assert!(!screen180.contains("▲ 告警"), "healthy state must not render alert strip:\n{screen180}");
    // Empty attention & empty running compress cleanly and show empty hints
    assert!(screen180.contains("所有者待拍板 0 · 待主控 0"), "{screen180}");
    assert!(screen180.contains("空闲 · 无在跑或排队运行"), "{screen180}");
    // Work chain card gets donated height -> shows many items (>= 12) without premature clipping
    assert!(screen180.contains("W001") && screen180.contains("W012"), "work chain card should receive donated height:\n{screen180}");
    // Cleaned deliveries appear from recentDeliveries with formatted time (NOT "-"), diff stats, and conclusion!
    assert!(screen180.contains("已清理交付-01") && screen180.contains("已清理交付-10"), "recentDeliveries missing:\n{screen180}");
    assert!(screen180.contains("3f +45/-12 · 12m · 1轮"), "delivery diff/duration/rounds missing:\n{screen180}");
    assert!(screen180.contains("已完成清理后交付回归验证-01"), "delivery conclusion missing:\n{screen180}");
    assert!(screen180.contains("合入 abcdef1"), "delivery commit hash missing:\n{screen180}");
    let deliv_line = screen180
        .lines()
        .find(|l| l.contains("已清理交付-01"))
        .expect("delivery row");
    assert!(!deliv_line.contains("│ -     demo-api"), "delivery time degraded to '-':\n{deliv_line}");

    // Compact height (180x18): overflow scrolls inside the card and shows 1-N/Total indicator
    let screen_short = render_to_string(&mut app, 180, 18);
    assert_lines_fit_width(&screen_short, 180);
    assert!(screen_short.contains("/14 "), "overflow work chain card must display scroll indicator 1-N/14:\n{screen_short}");

    // Narrow 100x30: single-column flow, every line fits within 100 cols
    let screen100 = render_to_string(&mut app, 100, 30);
    assert_lines_fit_width(&screen100, 100);
    assert!(screen100.contains("1总览"), "{screen100}");
    assert!(screen100.contains("▸ 需要关注"), "{screen100}");
}

#[test]
fn global_alert_strip_silent_when_healthy_and_visible_across_all_5_tabs_on_p0_p1() {
    use delegate_tui::data::parse_snapshot_json;

    // 1. Healthy state -> 0-row alert strip on all 5 tabs
    let healthy_raw = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-11-26T00:00:00Z",
        "seq": 1,
        "repos": [],
        "work": [],
        "events": [],
        "lane": {},
        "dagQueue": [],
        "merges": [],
        "candidates": []
    }"#;
    let healthy_snap = parse_snapshot_json(healthy_raw, None, None).unwrap();
    let mut healthy_app = App::new(healthy_snap, PathBuf::from("delegate"), PathBuf::from("."), 5);
    for key in ['1', '2', '3', '4', '5'] {
        healthy_app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        let s = render_to_string(&mut healthy_app, 140, 30);
        assert!(!s.contains("▲ 告警"), "healthy tab '{key}' must not render alert strip:\n{s}");
    }

    // 2. P0 liveDrifted + P0 premerge conflict + P1 stalled controller pending >= 30m
    //    (quota depletion + <15% low quota are ALSO present, and must NOT enter the top alert strip).
    let alert_raw = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-11-26T00:00:00Z",
        "seq": 2,
        "repos": [{
            "path": "/home/dev/demo-api",
            "runs": [
                {
                    "run": "20261125-235000-conflict",
                    "name": "conflict-run",
                    "state": "delivered",
                    "mode": "write",
                    "ageSeconds": 600,
                    "finishedAgoSeconds": 600,
                    "premerge": {"status": "conflict", "conflicts": ["src/main.rs"]}
                },
                {
                    "run": "20261125-230000-stalled-ctrl",
                    "name": "stalled-ctrl",
                    "state": "delivered",
                    "mode": "write",
                    "ageSeconds": 3600,
                    "finishedAgoSeconds": 3600
                }
            ],
            "cleanedNotes": []
        }],
        "agyAccounts": {
            "active": "alpha",
            "liveDrifted": true,
            "accounts": [{
                "id": "alpha",
                "email": "alpha@example.test",
                "active": true,
                "quota": {
                    "fetchedAt": "2026-11-26T00:00:00Z",
                    "ok": true,
                    "ageSeconds": 30,
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": 12.0, "resetInSeconds": 14400}
                    ]
                },
                "quotaEstimate": {
                    "fiveHour": {"pool": "Gemini Models", "remainingPct": 12.0, "tokensPerPct": 10000000, "estimatedRemainingHours": 1.2}
                }
            }]
        },
        "work": [],
        "events": [],
        "lane": {},
        "dagQueue": [],
        "merges": [],
        "candidates": []
    }"#;
    let alert_snap = parse_snapshot_json(alert_raw, Some(Path::new("/home/dev")), None).unwrap();
    let mut alert_app = App::new(alert_snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/demo-api"), 5);

    // All 5 tabs at 160x36 show 2-line global alert strip (with + 另有 1 条 for the 3rd alert)
    for key in ['1', '2', '3', '4', '5'] {
        alert_app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        let s = render_to_string(&mut alert_app, 160, 36);
        assert_lines_fit_width(&s, 160);
        assert!(s.contains("▲ 告警(3)"), "tab '{key}' missing alert strip:\n{s}");
        assert!(
            s.contains("额度·登录态漂移") && s.contains("冲突"),
            "tab '{key}' missing P0 alerts:\n{s}"
        );
        assert!(
            !s.contains("额度·需手动切号") && !s.contains("额度偏低·需手动切号"),
            "tab '{key}' must not include quota depletion/low in top alert strip:\n{s}"
        );
        assert!(s.contains("+ 另有 1 条"), "tab '{key}' missing overflow count:\n{s}");
    }

    // Narrow 100x30 compresses alert strip to 1 line and fits width 100
    alert_app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    let s100 = render_to_string(&mut alert_app, 100, 30);
    assert_lines_fit_width(&s100, 100);
    assert!(s100.contains("▲ 告警(3)") && s100.contains("+另有2条"), "narrow 100-col alert strip:\n{s100}");

    // 3. Quota depletion + <15% low quota alone -> NO top alert strip on Overview or Quota tab,
    //    while `4 额度(!)` badge and `4 额度` page reminders stay intact.
    let quota_only_raw = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-11-26T00:00:00Z",
        "seq": 3,
        "repos": [],
        "agyAccounts": {
            "active": "jzy-main",
            "liveDrifted": false,
            "accounts": [{
                "id": "jzy-main",
                "email": "jzy@example.test",
                "active": true,
                "quota": {
                    "fetchedAt": "2026-11-26T00:00:00Z",
                    "ok": true,
                    "ageSeconds": 30,
                    "entries": [
                        {"pool": "Gemini Models", "window": "weekly", "remainingPct": 63.0, "resetInSeconds": 432000},
                        {"pool": "Claude/GPT", "window": "fiveHour", "remainingPct": 11.0, "resetInSeconds": 10800}
                    ]
                },
                "quotaEstimate": {
                    "weekly": {"pool": "Gemini Models", "remainingPct": 63.0, "tokensPerPct": 10000000, "estimatedRemainingHours": 12.95, "burnRateSource": "24h"},
                    "pools": [
                        {"pool": "Gemini Models", "weekly": {"pool": "Gemini Models", "remainingPct": 63.0, "tokensPerPct": 10000000, "estimatedRemainingHours": 12.95, "burnRateSource": "24h"}}
                    ]
                }
            }]
        },
        "work": [],
        "events": [],
        "lane": {},
        "dagQueue": [],
        "merges": [],
        "candidates": []
    }"#;
    let quota_only_snap = parse_snapshot_json(quota_only_raw, None, None).unwrap();
    let mut quota_only_app = App::new(quota_only_snap, PathBuf::from("delegate"), PathBuf::from("."), 5);
    quota_only_app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    let ov_no_banner = render_to_string(&mut quota_only_app, 180, 50);
    assert_lines_fit_width(&ov_no_banner, 180);
    assert!(
        !ov_no_banner.contains("▲ 告警"),
        "quota depletion/low alone must not trigger top alert strip:\n{ov_no_banner}"
    );
    assert!(ov_no_banner.contains("4 额度(!)"), "4 额度(!) badge must stay:\n{ov_no_banner}");
    assert!(ov_no_banner.contains("用完"), "overview quota card hint must stay:\n{ov_no_banner}");

    quota_only_app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    let tab4_reminder = render_to_string(&mut quota_only_app, 180, 50);
    assert_lines_fit_width(&tab4_reminder, 180);
    assert!(!tab4_reminder.contains("▲ 告警"), "tab 4 must not show top alert strip:\n{tab4_reminder}");
    assert!(
        tab4_reminder.contains("⚠ 周 ~12h57m") && tab4_reminder.contains("用完"),
        "tab 4 must keep depletion reminder:\n{tab4_reminder}"
    );
    if std::env::var("PRINT_TUI_SNAPSHOTS").as_deref() == Ok("1") {
        println!("\n=== OVERVIEW_NO_QUOTA_BANNER 180x50 ===\n{ov_no_banner}");
        println!("\n=== QUOTA_TAB_REMINDER 180x50 ===\n{tab4_reminder}");
    }

    // 4. identityMismatch alone still triggers the top P0 alert strip.
    let mismatch_raw = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-11-26T00:00:00Z",
        "seq": 4,
        "repos": [],
        "agyAccounts": {
            "active": "alpha",
            "liveDrifted": false,
            "accounts": [{
                "id": "alpha",
                "email": "alpha@example.test",
                "active": true,
                "quota": {
                    "fetchedAt": "2026-11-26T00:00:00Z",
                    "ok": false,
                    "identityMismatch": true,
                    "ageSeconds": 30,
                    "entries": []
                }
            }]
        },
        "work": [],
        "events": [],
        "lane": {},
        "dagQueue": [],
        "merges": [],
        "candidates": []
    }"#;
    let mismatch_snap = parse_snapshot_json(mismatch_raw, None, None).unwrap();
    let mut mismatch_app = App::new(mismatch_snap, PathBuf::from("delegate"), PathBuf::from("."), 5);
    mismatch_app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    let ov_mismatch = render_to_string(&mut mismatch_app, 140, 30);
    assert!(
        ov_mismatch.contains("▲ 告警(1)") && ov_mismatch.contains("额度·身份不匹配"),
        "identityMismatch must still enter top alert strip:\n{ov_mismatch}"
    );
}

#[test]
fn runs_default_status_grouping_v_toggle_and_l1_card_risk_section() {
    use delegate_tui::data::parse_snapshot_json;

    let tmp_dir = std::env::temp_dir().join(format!(
        "delegate-tui-l1-card-test-{}",
        std::process::id()
    ));
    let run_a_dir = tmp_dir.join("20261126-010000-risk-run");
    let run_b_dir = tmp_dir.join("20261126-010500-clean-run");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&run_a_dir).unwrap();
    std::fs::create_dir_all(&run_b_dir).unwrap();

    // Write L1 card.json matching exact `crates/delegate/src/card.rs` schema (`file`/`line`/`location`/`reason`, `review.conclusion`)
    std::fs::write(
        run_a_dir.join("card.json"),
        r#"{
            "schemaVersion": 1,
            "review": {
                "enabled": true,
                "status": "done",
                "verdict": "NEEDS_CONTROLLER",
                "round": 1,
                "conclusion": "改动涉及受控鉴权路径，需主控复核令牌过期逻辑"
            },
            "risks": [
                {
                    "severity": "high",
                    "severityLabel": "高",
                    "file": "crates/api/src/auth.rs",
                    "line": 88,
                    "location": "crates/api/src/auth.rs:88",
                    "rule": "auth_surface",
                    "category": "鉴权/凭证",
                    "source": "规则+审查员",
                    "reason": "时钟回拨场景未覆盖"
                }
            ]
        }"#,
    )
    .unwrap();

    // Write L1 card.json with 0 risks for run_b
    std::fs::write(
        run_b_dir.join("card.json"),
        r#"{
            "schemaVersion": 1,
            "review": {
                "enabled": true,
                "status": "done",
                "verdict": "PASS",
                "round": 1,
                "conclusion": "改动清晰无风险"
            },
            "risks": []
        }"#,
    )
    .unwrap();

    let raw = format!(
        r#"{{
            "schemaVersion": 1,
            "generatedAt": "2026-11-26T02:00:00Z",
            "seq": 30,
            "repos": [
                {{
                    "path": "/home/dev/repo-alpha",
                    "runsDir": "{}",
                    "runs": [
                        {{
                            "run": "20261126-010000-risk-run",
                            "name": "risk-run",
                            "state": "delivered",
                            "mode": "write",
                            "modelName": "neon",
                            "ageSeconds": 600,
                            "finishedAgoSeconds": 600,
                            "accept": {{"ok": true, "command": "cargo test"}},
                            "review": {{"status": "done", "verdict": "NEEDS_CONTROLLER", "round": 1, "needsAttention": true}},
                            "card": {{"riskTotal": 1, "riskByCategory": [{{"category": "鉴权/凭证", "count": 1}}], "fastForward": null}}
                        }},
                        {{
                            "run": "20261126-010300-l0-only",
                            "name": "l0-only-run",
                            "state": "delivered",
                            "mode": "write",
                            "modelName": "neon",
                            "ageSeconds": 420,
                            "finishedAgoSeconds": 420,
                            "accept": {{"ok": true, "command": "cargo test"}},
                            "card": {{"riskTotal": 2, "riskByCategory": [{{"category": "鉴权/凭证", "count": 1}}, {{"category": "迁移/Schema", "count": 1}}], "fastForward": null}}
                        }}
                    ],
                    "cleanedNotes": []
                }},
                {{
                    "path": "/home/dev/repo-beta",
                    "runsDir": "{}",
                    "runs": [
                        {{
                            "run": "20261126-010500-clean-run",
                            "name": "clean-run",
                            "state": "delivered",
                            "mode": "write",
                            "modelName": "neon",
                            "ageSeconds": 300,
                            "finishedAgoSeconds": 300,
                            "accept": {{"ok": true, "command": "cargo test"}},
                            "review": {{"status": "done", "verdict": "PASS", "round": 1}},
                            "card": {{"riskTotal": 0, "riskByCategory": [], "fastForward": true}}
                        }}
                    ],
                    "cleanedNotes": []
                }}
            ],
            "work": [],
            "events": [],
            "lane": {{}},
            "dagQueue": [],
            "merges": [],
            "candidates": []
        }}"#,
        tmp_dir.display(),
        tmp_dir.display()
    );

    let snap = parse_snapshot_json(&raw, Some(Path::new("/home/dev")), None).unwrap();
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/repo-alpha"), 5);
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));

    // 1. Default cross-repo status grouping ('按状态'): all 3 pending runs appear under 待主控处理 (3)
    let by_status = render_to_string(&mut app, 180, 38);
    assert_lines_fit_width(&by_status, 180);
    assert!(by_status.contains("按状态"), "{by_status}");
    assert!(by_status.contains("待主控处理 (3)"), "{by_status}");
    assert!(by_status.contains("repo-alpha") && by_status.contains("repo-beta"), "{by_status}");
    // Right pane reads run_a's card.json on demand and renders real `file:line`, `reason`, and `review.conclusion`
    assert!(by_status.contains("审查卡"), "{by_status}");
    assert!(by_status.contains("crates/api/src/auth.rs:88"), "L1 card file:line risk missing:\n{by_status}");
    assert!(by_status.contains("时钟回拨场景未覆盖"), "{by_status}");
    assert!(by_status.contains("改动涉及受控鉴权路径"), "L1 review.conclusion missing:\n{by_status}");

    // 2. Move down to clean-run (0 risks, newest non-urgent) -> shows ✓ 0 风险项 & review.conclusion
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    let clean_detail = render_to_string(&mut app, 180, 38);
    assert!(clean_detail.contains("✓ 0 风险项"), "0-risk card should show ✓ 0 风险项:\n{clean_detail}");
    assert!(clean_detail.contains("改动清晰无风险"), "clean-run review.conclusion missing:\n{clean_detail}");

    // 3. Move down to l0-only-run (no card.json on disk, only real `riskTotal`/`riskByCategory` L0 summary)
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    let l0_detail = render_to_string(&mut app, 180, 38);
    assert!(l0_detail.contains("审查卡摘要 (L0)"), "{l0_detail}");
    assert!(l0_detail.contains("命中 2 项风险"), "L0 riskTotal must not fall back to 0:\n{l0_detail}");
    assert!(l0_detail.contains("鉴权/凭证 1、迁移/Schema 1"), "L0 riskByCategory summary missing:\n{l0_detail}");
    assert!(!l0_detail.contains("✓ 0 风险项"), "L0 with riskTotal=2 must not show 0 risks:\n{l0_detail}");

    // 4. Press 'v' to toggle to ByRepo ('按仓库'), and 'v' again back to ByStatus ('按状态')
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    let by_repo = render_to_string(&mut app, 180, 38);
    assert!(by_repo.contains("按仓库"), "{by_repo}");
    assert!(by_repo.contains("▌ ~/repo-alpha") && by_repo.contains("▌ ~/repo-beta"), "{by_repo}");

    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    let back_status = render_to_string(&mut app, 180, 38);
    assert!(back_status.contains("按状态") && back_status.contains("待主控处理 (3)"), "{back_status}");

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn remote_exec_cross_repo_and_reply_chain_at_80_and_120_cols() {
    let snap_raw = r#"{
        "schemaVersion": 1,
        "generatedEpoch": 1795681000,
        "repos": [
            {
                "path": "/home/dev/main-app",
                "runs": [
                    {"run":"20261126-010000-distill-r1","name":"模型蒸馏架构","state":"answered","mode":"read-only","modelName":"neon","elapsedSeconds":120,"ageSeconds":3600,"caller":"session-a"},
                    {"run":"20261126-020000-distill-r2","name":"reply to 模型蒸馏架构","parent":"20261126-010000-distill-r1","state":"answered","mode":"read-only","modelName":"neon","elapsedSeconds":180,"ageSeconds":2400,"caller":"session-a"},
                    {"run":"20261126-030000-distill-r3","name":"reply to reply to 模型蒸馏架构","parent":"20261126-020000-distill-r2","chainName":"模型蒸馏架构","chainRound":3,"state":"delivered","mode":"write","modelName":"neon","elapsedSeconds":240,"ageSeconds":600,"caller":"session-a","changes":{"files":2,"added":50,"deleted":10},"authorRounds":[
                        {"round":1,"run":"20261126-010000-distill-r1","kind":"initial","path":"/home/dev/.cache/delegate/archive/20261126-010000-distill-r1/record/result.md","totalLines":42,"totalChars":1200,"current":false},
                        {"round":2,"run":"20261126-020000-distill-r2","kind":"rework","path":"/home/dev/main-app/.local/run/delegate/20261126-020000-distill-r2/round-2-result.md","totalLines":30,"totalChars":900,"current":false},
                        {"round":3,"run":"20261126-030000-distill-r3","kind":"reply","path":"/home/dev/main-app/.local/run/delegate/20261126-030000-distill-r3/result.md","totalLines":55,"totalChars":1800,"current":true}
                    ]}
                ]
            },
            {
                "path": "/home/dev/main-app/tools/rl-sim",
                "runs": [
                    {
                        "run": "20261126-031000-train-m4",
                        "name": "底盘蒸馏 M4",
                        "state": "running",
                        "mode": "write",
                        "modelName": "neon",
                        "caller": "other-session-b",
                        "elapsedSeconds": 900,
                        "ageSeconds": 900,
                        "remote": {"host": "gpu-1"},
                        "remoteExec": {
                            "host": "gpu-1",
                            "active": true,
                            "running": true,
                            "queue": "remote-heavy",
                            "cmd": "uv run --frozen --extra gpu python scripts/train.py",
                            "elapsedSeconds": 420,
                            "outputTail": [
                                "epoch 10 loss=0.142",
                                "epoch 11 loss=0.118"
                            ]
                        }
                    }
                ]
            }
        ],
        "lane": {
            "maxHeavy": 4,
            "remoteMaxHeavy": 1,
            "local": [],
            "remote": [
                {"label": "/tmp/wt/train-m4: uv run --frozen --extra gpu python scripts/train.py", "running": true, "since": "2026-11-26T08:00:00Z"}
            ]
        },
        "events": [],
        "work": []
    }"#;
    let snap = delegate_tui::data::parse_snapshot_json(
        snap_raw,
        Some(Path::new("/home/dev")),
        Some(Path::new("/home/dev/main-app")),
    )
    .expect("snapshot parses");
    let mut app = App::new(
        snap,
        PathBuf::from("delegate"),
        PathBuf::from("/home/dev/main-app"),
        5,
    );

    // Overview tab (120 cols & 80 cols): shows remote run with host alias and collapsed reply chain
    let ov120 = render_to_string(&mut app, 120, 34);
    assert_lines_fit_width(&ov120, 120);
    assert!(ov120.contains("底盘蒸馏 M4"), "remote run missing from 120-col overview:\n{ov120}");
    assert!(ov120.contains("@gpu-1"), "remote host alias missing from 120-col overview:\n{ov120}");
    assert!(ov120.contains("模型蒸馏架构（第 3 轮）"), "collapsed reply chain missing from 120-col overview:\n{ov120}");
    assert!(!ov120.contains("reply to "), "raw reply prefix leaked in overview:\n{ov120}");

    let ov80 = render_to_string(&mut app, 80, 34);
    assert_lines_fit_width(&ov80, 80);
    assert!(ov80.contains("底盘蒸馏 M4"), "remote run missing from 80-col overview:\n{ov80}");
    assert!(ov80.contains("@gpu-1"), "remote host alias missing from 80-col overview:\n{ov80}");
    assert!(!ov80.contains("reply to "), "raw reply prefix leaked in 80-col overview:\n{ov80}");

    // Runs tab (120 cols): reply chain takes 1 row, remote run shows @gpu-1 + cmd + tail in detail
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let runs120 = render_to_string(&mut app, 120, 36);
    assert_lines_fit_width(&runs120, 120);
    assert!(runs120.contains("模型蒸馏架构（第3轮）"), "reply chain missing in 120-col runs:\n{runs120}");
    assert!(!runs120.contains("reply to "), "raw reply prefix leaked in runs:\n{runs120}");
    assert!(runs120.contains("底盘蒸馏 M4"), "remote run missing in 120-col runs:\n{runs120}");
    assert!(runs120.contains("@gpu-1"), "remote host alias missing in 120-col runs:\n{runs120}");
    assert!(runs120.contains("会话轮次与各轮原文 (共 3 轮)"), "expanded reply rounds missing in detail:\n{runs120}");
    assert!(runs120.contains("round-2-result.md"), "intra-run rework result path missing in detail:\n{runs120}");
    assert!(runs120.contains("共 42 行") && runs120.contains("1200 字"), "round stats missing in detail:\n{runs120}");

    // Select the remote run (next row down) -> detail pane shows remote command & output tail
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    let remote_detail_120 = render_to_string(&mut app, 120, 36);
    assert_lines_fit_width(&remote_detail_120, 120);
    assert!(remote_detail_120.contains("远端执行: @gpu-1"), "remote detail header missing:\n{remote_detail_120}");
    assert!(remote_detail_120.contains("scripts/train.py"), "remote cmd missing:\n{remote_detail_120}");
    assert!(remote_detail_120.contains("远端输出尾部 (最近 2 行)"), "remote tail header missing:\n{remote_detail_120}");
    assert!(remote_detail_120.contains("epoch 11 loss=0.118"), "remote tail content missing:\n{remote_detail_120}");

    // Runs tab at 80 cols
    let remote_detail_80 = render_to_string(&mut app, 80, 36);
    assert_lines_fit_width(&remote_detail_80, 80);
    assert!(remote_detail_80.contains("@gpu-1"), "remote host missing at 80 cols:\n{remote_detail_80}");
    assert!(remote_detail_80.contains("epoch 11 loss=0.118"), "remote tail missing at 80 cols:\n{remote_detail_80}");
}

#[test]
fn idle_remote_exec_does_not_shadow_review_or_activity_and_narrow_preserves_host() {
    let snap_raw = r#"{
        "schemaVersion": 1,
        "generatedEpoch": 1795681000,
        "repos": [
            {
                "path": "/home/dev/rl-sim",
                "runs": [
                    {
                        "run": "20261126-040000-reviewing-remote",
                        "name": "远端配置审查中任务",
                        "state": "running",
                        "mode": "write",
                        "modelName": "neon",
                        "elapsedSeconds": 300,
                        "ageSeconds": 300,
                        "review": {"status": "reviewing", "round": 1, "reworkRounds": 0},
                        "activity": {"kind": "thinking", "label": "在思考"},
                        "remoteExec": {"host": "gpu-1", "active": false, "running": false}
                    },
                    {
                        "run": "20261126-040100-active-local-cmd",
                        "name": "长名字测试任务用于验证窄列保留主机别名",
                        "state": "running",
                        "mode": "write",
                        "modelName": "neon",
                        "elapsedSeconds": 180,
                        "ageSeconds": 180,
                        "activity": {"kind": "running_command", "cmd": "pytest -q", "label": "在跑命令：pytest -q"},
                        "remoteExec": {"host": "gpu-2", "active": false, "running": false}
                    }
                ]
            }
        ],
        "lane": {"maxHeavy": 4, "local": [], "remote": []},
        "events": [],
        "work": []
    }"#;
    let snap = delegate_tui::data::parse_snapshot_json(
        snap_raw,
        Some(Path::new("/home/dev")),
        Some(Path::new("/home/dev/rl-sim")),
    )
    .expect("snapshot parses");
    let mut app = App::new(
        snap,
        PathBuf::from("delegate"),
        PathBuf::from("/home/dev/rl-sim"),
        5,
    );

    // Overview at 120 cols (narrow left card, act_w == 0): preserves @gpu-2 even with long task name
    let ov120 = render_to_string(&mut app, 120, 30);
    assert_lines_fit_width(&ov120, 120);
    assert!(ov120.contains("[@gpu-2]"), "narrow overview dropped @gpu-2:\n{ov120}");

    // Overview at 180 cols (wide left card, act_w > 0): preserves local activity (`跑 pytest` / `在思考`) instead of shadowing with `@gpu-1`
    let ov180 = render_to_string(&mut app, 180, 30);
    assert_lines_fit_width(&ov180, 180);
    assert!(ov180.contains("在思考") || ov180.contains("pytest"), "idle remote shadowed local activity in overview:\n{ov180}");

    // Runs tab at 120 cols: preserves `[审查中]` and `[pytest -q]` instead of shadowing with `[@gpu-1]`
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let runs120 = render_to_string(&mut app, 120, 30);
    assert_lines_fit_width(&runs120, 120);
    assert!(runs120.contains("[审查中]"), "idle remote shadowed review_snippet:\n{runs120}");
    assert!(runs120.contains("pytest"), "idle remote shadowed local activity cmd:\n{runs120}");
}

#[test]
fn runs_detail_renders_agy_account_switch_notice() {
    use delegate_tui::data::parse_snapshot_json;

    let raw = r#"{
        "seq": 10,
        "repos": [{
            "repo": "/home/dev/repo-alpha",
            "runs": [{
                "run": "20261126-155900-sticky-reply",
                "name": "粘滞续做测试",
                "state": "delivered",
                "mode": "write",
                "model": "agy/gemini-4-neon",
                "agyAccount": "beta",
                "agyAccountSwitch": {
                    "from": "alpha",
                    "to": "beta",
                    "reason": "额度已耗尽"
                }
            }]
        }],
        "agyAccounts": {
            "active": "beta",
            "liveDrifted": false,
            "accounts": []
        },
        "work": [],
        "events": [],
        "lane": {},
        "dagQueue": [],
        "merges": [],
        "candidates": []
    }"#;
    let snap = parse_snapshot_json(raw, Some(Path::new("/home/dev")), None).unwrap();
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/repo-alpha"), 5);
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let rendered = render_to_string(&mut app, 160, 30);
    assert!(
        rendered.contains("本 run 已从 alpha 换到 beta（额度已耗尽）"),
        "detail pane should render agy account switch line:\n{rendered}"
    );
}

#[test]
fn runs_detail_and_quota_tab_render_auto_switch_and_handoff_lines() {
    use delegate_tui::data::parse_snapshot_json;

    let raw = r#"{
        "seq": 11,
        "repos": [{
            "repo": "/home/dev/repo-alpha",
            "runs": [{
                "run": "20261127-025500-auto-handoff",
                "name": "自动选号测试",
                "state": "delivered",
                "mode": "write",
                "model": "agy/gemini-4-neon",
                "agyAccount": "beta",
                "agyAccountSwitch": {
                    "kind": "auto",
                    "from": "alpha",
                    "to": "beta",
                    "reason": "alpha 黄档，改用绿档 beta"
                },
                "reviewAgyAccountSwitch": {
                    "kind": "handoff",
                    "from": "alpha",
                    "to": "beta",
                    "reason": "审查中额度耗尽，完整模板换号重审"
                }
            }]
        }],
        "agyAccounts": {
            "active": "alpha",
            "mode": "auto",
            "selected": "auto",
            "liveDrifted": false,
            "autoSwitch": {
                "enabled": true,
                "mode": "auto",
                "fiveHourMinPct": 30,
                "weeklyMinPct": 30,
                "refreshMinutes": 15,
                "perRunReservePct": 5,
                "poolCurrent": {"gemini": "beta", "claude_gpt": "alpha"},
                "poolSummary": {
                    "gemini": {
                        "pool": "gemini",
                        "label": "Gemini",
                        "current": "beta",
                        "selected": "beta",
                        "tier": "green",
                        "tierLabel": "绿",
                        "wouldSwitch": false,
                        "reason": "维持当前号 beta：绿档"
                    },
                    "claude_gpt": {
                        "pool": "claude_gpt",
                        "label": "Claude/GPT",
                        "current": "alpha",
                        "selected": "alpha",
                        "tier": "green",
                        "tierLabel": "绿",
                        "wouldSwitch": false,
                        "reason": "维持当前号 alpha：绿档"
                    }
                }
            },
            "accounts": [{
                "id": "alpha",
                "email": "secret-alpha@example.test",
                "active": true,
                "fileCount": 5
            }, {
                "id": "beta",
                "email": "secret-beta@example.test",
                "active": false,
                "fileCount": 5
            }]
        },
        "work": [],
        "events": [],
        "lane": {},
        "dagQueue": [],
        "merges": [],
        "candidates": []
    }"#;
    let snap = parse_snapshot_json(raw, Some(Path::new("/home/dev")), None).unwrap();
    assert!(snap.agy_accounts.auto_switch.enabled);
    assert_eq!(snap.agy_accounts.auto_switch.refresh_minutes, 15);
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/repo-alpha"), 5);
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let runs_rendered = render_to_string(&mut app, 160, 30);
    assert!(
        runs_rendered.contains("自动选号：alpha → beta（alpha 黄档，改用绿档 beta）"),
        "detail pane should render auto switch line:\n{runs_rendered}"
    );
    assert!(
        runs_rendered.contains("审查换号重审：alpha → beta（审查中额度耗尽，完整模板换号重审）"),
        "detail pane should render reviewer handoff line:\n{runs_rendered}"
    );
    assert!(!runs_rendered.contains("secret-alpha@example.test"));

    // Tab 4 (`4 额度`): in auto mode, default selection lands on the `auto` row!
    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    let quota_rendered = render_to_string(&mut app, 180, 32);
    assert!(
        quota_rendered.contains("★ 自动") && quota_rendered.contains("auto"),
        "quota tab should show active auto row:\n{quota_rendered}"
    );
    assert!(
        quota_rendered.contains("Gemini→beta(绿)") && quota_rendered.contains("Claude/GPT→alpha(绿)"),
        "auto row should show pool pointers and tiers:\n{quota_rendered}"
    );
    assert!(
        quota_rendered.contains("维持当前号 beta：绿档"),
        "auto detail pane should show pool reason:\n{quota_rendered}"
    );
    assert!(
        quota_rendered.contains("实盘 ~/.gemini 保持 alpha"),
        "auto detail pane should show live slot untouched:\n{quota_rendered}"
    );

    // Pressing `u` on the `auto` row opens confirm modal and confirming emits SwitchAgyAccount("auto").
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
    let modal = render_to_string(&mut app, 130, 30);
    assert!(modal.contains("auto（按池自动选号"), "{modal}");
    let act = app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(
        act,
        delegate_tui::app::AppAction::SwitchAgyAccount("auto".to_string())
    );
}

#[test]
fn overview_renders_line_stats_card_wide_and_narrow_and_survives_snapshot_refresh() {
    use delegate_tui::app::ViewTab;
    use delegate_tui::data::parse_line_stats_json;

    let raw = r#"{
        "windows": [
            {
                "key": "24h",
                "label": "24 小时",
                "total": {"added": 1940, "deleted": 438, "churn": 2378},
                "byCategory": {
                    "code": {"total": {"added": 1204, "deleted": 310, "churn": 1514}},
                    "test": {"total": {"added": 640, "deleted": 88, "churn": 728}},
                    "docs": {"total": {"added": 96, "deleted": 40, "churn": 136}}
                },
                "delegatedSharePct": 82.0,
                "linesPerMillionTokens": 35.0
            },
            {
                "key": "7d",
                "label": "7 天",
                "total": {"added": 14808, "deleted": 4667, "churn": 19475},
                "byCategory": {
                    "code": {"total": {"added": 9876, "deleted": 3402, "churn": 13278}},
                    "test": {"total": {"added": 4120, "deleted": 960, "churn": 5080}},
                    "docs": {"total": {"added": 812, "deleted": 305, "churn": 1117}}
                },
                "delegatedSharePct": 77.0,
                "linesPerMillionTokens": 41.0
            }
        ],
        "daily7d": [
            {"date": "2026-11-20", "added": 500, "deleted": 100, "churn": 600},
            {"date": "2026-11-21", "added": 1200, "deleted": 300, "churn": 1500},
            {"date": "2026-11-22", "added": 0, "deleted": 0, "churn": 0},
            {"date": "2026-11-23", "added": 2100, "deleted": 400, "churn": 2500},
            {"date": "2026-11-24", "added": 800, "deleted": 200, "churn": 1000},
            {"date": "2026-11-25", "added": 400, "deleted": 100, "churn": 500},
            {"date": "2026-11-26", "added": 1940, "deleted": 438, "churn": 2378}
        ],
        "errors": []
    }"#;
    let ls = parse_line_stats_json(raw).unwrap();
    let mut app = make_sample_app();
    app.snapshot.line_stats = Some(ls.clone());
    app.last_fingerprint = app.snapshot.data_fingerprint();
    app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Overview);

    // Wide overview (right column card)
    let wide = render_to_string(&mut app, 150, 44);
    assert_lines_fit_width(&wide, 150);
    assert!(wide.contains("代码改动"), "{wide}");
    assert!(wide.contains("+1,204 / \u{2212}310"), "{wide}");
    assert!(wide.contains("+14,808 / \u{2212}4,667"), "{wide}");
    assert!(wide.contains("委派占比") && wide.contains("82%") && wide.contains("77%"), "{wide}");
    assert!(wide.contains("每百万 token") && wide.contains("35 行") && wide.contains("41 行"), "{wide}");
    assert!(wide.contains("7 天每日") && wide.contains('█'), "{wide}");

    // Narrow overview (single-column flow)
    let narrow = render_to_string(&mut app, 80, 48);
    assert_lines_fit_width(&narrow, 80);
    assert!(narrow.contains("▸ 代码改动"), "{narrow}");
    assert!(narrow.contains("+1,940 / \u{2212}438"), "{narrow}");
    assert!(narrow.contains("7 天每日") && narrow.contains('█'), "{narrow}");

    // Applying a new AppSnapshot (whose line_stats is None) preserves existing line_stats.
    let mut next_snap = app.snapshot.clone();
    next_snap.line_stats = None;
    app.apply_snapshot(next_snap, false);
    assert_eq!(app.snapshot.line_stats.as_ref(), Some(&ls));
}
