use crate::width::{display_width, wrap_styled_line};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

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

    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim();

        // Fenced code blocks (``` or ~~~)
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
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
            continue;
        }

        if trimmed.is_empty() {
            out.push(Line::raw(""));
            continue;
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
            continue;
        }

        // Blockquote: > ...
        if let Some(quote_body) = trimmed.strip_prefix('>') {
            let body = quote_body.strip_prefix(' ').unwrap_or(quote_body);
            let mut prefix = indent_seg.clone();
            prefix.push(("│ ".to_string(), muted));
            let segs = parse_inline(body, muted);
            out.extend(wrap_styled_line(&segs, &prefix, &prefix, max_cols.max(8)));
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
}
