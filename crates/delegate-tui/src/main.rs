use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use delegate_tui::app::{copy_to_clipboard, App, AppAction, Overlay, ViewTab};
use delegate_tui::data::{discover_repos, home_dir, local_hms, state_dir, DelegateClient};
use delegate_tui::model::{AgyAccountsState, AppSnapshot};
use delegate_tui::ui;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::HashSet;
use std::env;
use std::ffi::CString;
use std::io::{self, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

struct CliArgs {
    repos: Vec<PathBuf>,
    root: Option<PathBuf>,
    delegate: Option<PathBuf>,
    interval: u64,
}

fn print_help() {
    println!(
        "delegate-tui {}\n\n\
         Interactive read-only TUI dashboard for delegate runs.\n\n\
         Usage:\n\
         \x20 delegate-tui [OPTIONS]\n\n\
         Options:\n\
         \x20 --repo <PATH>       Monitor specific repository (repeatable)\n\
         \x20 --root <DIR>        Scan root directory for repos (default: $HOME)\n\
         \x20 --delegate <PATH>   Path to delegate binary\n\
         \x20 --interval <SECS>   Auto-refresh interval in seconds (default: 5)\n\
         \x20 --version           Print version\n\
         \x20 -h, --help          Show this help\n\n\
         Keys:\n\
         \x20 ↑↓/jk select/scroll  0-6/O/w/A tabs  Tab switch pane  Enter full result / confirm switch\n\
         \x20 d diff --stat  / filter  r refresh  Q refresh quota  v toggle chart  a/c/s/p write cmd  y copy  ? help  q quit",
        env!("CARGO_PKG_VERSION")
    );
}

fn parse_args() -> Result<Option<CliArgs>, String> {
    let mut repos = Vec::new();
    let mut root = None;
    let mut delegate = None;
    let mut interval: u64 = 5;

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            "--version" => {
                println!("delegate-tui {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--repo" => {
                i += 1;
                let val = args.get(i).ok_or("--repo expects a directory path")?;
                repos.push(PathBuf::from(val));
            }
            "--root" => {
                i += 1;
                let val = args.get(i).ok_or("--root expects a directory path")?;
                root = Some(PathBuf::from(val));
            }
            "--delegate" => {
                i += 1;
                let val = args.get(i).ok_or("--delegate expects a binary path")?;
                delegate = Some(PathBuf::from(val));
            }
            "--interval" => {
                i += 1;
                let val = args.get(i).ok_or("--interval expects seconds")?;
                let secs: u64 = val
                    .parse()
                    .map_err(|_| format!("invalid --interval: {val}"))?;
                interval = secs.max(1);
            }
            other => {
                return Err(format!("unrecognized argument: {other} (try --help)"));
            }
        }
        i += 1;
    }

    Ok(Some(CliArgs {
        repos,
        root,
        delegate,
        interval,
    }))
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
    }
}

enum WorkerCmd {
    Refresh {
        force: bool,
    },
    FetchResult {
        cache_key: String,
        repo: PathBuf,
        run_id: String,
        title: String,
        open_overlay: bool,
    },
    FetchDiffStat {
        cache_key: String,
        repo: PathBuf,
        run_id: String,
        title: String,
    },
    SwitchAgyAccount {
        id: String,
    },
    Shutdown,
}

enum WorkerEvent {
    Snapshot {
        snapshot: Box<AppSnapshot>,
        force: bool,
    },
    ResultLoaded {
        cache_key: String,
        run_id: String,
        title: String,
        result: Result<String, String>,
        open_overlay: bool,
    },
    DiffStatLoaded {
        cache_key: String,
        run_id: String,
        title: String,
        result: Result<String, String>,
    },
    AgyAccountSwitched {
        id: String,
        result: Result<String, String>,
    },
}

fn resolve_repos(
    explicit_repos: &[PathBuf],
    explicit_root: Option<&PathBuf>,
    home: Option<&PathBuf>,
    cwd: &std::path::Path,
) -> Vec<PathBuf> {
    if !explicit_repos.is_empty() {
        return explicit_repos.to_vec();
    }
    let root = explicit_root
        .or(home)
        .cloned()
        .unwrap_or_else(|| cwd.to_path_buf());
    discover_repos(&root, Some(cwd))
}

/// Spawn a Linux `inotify` thread watching `<state>` (`events.seq`, `index.json`, `work.jsonl`).
/// Returns `(rx_notify, active)` so the UI refreshes immediately on state changes without
/// periodically spawning subprocesses when idle.
fn spawn_state_watcher(stop: Arc<AtomicBool>) -> (mpsc::Receiver<()>, bool) {
    let (tx, rx) = mpsc::channel::<()>();
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let Ok(cpath) = CString::new(dir.as_os_str().as_bytes()) else {
        return (rx, false);
    };
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return (rx, false);
    }
    let mask = libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE | libc::IN_CREATE | libc::IN_MODIFY;
    let wd = unsafe { libc::inotify_add_watch(fd, cpath.as_ptr(), mask) };
    if wd < 0 {
        unsafe {
            libc::close(fd);
        }
        return (rx, false);
    }
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut last_seq = std::fs::read_to_string(dir.join("events.seq")).unwrap_or_default();
        while !stop.load(Ordering::Relaxed) {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, 250) };
            if rc <= 0 {
                continue;
            }
            // Drain all buffered events and debounce slightly to coalesce paired writes.
            loop {
                let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(60));
            loop {
                let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break;
                }
            }
            let cur_seq = std::fs::read_to_string(dir.join("events.seq")).unwrap_or_default();
            let _ = (&last_seq, &cur_seq);
            last_seq = cur_seq;
            if tx.send(()).is_err() {
                break;
            }
        }
        unsafe {
            libc::close(fd);
        }
    });
    (rx, true)
}

enum QuotaCmd {
    Refresh,
    Shutdown,
}

fn spawn_quota_worker(
    client: DelegateClient,
) -> (
    mpsc::Sender<QuotaCmd>,
    mpsc::Receiver<Result<AgyAccountsState, String>>,
    thread::JoinHandle<()>,
) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<QuotaCmd>();
    let (tx_res, rx_res) = mpsc::channel::<Result<AgyAccountsState, String>>();
    let handle = thread::spawn(move || {
        while let Ok(cmd) = rx_cmd.recv() {
            match cmd {
                QuotaCmd::Refresh => {
                    let res = client.refresh_agy_quota();
                    if tx_res.send(res).is_err() {
                        break;
                    }
                }
                QuotaCmd::Shutdown => break,
            }
        }
    });
    (tx_cmd, rx_res, handle)
}

fn spawn_worker(
    client: DelegateClient,
    explicit_repos: Vec<PathBuf>,
    explicit_root: Option<PathBuf>,
    home: Option<PathBuf>,
    cwd: PathBuf,
) -> (
    mpsc::Sender<WorkerCmd>,
    mpsc::Receiver<WorkerEvent>,
    thread::JoinHandle<()>,
) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<WorkerCmd>();
    let (tx_ev, rx_ev) = mpsc::channel::<WorkerEvent>();

    let handle = thread::spawn(move || {
        while let Ok(cmd) = rx_cmd.recv() {
            match cmd {
                WorkerCmd::Refresh { force } => {
                    let repos = resolve_repos(
                        &explicit_repos,
                        explicit_root.as_ref(),
                        home.as_ref(),
                        &cwd,
                    );
                    let snapshot =
                        client.collect_snapshot(&repos, home.as_deref(), Some(&cwd));
                    if tx_ev
                        .send(WorkerEvent::Snapshot {
                            snapshot: Box::new(snapshot),
                            force,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                WorkerCmd::FetchResult {
                    cache_key,
                    repo,
                    run_id,
                    title,
                    open_overlay,
                } => {
                    let result = client.fetch_result(&repo, &run_id);
                    if tx_ev
                        .send(WorkerEvent::ResultLoaded {
                            cache_key,
                            run_id,
                            title,
                            result,
                            open_overlay,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                WorkerCmd::FetchDiffStat {
                    cache_key,
                    repo,
                    run_id,
                    title,
                } => {
                    let result = client.fetch_diff_stat(&repo, &run_id);
                    if tx_ev
                        .send(WorkerEvent::DiffStatLoaded {
                            cache_key,
                            run_id,
                            title,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                WorkerCmd::SwitchAgyAccount { id } => {
                    let result = client.switch_agy_account(&id);
                    if tx_ev
                        .send(WorkerEvent::AgyAccountSwitched { id, result })
                        .is_err()
                    {
                        break;
                    }
                }
                WorkerCmd::Shutdown => break,
            }
        }
    });

    (tx_cmd, rx_ev, handle)
}

fn request_selected_result(
    app: &App,
    tx: &mpsc::Sender<WorkerCmd>,
    inflight_results: &mut HashSet<String>,
    open_overlay: bool,
) {
    let Some(run) = app.selected_run() else {
        return;
    };
    let key = run.cache_key();
    if app.result_cache.contains_key(&key) {
        return;
    }
    if !run.has_result && !open_overlay {
        return;
    }
    if !open_overlay && !inflight_results.insert(key.clone()) {
        return;
    }
    inflight_results.insert(key.clone());
    let _ = tx.send(WorkerCmd::FetchResult {
        cache_key: key,
        repo: run.repo_path.clone(),
        run_id: run.run.clone(),
        title: run.display_name.clone(),
        open_overlay,
    });
}

fn request_diff_stat(
    app: &mut App,
    tx: &mpsc::Sender<WorkerCmd>,
    inflight_diffs: &mut HashSet<String>,
) {
    let Some(run) = app.selected_run().cloned() else {
        return;
    };
    let key = run.cache_key();
    if let Some(res) = app.diff_cache.get(&key).cloned() {
        open_diff_overlay_from_result(app, &run.run, &run.display_name, res);
        return;
    }
    app.overlay = Overlay::DiffStat {
        run_id: run.run.clone(),
        title: run.display_name.clone(),
        content: "加载中…".to_string(),
        scroll: 0,
    };
    app.dirty = true;
    if inflight_diffs.insert(key.clone()) {
        let _ = tx.send(WorkerCmd::FetchDiffStat {
            cache_key: key,
            repo: run.repo_path,
            run_id: run.run,
            title: run.display_name,
        });
    }
}

fn open_result_overlay_from_result(
    app: &mut App,
    run_id: &str,
    title: &str,
    res: Result<String, String>,
) {
    let content = match res {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => "结论为空".to_string(),
        Err(err) => format!("无法读取结论：\n{err}"),
    };
    app.overlay = Overlay::FullResult {
        run_id: run_id.to_string(),
        title: title.to_string(),
        content,
        scroll: 0,
    };
    app.dirty = true;
}

fn open_diff_overlay_from_result(
    app: &mut App,
    run_id: &str,
    title: &str,
    res: Result<String, String>,
) {
    let content = match res {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => "无 diff 改动".to_string(),
        Err(err) => format!("无法获取 diff --stat：\n{err}"),
    };
    app.overlay = Overlay::DiffStat {
        run_id: run_id.to_string(),
        title: title.to_string(),
        content,
        scroll: 0,
    };
    app.dirty = true;
}

/// Draw with DEC Synchronized Update (mode 2026) + ratatui cell-level diffing to avoid flicker.
fn draw_synced(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    let _ = terminal.backend_mut().write_all(b"\x1b[?2026h");
    let res = terminal.draw(|frame| ui::render(frame, app)).map(|_| ());
    let _ = terminal.backend_mut().write_all(b"\x1b[?2026l");
    let _ = terminal.backend_mut().flush();
    res
}

fn open_in_pager(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    content: &str,
) -> io::Result<()> {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), Show, LeaveAlternateScreen);

    let pager = env::var("PAGER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "less -R".to_string());
    let mut parts = pager.split_whitespace();
    if let Some(prog) = parts.next() {
        let args: Vec<&str> = parts.collect();
        if let Ok(mut child) = Command::new(prog)
            .args(&args)
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(content.as_bytes());
            }
            let _ = child.wait();
        }
    }

    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen, Hide)?;
    terminal.clear()?;
    Ok(())
}

fn main() {
    let cli = match parse_args() {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(e) => {
            eprintln!("delegate-tui: {e}");
            std::process::exit(2);
        }
    };

    if !io::stdout().is_terminal() {
        eprintln!("delegate-tui: stdout is not a TTY; run interactively in a terminal.");
        std::process::exit(1);
    }

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        prev_hook(info);
    }));

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = home_dir();
    let client = DelegateClient::discover(cli.delegate.clone());
    let quota_client = client.clone();
    let delegate_bin = client.bin.clone();

    let (tx_worker, rx_worker, worker_handle) = spawn_worker(
        client,
        cli.repos.clone(),
        cli.root.clone(),
        home,
        cwd.clone(),
    );
    let (tx_quota, rx_quota, quota_handle) = spawn_quota_worker(quota_client);

    let initial = AppSnapshot {
        refreshed_at: local_hms(),
        ..AppSnapshot::default()
    };
    let mut app = App::new(initial, delegate_bin, cwd, cli.interval);
    app.loading = true;

    let mut refresh_in_flight = true;
    let _ = tx_worker.send(WorkerCmd::Refresh { force: true });

    let _guard = match TerminalGuard::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("delegate-tui: 无法初始化终端: {e}");
            std::process::exit(1);
        }
    };
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            drop(_guard);
            eprintln!("delegate-tui: 终端初始化失败: {e}");
            std::process::exit(1);
        }
    };

    let refresh_every = Duration::from_secs(cli.interval);
    let idle_heartbeat = Duration::from_secs(cli.interval.max(60));
    let quota_refresh_every = Duration::from_secs(600);
    let stop_watcher = Arc::new(AtomicBool::new(false));
    let (rx_notify, inotify_active) = spawn_state_watcher(stop_watcher.clone());
    let mut last_refresh = Instant::now();
    let mut last_quota_refresh: Option<Instant> = None;
    let mut quota_in_flight = false;
    let mut print_on_exit: Option<String> = None;
    let mut inflight_results: HashSet<String> = HashSet::new();
    let mut inflight_diffs: HashSet<String> = HashSet::new();

    loop {
        // Trigger real quota refresh (`agy -p /usage`) when on Overview or Accounts tab:
        // once on first entry, then every 10 minutes while TUI is open (never when TUI is closed).
        let on_quota_tab = matches!(app.tab, ViewTab::Overview | ViewTab::Accounts);
        if on_quota_tab && !quota_in_flight {
            let due = match last_quota_refresh {
                None => true,
                Some(t) => t.elapsed() >= quota_refresh_every,
            };
            if due {
                quota_in_flight = true;
                app.quota_refreshing = true;
                app.dirty = true;
                last_quota_refresh = Some(Instant::now());
                let _ = tx_quota.send(QuotaCmd::Refresh);
            }
        }

        // Drain completed quota refresh results without blocking UI.
        while let Ok(qres) = rx_quota.try_recv() {
            quota_in_flight = false;
            app.quota_refreshing = false;
            last_quota_refresh = Some(Instant::now());
            match qres {
                Ok(agy_state) => {
                    app.snapshot.agy_accounts = agy_state;
                    app.last_fingerprint = app.snapshot.data_fingerprint();
                    app.rebuild_rows(None);
                    app.dirty = true;
                }
                Err(err) => {
                    app.set_toast(format!("额度刷新失败：{err}"));
                    app.dirty = true;
                }
            }
        }

        // React immediately to state directory `inotify` events (`events.seq`, `index.json`, `work.jsonl`).
        let mut notified = false;
        while rx_notify.try_recv().is_ok() {
            notified = true;
        }
        if notified && !refresh_in_flight {
            refresh_in_flight = true;
            let _ = tx_worker.send(WorkerCmd::Refresh { force: false });
            last_refresh = Instant::now();
        }

        // Drain completed background worker responses without blocking UI.
        while let Ok(ev) = rx_worker.try_recv() {
            match ev {
                WorkerEvent::Snapshot { snapshot, force } => {
                    refresh_in_flight = false;
                    last_refresh = Instant::now();
                    if app.apply_snapshot(*snapshot, force) {
                        request_selected_result(&app, &tx_worker, &mut inflight_results, false);
                    }
                }
                WorkerEvent::ResultLoaded {
                    cache_key,
                    run_id,
                    title,
                    result,
                    open_overlay,
                } => {
                    inflight_results.remove(&cache_key);
                    app.result_cache.insert(cache_key.clone(), result.clone());
                    let is_current = app
                        .selected_run()
                        .is_some_and(|r| r.cache_key() == cache_key);
                    if open_overlay && matches!(app.overlay, Overlay::FullResult { run_id: ref rid, .. } if rid == &run_id) {
                        open_result_overlay_from_result(&mut app, &run_id, &title, result);
                    } else if is_current {
                        app.dirty = true;
                    }
                }
                WorkerEvent::DiffStatLoaded {
                    cache_key,
                    run_id,
                    title,
                    result,
                } => {
                    inflight_diffs.remove(&cache_key);
                    app.diff_cache.insert(cache_key, result.clone());
                    if matches!(app.overlay, Overlay::DiffStat { run_id: ref rid, .. } if rid == &run_id) {
                        open_diff_overlay_from_result(&mut app, &run_id, &title, result);
                    }
                }
                WorkerEvent::AgyAccountSwitched { id, result } => {
                    app.switching_account = None;
                    match result {
                        Ok(msg) => {
                            app.account_switch_error = None;
                            app.set_toast(msg);
                        }
                        Err(err) => {
                            let first_line = err.lines().next().unwrap_or("切换失败").to_string();
                            app.account_switch_error = Some(err);
                            app.set_toast(format!("切换 {id} 失败：{first_line}"));
                        }
                    }
                    if !refresh_in_flight {
                        refresh_in_flight = true;
                        let _ = tx_worker.send(WorkerCmd::Refresh { force: true });
                        last_refresh = Instant::now();
                    }
                }
            }
        }

        if app.expire_toast() {
            app.dirty = true;
        }

        if app.dirty {
            if let Err(e) = draw_synced(&mut terminal, &mut app) {
                drop(_guard);
                eprintln!("delegate-tui: 绘制失败: {e}");
                std::process::exit(1);
            }
            app.dirty = false;
        }

        let until_refresh = refresh_every.saturating_sub(last_refresh.elapsed());
        let poll_timeout = until_refresh.min(Duration::from_millis(40));

        if event::poll(poll_timeout).unwrap_or(false) {
            if let Ok(ev) = event::read() {
                match ev {
                    Event::Resize(_, _) => {
                        app.dirty = true;
                    }
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        match app.handle_key(key) {
                            AppAction::None => {}
                            AppAction::Redraw => {
                                app.dirty = true;
                            }
                            AppAction::Refresh => {
                                app.result_cache.clear();
                                app.diff_cache.clear();
                                inflight_results.clear();
                                inflight_diffs.clear();
                                if !refresh_in_flight {
                                    refresh_in_flight = true;
                                    let _ = tx_worker.send(WorkerCmd::Refresh { force: true });
                                }
                                app.set_toast("刷新中…");
                                last_refresh = Instant::now();
                            }
                            AppAction::LoadSelectedResult => {
                                request_selected_result(
                                    &app,
                                    &tx_worker,
                                    &mut inflight_results,
                                    false,
                                );
                                app.dirty = true;
                            }
                            AppAction::OpenFullResult => {
                                if let Some(run) = app.selected_run().cloned() {
                                    let key = run.cache_key();
                                    if let Some(res) = app.result_cache.get(&key).cloned() {
                                        open_result_overlay_from_result(
                                            &mut app,
                                            &run.run,
                                            &run.display_name,
                                            res,
                                        );
                                    } else {
                                        app.overlay = Overlay::FullResult {
                                            run_id: run.run.clone(),
                                            title: run.display_name.clone(),
                                            content: "加载中…".to_string(),
                                            scroll: 0,
                                        };
                                        app.dirty = true;
                                        request_selected_result(
                                            &app,
                                            &tx_worker,
                                            &mut inflight_results,
                                            true,
                                        );
                                    }
                                }
                            }
                            AppAction::OpenDiffStat => {
                                request_diff_stat(&mut app, &tx_worker, &mut inflight_diffs);
                            }
                            AppAction::CopyClipboard(cmd) => {
                                let msg = copy_to_clipboard(&cmd);
                                app.set_toast(msg);
                            }
                            AppAction::OpenPager(content) => {
                                let _ = open_in_pager(&mut terminal, &content);
                                app.dirty = true;
                            }
                            AppAction::SwitchAgyAccount(id) => {
                                app.set_toast(format!("正在切换到 agy 账号 {id}…"));
                                let _ = tx_worker.send(WorkerCmd::SwitchAgyAccount { id });
                            }
                            AppAction::RefreshQuota => {
                                if !quota_in_flight {
                                    quota_in_flight = true;
                                    app.quota_refreshing = true;
                                    last_quota_refresh = Some(Instant::now());
                                    let _ = tx_quota.send(QuotaCmd::Refresh);
                                    app.set_toast("正在通过 agy -p /usage 刷新真实额度…");
                                } else {
                                    app.set_toast("额度已在刷新中…");
                                }
                            }
                            AppAction::Quit => break,
                            AppAction::QuitAndPrint(cmd) => {
                                print_on_exit = Some(cmd);
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let has_active_runs = app
            .snapshot
            .repos
            .iter()
            .flat_map(|g| &g.runs)
            .any(|r| matches!(r.state.as_str(), "running" | "starting" | "waiting"));
        let effective_interval = if inotify_active && !has_active_runs {
            idle_heartbeat
        } else {
            refresh_every
        };
        if !refresh_in_flight && last_refresh.elapsed() >= effective_interval {
            refresh_in_flight = true;
            let _ = tx_worker.send(WorkerCmd::Refresh { force: false });
            last_refresh = Instant::now();
        }
    }

    stop_watcher.store(true, Ordering::Relaxed);
    let _ = tx_worker.send(WorkerCmd::Shutdown);
    let _ = tx_quota.send(QuotaCmd::Shutdown);
    drop(_guard);
    // Avoid blocking exit if a slow subprocess is mid-flight; detach cleanly if needed.
    drop(worker_handle);
    drop(quota_handle);
    if let Some(cmd) = print_on_exit {
        println!("{cmd}");
    }
}
