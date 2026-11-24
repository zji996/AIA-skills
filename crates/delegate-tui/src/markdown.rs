use crate::width::{display_width, wrap_styled_line};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

const CODE_COLOR: Color = Color::Cyan;

/// Render Markdown text into wrapped `ratatui` lines, stripping syntax markers (`**`, `` ` ``,
/// `#`, `[text](url)`) and applying terminal-adaptive styles (bold, code color, underlined link text).
pub fn render_markdown(text: &str, indent: &str, max_cols: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;
    let base = Style::default().fg(Color::Reset);
    let muted = Style::default().fg(Color::DarkGray);
    let code_style = Style::default().fg(CODE_COLOR);

    let indent_seg = if indent.is_empty() {
        Vec::new()
    } else {
        vec![(indent.to_string(), base)]
    };

    let raw_lines: Vec<&str> = text.lines().collect();
    let mut idx = 0usize;
    while idx < raw_lines.len() {
        let line = raw_lines[idx].trim_end_matches('\r');
        let trimmed = line.trim();

        // Fenced code blocks (``` or ~~~)
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            idx += 1;
            continue;
        }
        if in_code_block {
            let mut prefix = indent_seg.clone();
            prefix.push(("│ ".to_string(), muted));
            let wrapped = wrap_styled_line(
                &[(line.to_string(), code_style)],
                &prefix,
                &prefix,
                max_cols.max(8),
            );
            out.extend(wrapped);
            idx += 1;
            continue;
        }

        if trimmed.is_empty() {
            out.push(Line::raw(""));
            idx += 1;
            continue;
        }

        // Markdown table: header row + `|---|` separator row + body rows
        if idx + 1 < raw_lines.len() {
            if let Some(header_cells) = parse_table_row(trimmed) {
                let sep_line = raw_lines[idx + 1].trim_end_matches('\r').trim();
                if is_table_separator(sep_line) {
                    let mut body_rows: Vec<Vec<String>> = Vec::new();
                    let mut j = idx + 2;
                    while j < raw_lines.len() {
                        let btrim = raw_lines[j].trim_end_matches('\r').trim();
                        if btrim.is_empty()
                            || btrim.starts_with("```")
                            || btrim.starts_with("~~~")
                        {
                            break;
                        }
                        if let Some(cells) = parse_table_row(btrim) {
                            body_rows.push(cells);
                            j += 1;
                        } else {
                            break;
                        }
                    }
                    out.extend(render_table(
                        &header_cells,
                        &body_rows,
                        indent,
                        max_cols.max(8),
                        base,
                        muted,
                    ));
                    idx = j;
                    continue;
                }
            }
        }

        // Horizontal rule
        if is_horizontal_rule(trimmed) {
            let rule_w = max_cols
                .saturating_sub(display_width(indent))
                .clamp(4, 24);
            out.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled("─".repeat(rule_w), muted),
            ]));
            idx += 1;
            continue;
        }

        // Headings: # / ## / ### / ####
        if let Some(heading_text) = strip_heading(trimmed) {
            let h_style = base.add_modifier(Modifier::BOLD);
            let segs = parse_inline(heading_text, h_style);
            out.extend(wrap_styled_line(
                &segs,
                &indent_seg,
                &indent_seg,
                max_cols.max(8),
            ));
            idx += 1;
            continue;
        }

        // Blockquote: > ...
        if let Some(quote_body) = trimmed.strip_prefix('>') {
            let body = quote_body.strip_prefix(' ').unwrap_or(quote_body);
            let mut prefix = indent_seg.clone();
            prefix.push(("│ ".to_string(), muted));
            let segs = parse_inline(body, muted);
            out.extend(wrap_styled_line(&segs, &prefix, &prefix, max_cols.max(8)));
            idx += 1;
            continue;
        }

        // Unordered list: leading spaces + (`- ` or `* ` or `+ `)
        if let Some((lead_spaces, item_body)) = strip_bullet(line) {
            let extra = " ".repeat(lead_spaces.min(4));
            let mut first = indent_seg.clone();
            first.push((format!("{extra}• "), muted));
            let mut cont = indent_seg.clone();
            cont.push((format!("{extra}  "), base));
            let segs = parse_inline(item_body, base);
            out.extend(wrap_styled_line(&segs, &first, &cont, max_cols.max(8)));
            idx += 1;
            continue;
        }

        // Ordered list: `1. text`
        if let Some((lead_spaces, num_prefix, item_body)) = strip_ordered_item(line) {
            let extra = " ".repeat(lead_spaces.min(4));
            let marker = format!("{extra}{num_prefix} ");
            let cont_pad = " ".repeat(display_width(&marker));
            let mut first = indent_seg.clone();
            first.push((marker, muted));
            let mut cont = indent_seg.clone();
            cont.push((cont_pad, base));
            let segs = parse_inline(item_body, base);
            out.extend(wrap_styled_line(&segs, &first, &cont, max_cols.max(8)));
            idx += 1;
            continue;
        }

        // Regular paragraph line
        let segs = parse_inline(trimmed, base);
        out.extend(wrap_styled_line(
            &segs,
            &indent_seg,
            &indent_seg,
            max_cols.max(8),
        ));
        idx += 1;
    }

    out
}

fn parse_table_row(trimmed: &str) -> Option<Vec<String>> {
    if !trimmed.contains('|') {
        return None;
    }
    let inner = trimmed
        .strip_prefix('|')
        .unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&'|') {
            chars.next();
            cur.push('|');
        } else if ch == '|' {
            cells.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    cells.push(cur.trim().to_string());
    if cells.len() < 2 {
        return None;
    }
    Some(cells)
}

fn is_table_separator(trimmed: &str) -> bool {
    let Some(cells) = parse_table_row(trimmed) else {
        return false;
    };
    cells.iter().all(|c| {
        let s = c.trim();
        let core = s.trim_start_matches(':').trim_end_matches(':');
        !core.is_empty() && core.chars().all(|ch| ch == '-')
    })
}

fn spans_display_width(segs: &[(String, Style)]) -> usize {
    segs.iter().map(|(t, _)| display_width(t)).sum()
}

fn clip_and_pad_styled(
    segs: &[(String, Style)],
    target_w: usize,
    pad_style: Style,
) -> Vec<Span<'static>> {
    if target_w == 0 {
        return Vec::new();
    }
    let total_w = spans_display_width(segs);
    let mut out: Vec<Span<'static>> = Vec::new();
    if total_w <= target_w {
        for (t, st) in segs {
            if !t.is_empty() {
                out.push(Span::styled(t.clone(), *st));
            }
        }
        if total_w < target_w {
            out.push(Span::styled(" ".repeat(target_w - total_w), pad_style));
        }
        return out;
    }
    if target_w == 1 {
        out.push(Span::styled("…", pad_style));
        return out;
    }
    let limit = target_w - 1;
    let mut used = 0usize;
    let mut last_style = pad_style;
    for (t, st) in segs {
        let mut chunk = String::new();
        for ch in t.chars() {
            let cw = ch.width().unwrap_or(0);
            if used + cw > limit {
                break;
            }
            chunk.push(ch);
            used += cw;
        }
        if !chunk.is_empty() {
            last_style = *st;
            out.push(Span::styled(chunk, *st));
        }
        if used >= limit {
            break;
        }
    }
    out.push(Span::styled("…", last_style));
    used += 1;
    if used < target_w {
        out.push(Span::styled(" ".repeat(target_w - used), pad_style));
    }
    out
}

fn render_table(
    header_cells: &[String],
    body_rows: &[Vec<String>],
    indent: &str,
    max_cols: usize,
    base: Style,
    muted: Style,
) -> Vec<Line<'static>> {
    let num_cols = body_rows
        .iter()
        .map(Vec::len)
        .fold(header_cells.len(), usize::max);
    if num_cols == 0 {
        return Vec::new();
    }

    let h_style = base.add_modifier(Modifier::BOLD);
    let parsed_header: Vec<Vec<(String, Style)>> = (0..num_cols)
        .map(|c| {
            let txt = header_cells.get(c).map(String::as_str).unwrap_or("");
            parse_inline(txt, h_style)
        })
        .collect();
    let parsed_body: Vec<Vec<Vec<(String, Style)>>> = body_rows
        .iter()
        .map(|row| {
            (0..num_cols)
                .map(|c| {
                    let txt = row.get(c).map(String::as_str).unwrap_or("");
                    parse_inline(txt, base)
                })
                .collect()
        })
        .collect();

    let mut col_widths = vec![1usize; num_cols];
    for (c, segs) in parsed_header.iter().enumerate() {
        col_widths[c] = col_widths[c].max(spans_display_width(segs));
    }
    for row in &parsed_body {
        for (c, segs) in row.iter().enumerate() {
            col_widths[c] = col_widths[c].max(spans_display_width(segs));
        }
    }

    let indent_w = display_width(indent);
    let avail = max_cols.saturating_sub(indent_w).max(8);
    let use_pipe = num_cols >= 2 && avail >= num_cols * 3 + (num_cols - 1) * 3;
    let sep = if use_pipe { " │ " } else { " " };
    let div_sep = if use_pipe { "─┼─" } else { " " };
    let sep_total = display_width(sep) * num_cols.saturating_sub(1);
    let content_budget = avail.saturating_sub(sep_total).max(num_cols);

    while col_widths.iter().sum::<usize>() > content_budget {
        let Some((widest_idx, &widest_val)) = col_widths
            .iter()
            .enumerate()
            .max_by_key(|(_, &w)| w)
        else {
            break;
        };
        if widest_val <= 2 {
            break;
        }
        col_widths[widest_idx] -= 1;
    }

    let build_row = |cells: &[Vec<(String, Style)>], pad_style: Style| -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if !indent.is_empty() {
            spans.push(Span::raw(indent.to_string()));
        }
        for (c, segs) in cells.iter().enumerate() {
            if c > 0 {
                spans.push(Span::styled(sep, muted));
            }
            spans.extend(clip_and_pad_styled(segs, col_widths[c], pad_style));
        }
        Line::from(spans)
    };

    let mut out = Vec::with_capacity(parsed_body.len() + 2);
    out.push(build_row(&parsed_header, h_style));

    // Divider line under header
    let mut div_spans: Vec<Span<'static>> = Vec::new();
    if !indent.is_empty() {
        div_spans.push(Span::raw(indent.to_string()));
    }
    for (c, &w) in col_widths.iter().enumerate() {
        if c > 0 {
            div_spans.push(Span::styled(div_sep, muted));
        }
        div_spans.push(Span::styled("─".repeat(w), muted));
    }
    out.push(Line::from(div_spans));

    for row in &parsed_body {
        out.push(build_row(row, base));
    }
    out
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    if trimmed.len() < 3 {
        return false;
    }
    let chars: Vec<char> = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    chars.len() >= 3 && chars.iter().all(|&c| c == '-' || c == '*' || c == '_')
        && chars.iter().all(|&c| c == chars[0])
}

fn strip_heading(trimmed: &str) -> Option<&str> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) {
        let rest = &trimmed[hashes..];
        if rest.starts_with(' ') || rest.is_empty() {
            return Some(rest.trim());
        }
    }
    None
}

fn strip_bullet(line: &str) -> Option<(usize, &str)> {
    let lead = line.bytes().take_while(|&b| b == b' ').count();
    let rest = &line[lead..];
    for prefix in ["- ", "* ", "+ "] {
        if let Some(body) = rest.strip_prefix(prefix) {
            return Some((lead, body.trim_start()));
        }
    }
    None
}

fn strip_ordered_item(line: &str) -> Option<(usize, &str, &str)> {
    let lead = line.bytes().take_while(|&b| b == b' ').count();
    let rest = &line[lead..];
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let after_digits = &rest[digits..];
    if let Some(body) = after_digits.strip_prefix(". ") {
        return Some((lead, &rest[..digits + 1], body.trim_start()));
    }
    None
}

/// Parse inline Markdown (`**bold**`, `__bold__`, `*italic*`, `` `code` ``, `[label](url)`)
/// into styled `(String, Style)` spans with syntax markers stripped.
pub fn parse_inline(input: &str, base: Style) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut plain = String::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;

    let flush_plain = |plain: &mut String, out: &mut Vec<(String, Style)>| {
        if !plain.is_empty() {
            out.push((std::mem::take(plain), base));
        }
    };

    while i < bytes.len() {
        // Inline code: `...` (also handles ``...``)
        if bytes[i] == b'`' {
            let tick_count = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            let delim = &input[i..i + tick_count];
            if let Some(rel_end) = input[i + tick_count..].find(delim) {
                let code_text = &input[i + tick_count..i + tick_count + rel_end];
                if !code_text.is_empty() {
                    flush_plain(&mut plain, &mut out);
                    let st = base.fg(CODE_COLOR);
                    out.push((code_text.to_string(), st));
                    i += tick_count + rel_end + tick_count;
                    continue;
                }
            }
        }

        // Markdown link: [label](url) -> render only `label` (underlined, recursively parsed)
        if bytes[i] == b'[' {
            if let Some((label, next_i)) = parse_link(input, i) {
                flush_plain(&mut plain, &mut out);
                let link_style = base.add_modifier(Modifier::UNDERLINED);
                let inner = parse_inline(label, link_style);
                if inner.is_empty() {
                    out.push((label.to_string(), link_style));
                } else {
                    out.extend(inner);
                }
                i = next_i;
                continue;
            }
        }

        // Bold: **text** or __text__
        if (input[i..].starts_with("**") || input[i..].starts_with("__")) && i + 2 < bytes.len() {
            let marker = &input[i..i + 2];
            if let Some(rel_end) = find_closing_delim(&input[i + 2..], marker) {
                let inner_text = &input[i + 2..i + 2 + rel_end];
                if !inner_text.trim().is_empty() {
                    flush_plain(&mut plain, &mut out);
                    let bold_style = base.add_modifier(Modifier::BOLD);
                    out.extend(parse_inline(inner_text, bold_style));
                    i += 2 + rel_end + 2;
                    continue;
                }
            }
        }

        // Italic: *text* (only when not followed by whitespace and closed before whitespace)
        if bytes[i] == b'*' && !input[i..].starts_with("**") {
            if let Some(next_ch) = input[i + 1..].chars().next() {
                if !next_ch.is_whitespace() {
                    if let Some(rel_end) = find_single_star_close(&input[i + 1..]) {
                        let inner_text = &input[i + 1..i + 1 + rel_end];
                        if !inner_text.is_empty() && !inner_text.ends_with(char::is_whitespace) {
                            flush_plain(&mut plain, &mut out);
                            let italic_style = base.add_modifier(Modifier::ITALIC);
                            out.extend(parse_inline(inner_text, italic_style));
                            i += 1 + rel_end + 1;
                            continue;
                        }
                    }
                }
            }
        }

        // Escaped punctuation (\*, \`, \[, etc.)
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if let Some(next_ch) = input[i + 1..].chars().next() {
                if matches!(next_ch, '*' | '`' | '[' | ']' | '_' | '\\') {
                    plain.push(next_ch);
                    i += 1 + next_ch.len_utf8();
                    continue;
                }
            }
        }

        let ch = input[i..].chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }

    flush_plain(&mut plain, &mut out);
    out
}

fn parse_link(input: &str, start: usize) -> Option<(&str, usize)> {
    let rest = &input[start + 1..];
    let mut depth = 1usize;
    let mut close_bracket = None;
    for (idx, ch) in rest.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    close_bracket = Some(idx);
                    break;
                }
            }
            _ => {}
        }
    }
    let cb = close_bracket?;
    let after_bracket = start + 1 + cb + 1;
    if !input[after_bracket..].starts_with('(') {
        return None;
    }
    let url_rest = &input[after_bracket + 1..];
    let mut paren_depth = 1usize;
    let mut close_paren = None;
    for (idx, ch) in url_rest.char_indices() {
        match ch {
            '(' => paren_depth += 1,
            ')' => {
                paren_depth -= 1;
                if paren_depth == 0 {
                    close_paren = Some(idx);
                    break;
                }
            }
            _ => {}
        }
    }
    let cp = close_paren?;
    let label = &rest[..cb];
    if label.is_empty() {
        return None;
    }
    Some((label, after_bracket + 1 + cp + 1))
}

fn find_closing_delim(s: &str, delim: &str) -> Option<usize> {
    let mut idx = 0;
    let bytes = s.as_bytes();
    while idx < bytes.len() {
        if bytes[idx] == b'`' {
            if let Some(end) = s[idx + 1..].find('`') {
                idx += 1 + end + 1;
                continue;
            }
        }
        if s[idx..].starts_with(delim) {
            return Some(idx);
        }
        let ch = s[idx..].chars().next()?;
        idx += ch.len_utf8();
    }
    None
}

fn find_single_star_close(s: &str) -> Option<usize> {
    let mut idx = 0;
    let bytes = s.as_bytes();
    while idx < bytes.len() {
        if bytes[idx] == b'`' {
            if let Some(end) = s[idx + 1..].find('`') {
                idx += 1 + end + 1;
                continue;
            }
        }
        if bytes[idx] == b'*' && !s[idx..].starts_with("**") {
            return Some(idx);
        }
        let ch = s[idx..].chars().next()?;
        idx += ch.len_utf8();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_bold_code_heading_and_links_without_raw_syntax() {
        let md = "## 核心结论\n\n**根因。** 见 [`worktree.rs`](file:///tmp/worktree.rs) 中的 `pnpm` 链接逻辑。";
        let lines = render_markdown(md, "  ", 80);
        let flat: String = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!flat.contains("**"), "raw bold markers leaked: {flat}");
        assert!(!flat.contains('`'), "raw backticks leaked: {flat}");
        assert!(!flat.contains("##"), "raw heading marker leaked: {flat}");
        assert!(!flat.contains("file:///"), "raw link URL leaked: {flat}");
        assert!(flat.contains("核心结论"));
        assert!(flat.contains("根因。"));
        assert!(flat.contains("worktree.rs"));
        assert!(flat.contains("pnpm"));

        // Verify styles are actually applied
        let heading_bold = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains("核心结论") && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(heading_bold, "heading should be bold");

        let has_code_color = lines.iter().flat_map(|l| &l.spans).any(|s| {
            s.content.contains("pnpm") && s.style.fg == Some(CODE_COLOR)
        });
        assert!(has_code_color, "inline code should use code color");
    }

    #[test]
    fn renders_markdown_table_as_aligned_columns() {
        let md = "| 序 | 任务 | 状态 |\n|---|---|---|\n| 1 | `ui.rs` 改动 | **通过** |\n| 2 | 表格对齐 | 完成 |";
        let lines = render_markdown(md, "  ", 48);
        assert_eq!(lines.len(), 4, "header + divider + 2 body rows");
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let joined = rendered.join("\n");
        assert!(!joined.contains("|---"), "raw separator leaked:\n{joined}");
        assert!(!joined.contains("**"), "raw bold leaked:\n{joined}");
        assert!(!joined.contains('`'), "raw code backticks leaked:\n{joined}");
        assert!(joined.contains("─┼─"), "expected box divider:\n{joined}");
        for line in &rendered {
            assert!(display_width(line) <= 48, "table line too wide: {line}");
        }
        // Column separator `│` should align at the same display column across header and body rows.
        let pipe_col = |s: &str| {
            let idx = s.find('│').expect("separator");
            display_width(&s[..idx])
        };
        assert_eq!(pipe_col(&rendered[0]), pipe_col(&rendered[2]), "\n{joined}");
        assert_eq!(pipe_col(&rendered[2]), pipe_col(&rendered[3]), "\n{joined}");
    }
}
