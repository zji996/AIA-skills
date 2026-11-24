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
    assert!(screen.contains("验收 ✓ cargo test -p api"), "{screen}");
    assert!(!screen.contains("最近命令"), "accept command should not duplicate under 最近命令: {screen}");
    assert!(screen.contains("改动分布"), "{screen}");
    assert!(screen.contains("crates/api"), "{screen}");
    assert!(screen.contains("下一步"), "{screen}");

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
    let screen = render_to_string(&mut app, 80, 30);

    assert!(screen.contains("任务 (5)"), "{screen}");
    assert!(screen.contains("接口鉴权"), "{screen}");
    assert!(screen.contains("~/demo-api"), "{screen}");
    assert!(screen.contains("[2轮]"), "{screen}");
    assert!(screen.contains("3 分钟前"), "{screen}");
    assert!(screen.contains("结论摘要"), "{screen}");
    assert!(screen.contains("Lane ██░░░░ 2/6"), "{screen}");
    assert!(screen.contains("令牌 88/88 闲31"), "{screen}");
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
        refreshed_at: "11:00:00".to_string(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("."), 5);
    let empty = render_to_string(&mut app, 80, 24);
    assert!(empty.contains("暂无委派任务"), "{empty}");
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
        dag_queue: parse_dag_queue_jsonl(raw_queue),
        refreshed_at: "09:06:10".into(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/demo-api"), 5);

    // Runs tab shows liveness badges ("跑命", "卡警"), detail "正在做什么", and DAG queue section!
    let runs_screen = render_to_string(&mut app, 140, 32);
    assert!(runs_screen.contains("跑命"), "{runs_screen}");
    assert!(runs_screen.contains("卡警"), "{runs_screen}");
    assert!(runs_screen.contains("正在做什么"), "{runs_screen}");
    assert!(runs_screen.contains("续派队列 (1)"), "{runs_screen}");
    assert!(runs_screen.contains("step-2"), "{runs_screen}");
    assert!(runs_screen.contains("2事件(3)"), "{runs_screen}");

    // Press '2' to switch to Events tab!
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    let events_screen = render_to_string(&mut app, 140, 32);
    assert!(events_screen.contains("事件流 (3) · 跟随"), "{events_screen}");
    assert!(events_screen.contains("#1"), "{events_screen}");
    assert!(events_screen.contains("卡警"), "{events_screen}");
    assert!(events_screen.contains("step-2"), "{events_screen}");
    assert!(events_screen.contains("事件 #3"), "{events_screen}");
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
    let screen = render_to_string(&mut app, 150, 34);
    assert!(screen.contains("主审"), "{screen}");
    assert!(screen.contains("NEEDS_CONTROLLER"), "{screen}");
    assert!(screen.contains("PASS"), "{screen}");
    assert!(screen.contains("审中"), "{screen}");
    assert!(screen.contains("审查中"), "{screen}");
    assert!(screen.contains("返工"), "{screen}");
    assert!(screen.contains("REWORK 第2轮"), "{screen}");
    assert!(screen.contains("controllerReviewPaths"), "{screen}");
    assert!(screen.contains("sha256:abcdef012345"), "{screen}");

    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
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

    // Top bar shows 3合入(1) and 4候选(1)
    let home = render_to_string(&mut app, 148, 32);
    assert!(home.contains("3合入(1)"), "{home}");
    assert!(home.contains("4候选(1)"), "{home}");

    // Switch to Merges tab ('3')
    app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    let merges_screen = render_to_string(&mut app, 148, 32);
    assert!(merges_screen.contains("合入批次 (1)"), "{merges_screen}");
    assert!(merges_screen.contains("20261123-110000-merge"), "{merges_screen}");
    assert!(merges_screen.contains("可自主合入"), "{merges_screen}");
    assert!(merges_screen.contains("SKIPPED"), "{merges_screen}");
    assert!(merges_screen.contains("delegate apply --keep-commits 20261123-110000-merge"), "{merges_screen}");

    // Press 'a' in Merges tab opens CommandModal for apply --keep-commits
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    let apply_modal = render_to_string(&mut app, 148, 32);
    assert!(apply_modal.contains("apply --keep-commits 20261123-110000-merge"), "{apply_modal}");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    // Switch to Candidates tab ('4')
    app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    let cand_screen = render_to_string(&mut app, 148, 32);
    assert!(cand_screen.contains("Radar 候选板 (1)"), "{cand_screen}");
    assert!(cand_screen.contains("r-a1b2c3d4"), "{cand_screen}");
    assert!(cand_screen.contains("补齐 capabilities 缺口测试"), "{cand_screen}");
    assert!(cand_screen.contains("crates/api/src/lib.rs:42"), "{cand_screen}");
    assert!(cand_screen.contains("delegate radar pick r-a1b2c3d4"), "{cand_screen}");

    // Press 'p' in Candidates tab opens CommandModal for radar pick
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
    let pick_modal = render_to_string(&mut app, 148, 32);
    assert!(pick_modal.contains("radar pick r-a1b2c3d4"), "{pick_modal}");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    // Switch to Events tab ('2') and verify merge/radar event badges
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
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
    // Press 'w' to enter Work view
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Work);

    // 1. Wide 120 columns: unified full tab names, right-side counts trimmed cleanly without clipping
    let s120 = render_to_string(&mut app, 120, 34);
    let top120 = s120.lines().next().unwrap_or("");
    assert!(top120.contains("0总览") && top120.contains("1任务") && top120.contains("5工作"), "{top120}");
    assert!(!top120.contains("0览"), "work tab must not shrink tab labels: {top120}");
    assert!(!top120.trim_end().ends_with('·'), "top bar must not leave trailing separator: {top120}");
    assert!(s120.contains("等你 4"), "{s120}");
    assert!(s120.contains("需处理 2"), "{s120}");
    assert!(s120.contains("就绪 1"), "{s120}");

    // Very wide 160 columns shows all work counts
    let s160 = render_to_string(&mut app, 160, 34);
    assert!(s160.contains("等你 4"), "{s160}");
    assert!(s160.contains("需处理 2"), "{s160}");
    assert!(s160.contains("就绪 1"), "{s160}");
    assert!(s160.contains("推进中 2"), "{s160}");
    assert!(s160.contains("等待 4"), "{s160}");
    assert!(s160.contains("暂缓 1"), "{s160}");
    assert!(s160.contains("今天完成 1"), "{s160}");

    // Owner items pinned at top in 动手 -> 点头 -> 看卡 -> 方向 order
    let pos_q18 = s120.find("Q18").expect("Q18");
    let pos_q20 = s120.find("Q20").expect("Q20");
    let pos_q21 = s120.find("Q21").expect("Q21");
    let pos_d002 = s120.find("D002").expect("D002");
    assert!(pos_q18 < pos_q20 && pos_q20 < pos_q21 && pos_q21 < pos_d002, "{s120}");

    // Group order: 等你 -> 需处理 -> 推进中 -> 就绪 -> 等待 -> 暂缓 -> 已完成折叠
    let pos_owner = s120.find("▾ 等你").expect("owner group");
    let pos_need = s120.find("▾ 需处理").expect("need group");
    let pos_prog = s120.find("▾ 推进中").expect("prog group");
    let pos_ready = s120.find("▾ 就绪").expect("ready group");
    let pos_wait = s120.find("▾ 等待").expect("wait group");
    let pos_defer = s120.find("▾ 暂缓").expect("defer group");
    let pos_done_fold = s120.find("▸ 已完成 1 条").expect("done fold");
    assert!(
        pos_owner < pos_need
            && pos_need < pos_prog
            && pos_prog < pos_ready
            && pos_ready < pos_wait
            && pos_wait < pos_defer
            && pos_defer < pos_done_fold,
        "{s120}"
    );
    // Done item is folded by default
    assert!(!s120.contains("今天完成的基础重构"), "{s120}");

    // Dependency chain indentation: W033 -> W034 -> W035 (+1) and W029 -> W031
    assert!(s120.contains("└─ ◌ W034"), "{s120}");
    assert!(s120.contains("   └─ ◌ W035"), "{s120}");
    assert!(s120.contains("+1"), "{s120}");
    assert!(s120.contains("└─ ◌ W031"), "{s120}");

    // Live run one-liners
    assert!(s120.contains("运行中 12 分钟 · 在跑验收"), "{s120}");
    assert!(s120.contains("审查中"), "{s120}");
    assert!(s120.contains("待合入 · 验收✓ 审查 PASS"), "{s120}");
    assert!(s120.contains("等 W033 关闭"), "{s120}");
    assert!(s120.contains("等所有者 Q12"), "{s120}");

    // 2. 80 columns (unified full tab names, counts trimmed cleanly on the right)
    let s80 = render_to_string(&mut app, 80, 32);
    let top80 = s80.lines().next().unwrap_or("");
    assert!(top80.contains("0总览") && top80.contains("5工作"), "{top80}");
    assert!(s80.contains("等你 4"), "{s80}");
    assert!(s80.contains("└─ ◌ W034"), "{s80}");
    assert!(s80.contains("+1"), "{s80}");

    // 3. 50 columns (mobile/compact layout: 2-line L0, 2-line items, compact ↳ prefix)
    let s50 = render_to_string(&mut app, 50, 48);
    let lines50: Vec<&str> = s50.lines().collect();
    assert!(lines50[0].contains("等你 4") && lines50[0].contains("推进中 2"), "{s50}");
    assert!(lines50[1].contains("等待 4") && lines50[1].contains("今天完成 1"), "{s50}");
    assert!(s50.contains("↳ ◌ W034"), "{s50}");
    assert!(s50.contains(" ↳ ◌ W035"), "{s50}");
    assert!(s50.contains("等待 +1"), "{s50}");

    // 4. Dropped filter ('x' key) surfaces Q19
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    let s_drop = render_to_string(&mut app, 100, 20);
    assert!(s_drop.contains("滤:已否"), "{s_drop}");
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
    empty_app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
    let s_empty = render_to_string(&mut empty_app, 80, 20);
    assert!(s_empty.contains("暂无工作项"), "{s_empty}");
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

    // Jump to owner card item Q21 and press Enter -> opens L2 Work Card with rendered Markdown!
    assert!(app.jump_to_work_id("Q21"));
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

    // 1. Runs tab: run rows show premerge tags AND detail pane shows full premerge line + log
    let runs_screen = render_to_string(&mut app, 170, 30);
    assert!(runs_screen.contains("快进✓ abcdef1"), "{runs_screen}");
    assert!(runs_screen.contains("预合并中"), "{runs_screen}");
    assert!(runs_screen.contains("快进✗ 主干已前进"), "{runs_screen}");
    assert!(runs_screen.contains("预合并冲突:src/lib.rs"), "{runs_screen}");
    assert!(runs_screen.contains("预合并: 预合并冲突：src/lib.rs"), "{runs_screen}");
    assert!(runs_screen.contains("/tmp/run/premerge.log"), "{runs_screen}");

    // 2. Work tab ("此刻在干什么"): shows live premerge status for every linked work item
    app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
    let work_screen = render_to_string(&mut app, 140, 28);
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

    // Press '6' to open Accounts tab
    app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Accounts);
    let screen = render_to_string(&mut app, 130, 30);
    assert!(screen.contains("6账号(2)"), "{screen}");
    assert!(screen.contains("agy 账号 (2)"), "{screen}");
    assert!(screen.contains("★ 活跃"), "{screen}");
    // Default masked display: 3-char prefix + `…`, no email column, one-line proc without cmdline
    assert!(screen.contains("ali…"), "{screen}");
    assert!(screen.contains("bob"), "{screen}");
    assert!(!screen.contains("alice@example.test"), "email hidden by default:\n{screen}");
    assert!(!screen.contains("bob@example.test"), "email hidden by default:\n{screen}");
    assert!(screen.contains("在跑 agy 进程: 1 个"), "{screen}");
    assert!(screen.contains("4242 · 20261124-030000-demo"), "{screen}");
    assert!(!screen.contains("pid=4242"), "old pid= format should be replaced:\n{screen}");
    assert!(screen.contains("i 全称"), "bottom bar should advertise `i 全称`:\n{screen}");

    // Move selection down to 'bob' and press Enter to open in-place confirmation modal
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(app.selected_account_item().map(|a| a.id.as_str()), Some("bob"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let modal_screen = render_to_string(&mut app, 130, 30);
    assert!(modal_screen.contains("确认切换 agy 账号"), "{modal_screen}");
    assert!(modal_screen.contains("bob (bob@example.test)"), "{modal_screen}");
    assert!(modal_screen.contains("1 个在跑 agy 进程"), "{modal_screen}");
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
    app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Accounts);

    let screen = render_to_string(&mut app, 140, 34);
    assert!(screen.contains("5小时用量"), "{screen}");
    assert!(screen.contains("7天用量"), "{screen}");
    assert!(screen.contains("15.0M+"), "{screen}");
    assert!(screen.contains("50.0M"), "{screen}");
    assert!(screen.contains("▸ 本地用量"), "{screen}");
    assert!(screen.contains("近 5 小时: 输入 500.0K"), "{screen}");
    assert!(screen.contains("运行中未计 1"), "{screen}");
    assert!(screen.contains("近 7 天: 输入 2.0M"), "{screen}");
    assert_eq!(screen.matches("15.0M").count(), 1, "5h total should appear once (in list): {screen}");
    assert_eq!(screen.matches("50.0M").count(), 1, "7d total should appear once (in list): {screen}");
    assert!(screen.contains("本周剩余 95.41%"), "{screen}");
    assert!(screen.contains("5 小时剩余 85.1%"), "{screen}");
    assert!(screen.contains("按当前用量，5 小时额度约 1 天 4 小时后用完"), "{screen}");
    assert!(screen.contains("按当前用量，本周额度约 14 天 10 小时后用完"), "{screen}");
    assert!(!screen.contains("不自动换号"), "{screen}");
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

    // 1. Overview 120x40 (wide charts: Gauge + Sparkline + BarChart + 7d trends)
    app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Overview);
    let ov_wide = render_to_string(&mut app, 120, 40);
    assert_lines_fit_width(&ov_wide, 120);
    assert!(ov_wide.contains("0总览"), "{ov_wide}");
    assert!(ov_wide.contains("账号额度 · 3 分钟前更新"), "{ov_wide}");
    assert_eq!(ov_wide.matches("3 分钟前更新").count(), 1, "{ov_wide}");
    assert!(ov_wide.contains("Gemini · 5 小时"), "{ov_wide}");
    assert!(ov_wide.contains("Gemini · 本周"), "{ov_wide}");
    assert!(ov_wide.contains("Claude/GPT · 5 小时"), "{ov_wide}");
    assert!(ov_wide.contains("剩余 82% · 4 小时后重置"), "{ov_wide}");
    assert!(ov_wide.contains("剩余 91% · 6 天 9 小时后重置"), "{ov_wide}");
    assert!(ov_wide.contains("按最近 5 小时的平均用量，5 小时与本周额度在重置前用不完"), "{ov_wide}");
    assert!(!ov_wide.contains("Preview Tier: experimental pool active"), "unknown line should not show in overview: {ov_wide}");
    assert!(ov_wide.contains("近 24 小时 Token（按账号）"), "{ov_wide}");
    assert!(ov_wide.contains("近 7 天 Token 分布"), "{ov_wide}");
    assert!(ov_wide.contains("写"), "{ov_wide}");
    assert!(ov_wide.contains("审查"), "{ov_wide}");
    assert!(ov_wide.contains("返工"), "{ov_wide}");
    assert!(ov_wide.contains("只读"), "{ov_wide}");
    assert!(ov_wide.contains("派发→合入中位耗时"), "{ov_wide}");
    assert!(ov_wide.contains("一轮通过率"), "{ov_wide}");
    assert!(ov_wide.contains("11-18") && ov_wide.contains("11-24"), "{ov_wide}");
    // Wide overview bottom bar lists only usable keys on wide overview (no jk scroll, no 0-6 repeat).
    let ov_wide_last = ov_wide.lines().last().unwrap_or("");
    assert!(!ov_wide_last.contains("jk"), "{ov_wide_last}");
    assert!(!ov_wide_last.contains("0-6"), "{ov_wide_last}");
    for bad in forbidden {
        assert!(!ov_wide.contains(bad), "forbidden copy {bad:?} in ov_wide:\n{ov_wide}");
    }

    // Toggle BarChart to Model mode with 'v'
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    let ov_wide_model = render_to_string(&mut app, 120, 40);
    assert_lines_fit_width(&ov_wide_model, 120);
    assert!(ov_wide_model.contains("按模型"), "{ov_wide_model}");
    assert!(ov_wide_model.contains("gemini-4"), "{ov_wide_model}");
    assert_eq!(ov_wide_model.matches("41.2M").count(), 1, "model total should not duplicate in footer: {ov_wide_model}");
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));

    // 2. Overview 80x24 (narrow text degradation, zero misalignment)
    let ov_narrow = render_to_string(&mut app, 80, 24);
    assert_lines_fit_width(&ov_narrow, 80);
    assert!(ov_narrow.contains("总览 · 3 分钟前更新"), "{ov_narrow}");
    assert_eq!(ov_narrow.matches("3 分钟前更新").count(), 1, "{ov_narrow}");
    assert!(ov_narrow.contains("alp…"), "{ov_narrow}");
    assert!(!ov_narrow.contains("alpha"), "account id should be masked by default:\n{ov_narrow}");
    assert!(ov_narrow.contains("Gemini · 5 小时  剩余 82% · 4 小时后重置"), "{ov_narrow}");
    assert!(ov_narrow.contains("按最近 5 小时的平均用量，5 小时与本周额度在重置前用不完"), "{ov_narrow}");
    assert!(!ov_narrow.contains("Preview Tier: experimental pool active"), "{ov_narrow}");
    assert!(ov_narrow.contains("近 24 小时 Token"), "{ov_narrow}");
    assert!(ov_narrow.contains("近 7 天 Token 分布"), "{ov_narrow}");
    assert!(ov_narrow.contains("近 7 天交付走势"), "{ov_narrow}");
    let ov_narrow_last = ov_narrow.lines().last().unwrap_or("");
    assert!(!ov_narrow_last.contains("v切图"), "v does nothing on narrow overview: {ov_narrow_last}");
    for bad in forbidden {
        assert!(!ov_narrow.contains(bad), "forbidden copy {bad:?} in ov_narrow:\n{ov_narrow}");
    }

    // 3. Accounts 120x40 (wide: list + quota bars + 24h Sparkline + plain estimate)
    app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Accounts);
    let acct_wide = render_to_string(&mut app, 120, 40);
    assert_lines_fit_width(&acct_wide, 120);
    assert!(acct_wide.contains("6账号(2)"), "{acct_wide}");
    assert!(acct_wide.contains("alp… · 3 分钟前更新"), "{acct_wide}");
    assert_eq!(acct_wide.matches("3 分钟前更新").count(), 1, "{acct_wide}");
    assert!(!acct_wide.contains("alpha@example.test"), "email hidden by default: {acct_wide}");
    assert!(!acct_wide.contains("主号"), "label hidden by default: {acct_wide}");
    assert!(!acct_wide.contains("近 5 小时: 12.0M"), "5h total should not repeat in detail: {acct_wide}");
    assert_eq!(acct_wide.matches("54.0M").count(), 1, "7d total should appear once: {acct_wide}");
    assert!(!acct_wide.contains("· 在跑 1"), "running count should not duplicate in list title: {acct_wide}");
    assert!(acct_wide.contains("▸ 额度"), "{acct_wide}");
    assert!(acct_wide.contains("Gemini · 5 小时"), "{acct_wide}");
    assert!(acct_wide.contains("Claude/GPT · 5 小时"), "{acct_wide}");
    assert!(acct_wide.contains("Preview Tier: experimental pool active"), "{acct_wide}");
    assert!(acct_wide.contains("按最近 5 小时的平均用量，Claude/GPT 5 小时额度在重置前用不"), "{acct_wide}");
    assert!(acct_wide.contains("按最近 24 小时的平均用量，Gemini 本周额度在重置前用不完"), "{acct_wide}");
    assert!(acct_wide.contains("手动记录: 本周剩余 95% · 5 小时剩余 90%"), "{acct_wide}");
    assert!(acct_wide.contains("近 24 小时 Token"), "{acct_wide}");
    assert!(acct_wide.contains("在跑 agy 进程: 1 个（有进程时无法切换）"), "{acct_wide}");
    for bad in forbidden {
        assert!(!acct_wide.contains(bad), "forbidden copy {bad:?} in acct_wide:\n{acct_wide}");
    }

    // 4. Accounts 80x24 (narrow text degradation, zero misalignment)
    let acct_narrow = render_to_string(&mut app, 80, 24);
    assert_lines_fit_width(&acct_narrow, 80);
    assert!(acct_narrow.contains("agy 账号 (2)"), "{acct_narrow}");
    assert!(acct_narrow.contains("alp… · 3 分钟前更新"), "{acct_narrow}");
    assert_eq!(acct_narrow.matches("3 分钟前更新").count(), 1, "{acct_narrow}");
    assert!(!acct_narrow.contains("alpha@example.test"), "email hidden by default: {acct_narrow}");
    assert!(!acct_narrow.contains("· 在跑 1"), "{acct_narrow}");
    assert!(acct_narrow.contains("▸ 额度"), "{acct_narrow}");
    assert!(acct_narrow.contains("Gemini · 5 小时  剩余 82% · 4 小时后重置"), "{acct_narrow}");
    assert!(acct_narrow.contains("按最近 5 小时的平均用量，Claude/GPT 5 小时额度在重置前用不完"), "{acct_narrow}");
    assert!(acct_narrow.contains("按最近 24 小时的平均用量，Gemini 本周额度在重置前用不完"), "{acct_narrow}");
    for bad in forbidden {
        assert!(!acct_narrow.contains(bad), "forbidden copy {bad:?} in acct_narrow:\n{acct_narrow}");
    }

    if std::env::var("PRINT_TUI_SNAPSHOTS").as_deref() == Ok("1") {
        println!("\n=== OVERVIEW 120x40 ===\n{ov_wide}");
        println!("\n=== OVERVIEW 80x24 ===\n{ov_narrow}");
        println!("\n=== ACCOUNTS 120x40 ===\n{acct_wide}");
        println!("\n=== ACCOUNTS 80x24 ===\n{acct_narrow}");
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

    // Overview shows the sooner bottleneck in one sentence (wrapped cleanly without ellipsis).
    app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Overview);
    let ov = render_to_string(&mut app, 120, 40);
    assert_lines_fit_width(&ov, 120);
    assert!(ov.contains("按最近 5 小时的平均用量，Claude/GPT 5 小时额度约 1 小时 30"), "{ov}");
    assert!(ov.contains("分钟后用完"), "{ov}");
    assert!(!ov.contains("30 分…"), "estimate should wrap rather than truncate: {ov}");

    // Accounts detail shows both windows clearly.
    app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
    let acct = render_to_string(&mut app, 120, 40);
    assert_lines_fit_width(&acct, 120);
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
    app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Work);

    // Move selection away from W001..W005 (keep on Q01 at top) so fg colors are not reversed.
    // Use 160 cols so all work counts including `老化 4` fit alongside the unified full tab names.
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

    // Top bar shows 老化 4
    assert!(rows_text[0].contains("老化 4"), "top bar: {}", rows_text[0]);

    // Check row tail markers and colors (scan right-to-left to hit the tail badge, not the title)
    let find_cell_fg = |row_needle: &str, token_first_char: &str| -> Option<Color> {
        for y in 0..buf.area.height {
            if !rows_text[y as usize].contains(row_needle) {
                continue;
            }
            for x in (0..buf.area.width).rev() {
                if buf[(x, y)].symbol() == token_first_char {
                    return Some(buf[(x, y)].fg);
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

    // When no stale items exist, top bar does NOT show 老化
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
    fresh_app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
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
            {"pid": 33057, "run": "20261124-155318-call-orphan", "cmdline": "/home/zji/.local/libexec/agy --input-format stream-json --output-format stream-json"},
            {"pid": 2896311, "cmdline": "/bin/sh /home/zji/.local/bin/agy --agent aia-lead"}
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

    // Overview: accounts masked (`gui…`, `jzy…`), zero-usage account collapsed to one line
    app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Overview);
    let ov = render_to_string(&mut app, 120, 40);
    assert!(ov.contains("gui…"), "{ov}");
    assert!(ov.contains("jzy…"), "{ov}");
    assert!(ov.contains("· jzy… 近 24 小时无用量"), "{ov}");
    assert!(!ov.contains("guimondbuchinsky"), "full account name leaked on overview:\n{ov}");
    assert!(!ov.contains("jzy896370029"), "full account name leaked on overview:\n{ov}");
    assert!(!ov.contains("Buchinsky Guimond"), "label leaked on overview:\n{ov}");

    // Accounts tab: default masked, no email column, no label, one-line procs without cmdline
    app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Accounts);
    let masked = render_to_string(&mut app, 120, 40);
    assert!(masked.contains("gui…"), "{masked}");
    assert!(masked.contains("jzy…"), "{masked}");
    assert!(!masked.contains("guimondbuchinsky"), "full id leaked:\n{masked}");
    assert!(!masked.contains("gmail.com"), "email leaked:\n{masked}");
    assert!(!masked.contains("Buchinsky Guimond"), "label leaked:\n{masked}");
    assert!(!masked.contains("邮箱"), "email column header should be removed:\n{masked}");
    assert!(masked.contains("i 全称"), "footer should show `i 全称`:\n{masked}");
    assert!(masked.contains("33057 · 20261124-155318-call-orphan"), "{masked}");
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
    let revealed = render_to_string(&mut app, 120, 40);
    assert!(revealed.contains("guimondbuchinsky"), "{revealed}");
    assert!(revealed.contains("guimondbuchinsky@gmail.com"), "{revealed}");
    assert!(revealed.contains("Buchinsky Guimond"), "{revealed}");
    assert!(revealed.contains("jzy…"), "{revealed}");
    assert!(!revealed.contains("jzy896370029"), "unselected account must stay masked:\n{revealed}");

    // Press `i` again -> restores masked default
    app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
    let remasked = render_to_string(&mut app, 120, 40);
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

    // 120x40 Overview & Accounts: bar has 1 space before `剩余`, and nothing trails after `重置`
    for tab_key in ['0', '6'] {
        app.handle_key(KeyEvent::new(KeyCode::Char(tab_key), KeyModifiers::NONE));
        let screen = render_to_string(&mut app, 120, 40);
        assert_lines_fit_width(&screen, 120);
        assert!(
            !screen.contains("█剩余") && !screen.contains("░剩余"),
            "bar must be separated from text by a space:\n{screen}"
        );
        for line in screen.lines() {
            if let Some(pos) = line.find("后重置") {
                let after = &line[pos + "后重置".len()..];
                // Before the right border `│`, there must be no stray block characters.
                let before_border = after.split('│').next().unwrap_or(after);
                assert!(
                    !before_border.chars().any(|c| ('▏'..='█').contains(&c) || c == '░'),
                    "bar overflowed after text on line: {line}"
                );
            }
        }
    }

    // 80x24 Overview & Accounts: fits cleanly within 80 cols with no overflow
    app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Overview);
    let ov80 = render_to_string(&mut app, 80, 24);
    assert_lines_fit_width(&ov80, 80);

    app.handle_key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
    assert_eq!(app.tab, ViewTab::Accounts);
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
    let screen = render_to_string(&mut app, 120, 30);

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
    app.handle_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));

    let list_screen = render_to_string(&mut app, 120, 24);
    let q01_line = list_screen.lines().find(|l| l.contains("Q01")).expect("Q01 row");
    let q02_line = list_screen.lines().find(|l| l.contains("Q02")).expect("Q02 row");
    let d001_line = list_screen.lines().find(|l| l.contains("D001")).expect("D001 row");
    assert!(q01_line.contains("缺建议"), "incomplete Q01 must show 缺建议: {q01_line}");
    assert!(!q02_line.contains("缺建议"), "complete Q02 must not show 缺建议: {q02_line}");
    assert!(!d001_line.contains("缺建议"), "direction D001 must not show 缺建议: {d001_line}");

    // Move selection to Q02 and open its card -> must display 在等它的工作 with W001 details
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let card_screen = render_to_string(&mut app, 120, 32);
    assert!(card_screen.contains("在等它的工作"), "{card_screen}");
    assert!(card_screen.contains("[W001] 落地新路由"), "{card_screen}");
    assert!(card_screen.contains("完成条件：灰度与单测全绿"), "{card_screen}");
    assert!(card_screen.contains("说明：docs/plan/routing.md"), "{card_screen}");
}
