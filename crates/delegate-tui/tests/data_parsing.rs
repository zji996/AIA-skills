use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use delegate_tui::app::{build_write_command, App, AppAction, CollapseKind, ListRow, Overlay, WriteAction};
use delegate_tui::data::{
    extract_result_excerpt, parse_lane_json, parse_lane_text, parse_proc_loadavg,
    parse_proc_meminfo, parse_status_jsonl, sort_repos_for_cwd,
};
use delegate_tui::model::*;
use std::path::{Path, PathBuf};

const STATUS_FIXTURE: &str = r#"
{"worktreeDisk":{"totalBytes":100,"thresholdBytes":50}}
{"statusSummary":{"running":1,"pendingApply":1}}
{"run":"20261120-010000-auth-r1","name":"登录鉴权重构","state":"rejected","mode":"write","tier":"write","modelName":"sol","elapsedSeconds":420,"ageSeconds":7200,"changes":{"files":3,"added":80,"deleted":12},"accept":{"command":"cargo test","ok":false,"exitCode":1}}
{"run":"20261120-020000-auth-r2","name":"reply to 登录鉴权重构","parent":"20261120-010000-auth-r1","state":"rejected","mode":"write","tier":"write","modelName":"sol","elapsedSeconds":300,"ageSeconds":5400,"changes":{"files":4,"added":110,"deleted":15},"accept":{"command":"cargo test","ok":false,"exitCode":1}}
{"run":"20261120-030000-auth-r3","name":"reply to reply to 登录鉴权重构","parent":"20261120-020000-auth-r2","chainName":"登录鉴权重构","chainRound":3,"state":"delivered","finishedAt":"2026-11-20T03:10:00Z","mode":"write","tier":"write","modelName":"neon","elapsedSeconds":610,"ageSeconds":1800,"worktree":"/tmp/wt/auth-r3","files":["crates/auth/src/lib.rs","crates/auth/src/jwt.rs","tests/auth_test.rs"],"changes":{"files":3,"added":145,"deleted":22},"pendingChanges":{"files":3,"added":145,"deleted":22},"shape":{"dirs":[{"path":"crates/auth","added":110,"deleted":20},{"path":"tests","added":35,"deleted":2}],"dirsMore":1},"accept":{"command":"cargo test -p auth","ok":true,"exitCode":0},"promptProtectedPaths":["migrations/"],"result":"/tmp/r3/result.md","resultChars":920,"next":"review delegate diff 20261120-030000-auth-r3 --total, then merge with delegate apply 20261120-030000-auth-r3"}
{"run":"20261120-031500-metrics","name":"监控指标采集","state":"running","mode":"write","tier":"write","model":"agy/gemini-4-neon","elapsedSeconds":950,"ageSeconds":950,"idleSeconds":12,"turns":8,"last":"bash: cargo clippy --all-targets","files":["crates/metrics/src/collect.rs","crates/metrics/src/export.rs","README.md"]}
{"run":"20261120-032000-queue-job","name":"排队编译检查","state":"waiting","mode":"write","tier":"write","modelName":"sol","ageSeconds":180,"queue":{"reason":"model capacity (sol 2/2)","ahead":2}}
{"run":"20261120-003000-cleanup-old","name":"历史配置清理","state":"delivered","applied":true,"appliedBy":"delegate","mode":"write","modelName":"sol","elapsedSeconds":180,"ageSeconds":9600,"changes":{"files":2,"added":14,"deleted":40},"accept":{"command":"scripts/verify.sh","ok":true,"exitCode":0},"result":"/tmp/old/result.md","resultChars":300}
{"run":"20261120-031800-protect-bad","name":"越界改测试","state":"rejected","mode":"write","modelName":"neon","elapsedSeconds":240,"ageSeconds":600,"protectViolation":["tests/core_test.rs"],"accept":{"command":"make test","ok":false,"exitCode":1},"changes":{"files":2,"added":30,"deleted":5}}
{"run":"20261120-031900-ask-me","name":"架构方案确认","state":"answered","question":true,"mode":"read-only","modelName":"neon","elapsedSeconds":150,"ageSeconds":400,"result":"/tmp/ask/result.md","resultChars":500,"next":"the colleague asks for a decision; answer with delegate reply 20261120-031900-ask-me '<decision>'"}
"#;

#[test]
fn parses_status_jsonl_groups_and_collapses_chains() {
    let group = parse_status_jsonl(Path::new("/home/dev/demo-api"), "~/demo-api", STATUS_FIXTURE);
    // 8 run lines in fixture, 2 are superseded parents -> 6 collapsed heads
    assert_eq!(group.runs.len(), 6);
    assert_eq!(group.count_group(GroupKind::Pending), 3); // ask-me, protect-bad, auth-r3
    assert_eq!(group.count_group(GroupKind::Running), 1);
    assert_eq!(group.count_group(GroupKind::Queued), 1);
    assert_eq!(group.count_group(GroupKind::MergedUncleaned), 1);

    let auth = group
        .runs
        .iter()
        .find(|r| r.run == "20261120-030000-auth-r3")
        .expect("auth-r3 present");
    assert_eq!(auth.display_name, "登录鉴权重构");
    assert_eq!(auth.chain_round, 3);
    assert_eq!(auth.chain_ids.len(), 3);
    assert!(matches!(auth.accept, AcceptStatus::Passed { .. }));
    assert!(auth.has_protect_warning());
    assert!(!auth.has_protect_violation());
    assert_eq!(auth.dir_changes.len(), 2);
    assert_eq!(auth.dir_changes[0].path, "crates/auth");
    assert_eq!(auth.dir_changes[0].files, 2);
    assert_eq!(auth.dir_changes[0].added, 110);
    assert_eq!(auth.dirs_more, 1);

    let running = group
        .runs
        .iter()
        .find(|r| r.run == "20261120-031500-metrics")
        .expect("running run present");
    assert_eq!(running.model, "neon");
    assert_eq!(
        running.last_command.as_deref(),
        Some("$ cargo clippy --all-targets")
    );
    // Fallback dir aggregation from `files` when `shape` is absent
    assert_eq!(running.dir_changes.len(), 2);
    assert_eq!(running.dir_changes[0].path, "crates/metrics");
    assert_eq!(running.dir_changes[0].files, 2);

    let queued = group
        .runs
        .iter()
        .find(|r| r.run == "20261120-032000-queue-job")
        .expect("queued run present");
    assert_eq!(queued.group, GroupKind::Queued);
    assert_eq!(queued.queue_ahead, Some(2));
    assert_eq!(
        queued.queue_reason.as_deref(),
        Some("model capacity (sol 2/2)")
    );

    let bad = group
        .runs
        .iter()
        .find(|r| r.run == "20261120-031800-protect-bad")
        .expect("protect-bad present");
    assert!(bad.has_protect_violation());
    assert!(matches!(bad.accept, AcceptStatus::Failed { exit_code: Some(1), .. }));

    // Pending ordering: newest finished activity first (ask-me 250s ago, protect-bad 360s ago, auth-r3 1190s ago)
    let pending_ids: Vec<&str> = group
        .runs
        .iter()
        .filter(|r| r.group == GroupKind::Pending)
        .map(|r| r.run.as_str())
        .collect();
    assert_eq!(
        pending_ids,
        vec![
            "20261120-031900-ask-me",
            "20261120-031800-protect-bad",
            "20261120-030000-auth-r3"
        ]
    );
}

#[test]
fn sorts_repos_with_cwd_first_then_most_recent_activity() {
    let old_repo = parse_status_jsonl(
        Path::new("/tmp/repo-old"),
        "~/repo-old",
        r#"{"run":"r1","name":"old","state":"delivered","ageSeconds":300000,"elapsedSeconds":100}"#,
    );
    let active_repo = parse_status_jsonl(
        Path::new("/tmp/repo-active"),
        "~/repo-active",
        r#"{"run":"r2","name":"fresh","state":"delivered","ageSeconds":600,"elapsedSeconds":100}"#,
    );
    let cwd_repo = parse_status_jsonl(
        Path::new("/tmp/repo-cwd"),
        "~/repo-cwd",
        r#"{"run":"r3","name":"cwd-old","state":"delivered","ageSeconds":500000,"elapsedSeconds":100}"#,
    );
    let mut repos = vec![old_repo, active_repo, cwd_repo];
    sort_repos_for_cwd(&mut repos, Some(Path::new("/tmp/repo-cwd/sub")));
    let labels: Vec<&str> = repos.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, vec!["~/repo-cwd", "~/repo-active", "~/repo-old"]);
}

#[test]
fn folds_stale_pending_and_merged_by_default_and_toggles_on_enter() {
    let raw = r#"
{"run":"20261123-060000-fresh","name":"刚刚交付","state":"delivered","ageSeconds":600,"elapsedSeconds":200}
{"run":"20261120-010000-stale1","name":"三天前任务","state":"delivered","ageSeconds":260000,"elapsedSeconds":200}
{"run":"20261119-010000-stale2","name":"四天前任务","state":"rejected","ageSeconds":350000,"elapsedSeconds":200}
{"run":"20261122-120000-merged1","name":"已合任务","state":"delivered","applied":true,"ageSeconds":5000,"elapsedSeconds":200}
"#;
    let repo = parse_status_jsonl(Path::new("/tmp/demo"), "~/demo", raw);
    let snap = AppSnapshot {
        repos: vec![repo],
        refreshed_at: "10:00:00".into(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(snap, PathBuf::from("delegate"), PathBuf::from("/tmp/demo"), 5);

    // Only the fresh pending run is visible; stale pending & merged are collapsed into ToggleFold rows.
    let visible_runs: usize = app
        .rows
        .iter()
        .filter(|r| matches!(r, ListRow::Run { .. }))
        .count();
    assert_eq!(visible_runs, 1);
    assert!(app.rows.iter().any(|r| matches!(
        r,
        ListRow::ToggleFold {
            kind: CollapseKind::StalePending,
            count: 2,
            expanded: false,
            ..
        }
    )));
    assert!(app.rows.iter().any(|r| matches!(
        r,
        ListRow::ToggleFold {
            kind: CollapseKind::MergedUncleaned,
            count: 1,
            expanded: false,
            ..
        }
    )));

    // Move down to the StalePending fold row and press Enter -> expands 2 stale runs.
    assert!(app.move_selection(1));
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        AppAction::Redraw
    );
    let visible_after_expand: usize = app
        .rows
        .iter()
        .filter(|r| matches!(r, ListRow::Run { .. }))
        .count();
    assert_eq!(visible_after_expand, 3);

    // Press Enter again -> collapses back to 1.
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        AppAction::Redraw
    );
    let visible_after_collapse: usize = app
        .rows
        .iter()
        .filter(|r| matches!(r, ListRow::Run { .. }))
        .count();
    assert_eq!(visible_after_collapse, 1);
}

#[test]
fn parses_lane_both_json_and_text() {
    let json_raw = r#"{
        "maxHeavy": 2,
        "local": [
            {"label": "demo-api: cargo test", "since": "2026-11-22T20:50:42Z", "running": true},
            {"label": "web-app: pnpm build", "since": "2026-11-22T20:51:00Z", "running": false}
        ],
        "remoteMaxHeavy": 4,
        "remote": [
            {"label": "demo-api: cargo clippy", "since": "2026-11-22T20:52:00Z", "running": true}
        ]
    }"#;
    let parsed = parse_lane_json(json_raw).expect("valid lane json");
    assert_eq!(parsed.max_heavy, Some(2));
    assert_eq!(parsed.local_running(), 1);
    assert_eq!(parsed.local_queued(), 1);
    assert_eq!(parsed.remote_max_heavy, Some(4));
    assert_eq!(parsed.remote_running(), 1);
    assert_eq!(parsed.remote_queued(), 0);

    let text_raw = "\
线程 96 − 2 = 94 · 令牌 88/88 空闲 31
remote running  2026-11-22T20:50:00Z  demo-api: cargo clippy
running  2026-11-22T20:50:42Z  #1 61/62  demo-api: cargo test
queued   2026-11-22T20:51:00Z  web-app: pnpm build
";
    let parsed_text = parse_lane_text(text_raw);
    assert_eq!(parsed_text.max_heavy, None);
    assert_eq!(parsed_text.local_running(), 1);
    assert_eq!(parsed_text.local_queued(), 1);
    assert_eq!(parsed_text.remote_running(), 1);
    assert_eq!(parsed_text.leases_summary().as_deref(), Some("#1 61/62"));
}

#[test]
fn parses_proc_loadavg_meminfo_and_excerpt() {
    assert_eq!(parse_proc_loadavg("2.34 1.98 1.50 2/412 98765"), Some((23, 20, 15)));
    let mem = "MemTotal:       67108864 kB\nMemFree:        10485760 kB\nMemAvailable:   41943040 kB\n";
    // 64.0 GiB total, 24.0 GiB used
    assert_eq!(parse_proc_meminfo(mem), Some((240, 640)));

    let result_md = "第一段结论：所有接口通过。\n补充一行。\n\n第二段：改动文件清单。\n\n第三段：未验证项。\n\n第四段：多余细节。";
    let (excerpt, truncated) = extract_result_excerpt(result_md, 2, 10);
    assert!(truncated);
    assert!(excerpt.contains("第一段结论"));
    assert!(excerpt.contains("第二段"));
    assert!(!excerpt.contains("第三段"));

    assert_eq!(format_relative_time(Some(10)), "刚刚");
    assert_eq!(format_relative_time(Some(180)), "3 分钟前");
    assert_eq!(format_relative_time(Some(7200)), "2 小时前");
    assert_eq!(format_relative_time(Some(172800)), "2 天前");

    assert_eq!(diff_bar_segments(0, 0, 0, 5), (0, 0, 0));
    let (a, d, n) = diff_bar_segments(800, 200, 10, 5);
    assert_eq!(n, 0);
    assert!(a > d && a + d == 4);
    assert_eq!(lane_meter_cells(1, Some(2)), (1, 2));
    assert_eq!(lane_meter_cells(2, Some(12)), (2, 8));
}

#[test]
fn snapshot_fingerprint_skips_redraw_when_unchanged_and_preserves_selection() {
    let repo = parse_status_jsonl(Path::new("/home/dev/demo-api"), "~/demo-api", STATUS_FIXTURE);
    let snap1 = AppSnapshot {
        repos: vec![repo.clone()],
        host_load: Some(HostLoad {
            load1_tenths: 12,
            load5_tenths: 10,
            load15_tenths: 9,
            mem_used_tenths_gib: Some(200),
            mem_total_tenths_gib: Some(640),
        }),
        refreshed_at: "10:00:00".to_string(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(
        snap1.clone(),
        PathBuf::from("delegate"),
        PathBuf::from("/home/dev/demo-api"),
        5,
    );
    app.dirty = false;
    // Move to second run
    assert!(app.move_selection(1));
    let selected_id = app.selected_run().unwrap().run.clone();

    // Only refreshed_at changed -> must NOT trigger redraw
    let mut snap2 = snap1.clone();
    snap2.refreshed_at = "10:00:05".to_string();
    assert!(!app.apply_snapshot(snap2, false));

    // Lane changed -> MUST trigger redraw and keep same selected run
    let mut snap3 = snap1;
    snap3.lane.local.push(LaneEntry {
        label: "demo: test".into(),
        since: "2026-11-22T20:00:00Z".into(),
        running: true,
        remote: false,
        rank: None,
        used: None,
        share: None,
    });
    assert!(app.apply_snapshot(snap3, false));
    assert_eq!(app.selected_run().unwrap().run, selected_id);
}

#[test]
fn write_actions_only_generate_commands_and_filter_narrows_rows() {
    let repo = parse_status_jsonl(Path::new("/home/dev/demo-api"), "~/demo-api", STATUS_FIXTURE);
    let snap = AppSnapshot {
        repos: vec![repo],
        refreshed_at: "10:00:00".to_string(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(
        snap,
        PathBuf::from("/usr/local/bin/delegate"),
        PathBuf::from("/home/dev/other"),
        5,
    );

    // Press 'a' -> opens CommandModal with cd prefix (since cwd != repo)
    let action = app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    assert_eq!(action, AppAction::Redraw);
    match &app.overlay {
        Overlay::CommandModal { command, .. } => {
            assert!(command.starts_with("cd /home/dev/demo-api && "));
            assert!(command.contains("apply"));
        }
        other => panic!("expected CommandModal, got {other:?}"),
    }
    // Close modal with Esc
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.overlay, Overlay::None);

    // Filter with '/' -> "监控"
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    for c in "监控".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.selected_run().unwrap().run, "20261120-031500-metrics");

    // Verify reply command for running run uses --when-done
    let (cmd, _) = build_write_command(
        Path::new("delegate"),
        Path::new("/home/dev/demo-api"),
        app.selected_run().unwrap(),
        WriteAction::Reply,
    );
    assert!(cmd.contains("reply --when-done 20261120-031500-metrics"));
}

#[test]
fn collected_readonly_and_cleaned_runs_fold_by_default_and_exclude_from_pending() {
    let raw = r#"
{"run":"20261124-100001-write-unmerged","name":"待合写任务","mode":"write","state":"delivered","applied":false,"changes":{"files":2,"added":10,"deleted":1},"ageSeconds":300,"elapsedSeconds":60}
{"run":"20261124-100002-ro-read","name":"已读只读","mode":"read-only","state":"answered","applied":false,"resultReadAt":"2026-11-24T10:05:00Z","ageSeconds":400,"elapsedSeconds":30}
{"run":"20261124-100003-ro-unread","name":"未读只读","mode":"read-only","state":"answered","applied":false,"ageSeconds":500,"elapsedSeconds":30}
{"run":"20261124-100004-cleaned","name":"已清理","mode":"write","state":"cleaned","cleaned":true,"applied":false,"ageSeconds":600}
"#;
    let repo = parse_status_jsonl(Path::new("/home/dev/demo-api"), "~/demo-api", raw);
    assert_eq!(repo.count_group(GroupKind::Pending), 2);
    assert_eq!(repo.count_group(GroupKind::Collected), 2);

    let mut app = App::new(
        AppSnapshot {
            repos: vec![repo],
            ..AppSnapshot::default()
        },
        PathBuf::from("delegate"),
        PathBuf::from("/home/dev/demo-api"),
        5,
    );
    // Only the 2 Pending runs are visible by default; the 2 Collected runs are folded.
    let visible_runs = app
        .rows
        .iter()
        .filter(|r| matches!(r, ListRow::Run { .. }))
        .count();
    assert_eq!(visible_runs, 2);
    assert!(app.rows.iter().any(|r| matches!(
        r,
        ListRow::ToggleFold {
            kind: CollapseKind::Collected,
            count: 2,
            expanded: false,
            ..
        }
    )));
    // Move to Collected fold row and press Enter -> expands to 4 visible runs.
    assert!(app.move_selection(2));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let visible_after = app
        .rows
        .iter()
        .filter(|r| matches!(r, ListRow::Run { .. }))
        .count();
    assert_eq!(visible_after, 4);
}
