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
        r#"{"maxHeavy":6,"heavyJobs":94,"threads":96,"budget":94,"poolTokens":88,"heavyJobsExplicit":false,"concurrencySummary":"线程 96 − 2 = 94 · 令牌 88/88 空闲 31","pool":{"target":88,"total":88,"free":31,"borrowed":57,"leases":[{"rank":1,"used":61,"share":62},{"rank":2,"used":31,"share":31}]},"local":[{"label":"demo-api: cargo test","since":"2026-11-22T10:40:00Z","running":true,"rank":1,"used":61,"share":62},{"label":"web-app: cargo build","since":"2026-11-22T10:41:00Z","running":true,"rank":2,"used":31,"share":31}],"remoteMaxHeavy":4,"remote":[]}"#,
    )
    .unwrap();
    let snap = AppSnapshot {
        repos: vec![repo_a, repo_b],
        lane,
        events: vec![],
        dag_queue: vec![],
        merges: vec![],
        candidates: vec![],
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
        repos: vec![],
        lane: LaneStatus::default(),
        events: vec![],
        dag_queue: vec![],
        merges: vec![],
        candidates: vec![],
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
        lane: LaneStatus::default(),
        events: parse_events_jsonl(raw_events),
        dag_queue: parse_dag_queue_jsonl(raw_queue),
        merges: vec![],
        candidates: vec![],
        host_load: None,
        refreshed_at: "09:06:10".into(),
        warnings: vec![],
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/home/dev/demo-api"), 5);

    // Runs tab shows liveness badges ("跑命", "卡警"), detail "正在做什么", and DAG queue section!
    let runs_screen = render_to_string(&mut app, 140, 32);
    assert!(runs_screen.contains("跑命"), "{runs_screen}");
    assert!(runs_screen.contains("卡警"), "{runs_screen}");
    assert!(runs_screen.contains("正在做什么"), "{runs_screen}");
    assert!(runs_screen.contains("确定性续派队列 (1)"), "{runs_screen}");
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
        lane: LaneStatus::default(),
        events: parse_events_jsonl(raw_events),
        dag_queue: vec![],
        merges: vec![],
        candidates: vec![],
        host_load: None,
        refreshed_at: "10:04:00".into(),
        warnings: vec![],
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
        repos: vec![],
        lane: LaneStatus::default(),
        events: parse_events_jsonl(raw_events),
        dag_queue: vec![],
        merges: parse_merges_jsonl(raw_merges),
        candidates: parse_candidates_jsonl(raw_candidates),
        host_load: None,
        refreshed_at: "11:03:00".into(),
        warnings: vec![],
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
