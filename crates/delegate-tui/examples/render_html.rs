use delegate_tui::app::App;
use delegate_tui::data::{discover_repos, home_dir, DelegateClient};
use delegate_tui::ui;
use delegate_tui::width::display_width;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy)]
struct Theme {
    outer_bg: &'static str,
    bg: &'static str,
    fg: &'static str,
    border: &'static str,
    muted: &'static str,
    red: &'static str,
    green: &'static str,
    yellow: &'static str,
    cyan: &'static str,
    shadow: &'static str,
}

const DARK_THEME: Theme = Theme {
    outer_bg: "#0b101b",
    bg: "#121722",
    fg: "#e5e9f0",
    border: "#283244",
    muted: "#7c889b",
    red: "#f87171",
    green: "#4ade80",
    yellow: "#facc15",
    cyan: "#38bdf8",
    shadow: "0 12px 32px rgba(0,0,0,0.45)",
};

const LIGHT_THEME: Theme = Theme {
    outer_bg: "#e5e7eb",
    bg: "#f8fafc",
    fg: "#0f172a",
    border: "#cbd5e1",
    muted: "#64748b",
    red: "#dc2626",
    green: "#15803d",
    yellow: "#b45309",
    cyan: "#0e7490",
    shadow: "0 12px 32px rgba(15,23,42,0.12)",
};

fn color_to_css(color: Color, theme: Theme, is_bg: bool) -> String {
    match color {
        Color::Reset => {
            if is_bg {
                theme.bg.into()
            } else {
                theme.fg.into()
            }
        }
        Color::Red | Color::LightRed => theme.red.into(),
        Color::Green | Color::LightGreen => theme.green.into(),
        Color::Yellow | Color::LightYellow => theme.yellow.into(),
        Color::Cyan | Color::LightCyan => theme.cyan.into(),
        Color::DarkGray | Color::Gray => theme.muted.into(),
        Color::Black => theme.fg.into(),
        Color::White => theme.bg.into(),
        Color::Blue | Color::LightBlue => theme.cyan.into(),
        Color::Magenta | Color::LightMagenta => theme.yellow.into(),
        Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
        Color::Indexed(n) => format!("rgb({n},{n},{n})"),
    }
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn buffer_to_html(buf: &Buffer, width: u16, height: u16, title: &str, theme: Theme) -> String {
    let mut body = String::new();
    for y in 0..height {
        body.push_str("<div class=\"row\">");
        let mut x = 0u16;
        while x < width {
            let cell = &buf[(x, y)];
            let sym = cell.symbol();
            let step = display_width(sym).max(1) as u16;
            let reversed = cell.modifier.contains(Modifier::REVERSED);
            let mut fg = color_to_css(cell.fg, theme, false);
            let mut bg = color_to_css(cell.bg, theme, true);
            if reversed {
                std::mem::swap(&mut fg, &mut bg);
            }
            let bold = cell.modifier.contains(Modifier::BOLD);
            let italic = cell.modifier.contains(Modifier::ITALIC);
            let underlined = cell.modifier.contains(Modifier::UNDERLINED);
            let cls = if step == 2 { "c w2" } else { "c w1" };
            let mut style = format!("color:{fg};");
            if bg != theme.bg {
                style.push_str(&format!("background:{bg};"));
            }
            if bold {
                style.push_str("font-weight:700;");
            }
            if italic {
                style.push_str("font-style:italic;");
            }
            if underlined {
                style.push_str("text-decoration:underline;");
            }
            body.push_str(&format!(
                "<span class=\"{cls}\" style=\"{style}\">{}</span>",
                escape_html(sym)
            ));
            x = x.saturating_add(step);
        }
        body.push_str("</div>");
    }

    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
         <style>\
         *{{box-sizing:border-box;margin:0;padding:0;}}\
         body{{background:{outer_bg};display:inline-block;padding:12px;}}\
         .term{{background:{bg};border:1px solid {border};border-radius:12px;padding:12px 14px;\
                box-shadow:{shadow};display:inline-block;\
                font-family:'Noto Sans Mono CJK SC','DejaVu Sans Mono',monospace;\
                font-size:14px;line-height:19px;color:{fg};}}\
         .row{{display:flex;height:19px;white-space:pre;}}\
         .c{{display:inline-flex;align-items:center;justify-content:flex-start;height:19px;overflow:visible;flex-shrink:0;}}\
         .w1{{width:8.5px;}}\
         .w2{{width:17px;}}\
         </style></head><body><div class=\"term\">{body}</div></body></html>",
        outer_bg = theme.outer_bg,
        bg = theme.bg,
        border = theme.border,
        shadow = theme.shadow,
        fg = theme.fg,
    )
}

fn render_html(app: &mut App, width: u16, height: u16, title: &str, theme: Theme) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test backend");
    terminal.draw(|f| ui::render(f, app)).expect("draw");
    buffer_to_html(terminal.backend().buffer(), width, height, title, theme)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let out_dir = PathBuf::from(
        args.first()
            .map(String::as_str)
            .unwrap_or(".local/test-artifacts/delegate-tui"),
    );
    let delegate_override = args.get(1).map(PathBuf::from);
    fs::create_dir_all(&out_dir).expect("create out_dir");

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = home_dir();
    let root = home.clone().unwrap_or_else(|| cwd.clone());
    let repos = discover_repos(&root, Some(&cwd));
    let client = DelegateClient::discover(delegate_override);
    let snapshot = client.collect_snapshot(&repos, home.as_deref(), Some(&cwd));

    let mut app = App::new(snapshot, client.bin.clone(), cwd, 5);
    if let Some(run) = app.selected_run().cloned() {
        if run.has_result {
            let res = client.fetch_result(&run.repo_path, &run.run);
            app.result_cache.insert(run.cache_key(), res);
        }
    }

    let wide = render_html(&mut app, 160, 42, "delegate-tui wide dark 160x42", DARK_THEME);
    fs::write(out_dir.join("wide-dark-160x42.html"), wide).expect("write wide");

    let narrow = render_html(&mut app, 80, 30, "delegate-tui narrow 80x30", DARK_THEME);
    fs::write(out_dir.join("narrow-80x30.html"), narrow).expect("write narrow");

    let light = render_html(&mut app, 148, 38, "delegate-tui light 148x38", LIGHT_THEME);
    fs::write(out_dir.join("light-148x38.html"), light).expect("write light");

    println!(
        "Generated HTML snapshots in {} (repos: {}, rows: {})",
        out_dir.display(),
        app.snapshot.repos.len(),
        app.rows.len()
    );
}
