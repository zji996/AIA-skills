use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display column width of a string (CJK full-width chars count as 2).
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Sanitize control characters / newlines for single-line rendering.
pub fn single_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_control() || ch == '\t' {
            if !last_space && !out.is_empty() {
                out.push(' ');
                last_space = true;
            }
        } else {
            last_space = ch == ' ';
            out.push(ch);
        }
    }
    out.trim().to_string()
}

/// Truncate a string to fit within `max_cols` terminal cells, appending `…` if truncated.
pub fn truncate(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    if max_cols == 1 {
        return "…".to_string();
    }
    let target = max_cols - 1;
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > target {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Truncate or right-pad a string to exactly `cols` terminal cells.
pub fn pad_right(s: &str, cols: usize) -> String {
    let clipped = truncate(s, cols);
    let w = display_width(&clipped);
    if w >= cols {
        clipped
    } else {
        format!("{clipped}{}", " ".repeat(cols - w))
    }
}

/// Truncate or left-pad a string to exactly `cols` terminal cells.
pub fn pad_left(s: &str, cols: usize) -> String {
    let clipped = truncate(s, cols);
    let w = display_width(&clipped);
    if w >= cols {
        clipped
    } else {
        format!("{}{clipped}", " ".repeat(cols - w))
    }
}

#[derive(Debug, Clone)]
struct StyledPiece {
    text: String,
    style: Style,
    is_space: bool,
    width: usize,
}

fn is_cjk_breakable(ch: char) -> bool {
    ch.width().unwrap_or(0) >= 2
}

/// Tokenize a styled segment into smart-wrappable pieces:
/// - spaces are separate pieces (dropped when wrapping to a new line)
/// - `/` ends the current path segment so breaks happen cleanly after `/`
/// - CJK full-width characters can break between characters
/// - ASCII/Latin words stay intact unless wider than `max_cols`
fn tokenize_styled(text: &str, style: Style, out: &mut Vec<StyledPiece>) {
    let mut buf = String::new();
    let flush_word = |buf: &mut String, out: &mut Vec<StyledPiece>| {
        if !buf.is_empty() {
            let t = std::mem::take(buf);
            let w = display_width(&t);
            out.push(StyledPiece {
                text: t,
                style,
                is_space: false,
                width: w,
            });
        }
    };

    for ch in text.chars() {
        if ch == '\t' {
            flush_word(&mut buf, out);
            out.push(StyledPiece {
                text: "  ".to_string(),
                style,
                is_space: true,
                width: 2,
            });
        } else if ch == ' ' {
            flush_word(&mut buf, out);
            out.push(StyledPiece {
                text: " ".to_string(),
                style,
                is_space: true,
                width: 1,
            });
        } else if ch.is_control() {
            continue;
        } else if ch == '/' {
            buf.push(ch);
            flush_word(&mut buf, out);
        } else if is_cjk_breakable(ch) {
            flush_word(&mut buf, out);
            let s = ch.to_string();
            let w = display_width(&s);
            out.push(StyledPiece {
                text: s,
                style,
                is_space: false,
                width: w,
            });
        } else {
            buf.push(ch);
        }
    }
    flush_word(&mut buf, out);
}

fn push_span_merged(spans: &mut Vec<Span<'static>>, text: String, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.style == style {
            last.content.to_mut().push_str(&text);
            return;
        }
    }
    spans.push(Span::styled(text, style));
}

/// Wrap styled segments (representing a single logical line without `\n`) cleanly at spaces, `/`,
/// and CJK boundaries, prepending `first_prefix` on line 0 and `cont_prefix` on continuation lines.
pub fn wrap_styled_line(
    segments: &[(String, Style)],
    first_prefix: &[(String, Style)],
    cont_prefix: &[(String, Style)],
    max_cols: usize,
) -> Vec<Line<'static>> {
    if max_cols == 0 {
        return vec![Line::raw("")];
    }

    let first_pw: usize = first_prefix.iter().map(|(t, _)| display_width(t)).sum();
    let cont_pw: usize = cont_prefix.iter().map(|(t, _)| display_width(t)).sum();

    let mut pieces = Vec::new();
    for (text, style) in segments {
        tokenize_styled(text, *style, &mut pieces);
    }

    let make_prefix = |is_first: bool| -> (Vec<Span<'static>>, usize) {
        let src = if is_first { first_prefix } else { cont_prefix };
        let pw = if is_first { first_pw } else { cont_pw };
        let spans = src
            .iter()
            .filter(|(t, _)| !t.is_empty())
            .map(|(t, s)| Span::styled(t.clone(), *s))
            .collect();
        (spans, pw)
    };

    if pieces.is_empty() {
        let (spans, _) = make_prefix(true);
        return vec![Line::from(spans)];
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let (mut cur_spans, mut cur_w) = make_prefix(true);
    let mut line_has_content = false;

    for piece in pieces {
        if piece.is_space {
            if !line_has_content {
                // Preserve intentional leading spaces only on the very first line before wrapping.
                if lines.is_empty() && cur_w + piece.width <= max_cols {
                    push_span_merged(&mut cur_spans, piece.text, piece.style);
                    cur_w += piece.width;
                }
                continue;
            }
            if cur_w + piece.width <= max_cols {
                push_span_merged(&mut cur_spans, piece.text, piece.style);
                cur_w += piece.width;
            } else {
                lines.push(Line::from(cur_spans));
                let (next_spans, next_pw) = make_prefix(false);
                cur_spans = next_spans;
                cur_w = next_pw;
                line_has_content = false;
            }
            continue;
        }

        // Non-space token: if it does not fit on the current line and we already have content,
        // move it intact to the next line (avoiding breaking English words or path segments).
        if cur_w + piece.width > max_cols && line_has_content {
            trim_trailing_spaces(&mut cur_spans);
            lines.push(Line::from(cur_spans));
            let (next_spans, next_pw) = make_prefix(false);
            cur_spans = next_spans;
            cur_w = next_pw;
            line_has_content = false;
        }

        // If a single token fits in the remaining width, append it directly; otherwise hard-split.
        if piece.width <= max_cols.saturating_sub(cur_w) {
            push_span_merged(&mut cur_spans, piece.text, piece.style);
            cur_w += piece.width;
            line_has_content = true;
            continue;
        }

        let mut chunk = String::new();
        let mut chunk_w = 0usize;
        for ch in piece.text.chars() {
            let cw = ch.width().unwrap_or(0);
            if cw > 0 && cur_w + chunk_w + cw > max_cols && (line_has_content || !chunk.is_empty()) {
                if !chunk.is_empty() {
                    push_span_merged(&mut cur_spans, std::mem::take(&mut chunk), piece.style);
                }
                lines.push(Line::from(cur_spans));
                let (next_spans, next_pw) = make_prefix(false);
                cur_spans = next_spans;
                cur_w = next_pw;
                chunk_w = 0;
                line_has_content = false;
            }
            chunk.push(ch);
            chunk_w += cw;
        }
        if !chunk.is_empty() {
            push_span_merged(&mut cur_spans, chunk, piece.style);
            cur_w += chunk_w;
            line_has_content = true;
        }
    }

    trim_trailing_spaces(&mut cur_spans);
    lines.push(Line::from(cur_spans));
    lines
}

fn trim_trailing_spaces(spans: &mut Vec<Span<'static>>) {
    while let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end_matches(' ');
        if trimmed.is_empty() {
            spans.pop();
        } else {
            if trimmed.len() != last.content.len() {
                let s = trimmed.to_string();
                last.content = s.into();
            }
            break;
        }
    }
}

/// Smartly wrap plain text into visual lines whose display width does not exceed `max_cols`,
/// breaking at spaces, `/`, and CJK character boundaries before splitting words.
pub fn wrap_text(text: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        let clean = raw_line.trim_end_matches('\r');
        if clean.is_empty() {
            lines.push(String::new());
            continue;
        }
        let wrapped = wrap_styled_line(
            &[(clean.to_string(), Style::default())],
            &[],
            &[],
            max_cols,
        );
        for line in wrapped {
            let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            lines.push(joined);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_width_and_truncation_are_exact() {
        assert_eq!(display_width("中文abc"), 7);
        assert_eq!(truncate("中文任务名称", 6), "中文…");
        assert_eq!(display_width(&truncate("中文任务名称", 6)), 5);
        assert_eq!(display_width(&pad_right("中文任务名称", 6)), 6);
        assert_eq!(display_width(&pad_left("12m", 6)), 6);
    }

    #[test]
    fn wrap_text_respects_cjk_width() {
        let wrapped = wrap_text("中文测试ABCD一二三", 6);
        for line in &wrapped {
            assert!(display_width(line) <= 6, "line too wide: {line:?}");
        }
        assert_eq!(wrapped.join(""), "中文测试ABCD一二三");
    }

    #[test]
    fn wrap_text_breaks_at_spaces_and_slashes_not_mid_word() {
        let wrapped = wrap_text("见 crates/delegate/src/source_watch.rs:70 说明", 28);
        for line in &wrapped {
            assert!(display_width(line) <= 28, "too wide: {line:?}");
        }
        // Should break cleanly after `/` rather than splitting `source_watch.rs` mid-word.
        assert!(
            wrapped.iter().any(|l| l.contains("source_watch.rs:70")),
            "expected intact filename, got {wrapped:?}"
        );

        let eng = wrap_text("check virtualStoreDir inside pnpm", 16);
        assert!(
            eng.iter().any(|l| l.contains("virtualStoreDir")),
            "expected intact word, got {eng:?}"
        );
    }
}
