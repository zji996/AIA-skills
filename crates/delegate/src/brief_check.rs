//! Zero-token read-only static preflight check for `delegate start` / `reply` briefs.
//!
//! Scans backtick-enclosed repository-relative paths and code symbols against the
//! source workspace and reports diagnostics to stderr only. Never blocks dispatch,
//! never modifies the prompt, and never injects anything into model context.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};

const KNOWN_EXTENSIONS: &[&str] = &[
    // Code
    "rs",
    "py",
    "pyi",
    "js",
    "mjs",
    "cjs",
    "jsx",
    "ts",
    "mts",
    "cts",
    "tsx",
    "go",
    "c",
    "h",
    "cc",
    "cpp",
    "cxx",
    "hh",
    "hpp",
    "hxx",
    "java",
    "kt",
    "kts",
    "swift",
    "rb",
    "php",
    "cs",
    "fs",
    "scala",
    "zig",
    "nim",
    "lua",
    "pl",
    "pm",
    "r",
    "m",
    "mm",
    "dart",
    "ex",
    "exs",
    "erl",
    "hs",
    "ml",
    "mli",
    "clj",
    "cljs",
    "el",
    "vim",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    "sql",
    "proto",
    "graphql",
    "gql",
    "vue",
    "svelte",
    "astro",
    "sol",
    "wgsl",
    "glsl",
    "hlsl",
    "tf",
    "hcl",
    "nix",
    // Docs & styles
    "md",
    "mdx",
    "rst",
    "txt",
    "adoc",
    "tex",
    "html",
    "htm",
    "xml",
    "svg",
    "css",
    "scss",
    "sass",
    "less",
    // Config & data
    "json",
    "jsonl",
    "jsonc",
    "json5",
    "toml",
    "yaml",
    "yml",
    "ini",
    "cfg",
    "conf",
    "env",
    "properties",
    "lock",
    "csv",
    "tsv",
    "patch",
    "diff",
    "cmake",
    "mk",
    "gradle",
    "sbt",
    "gemspec",
    "podspec",
    "plist",
    "dockerfile",
    "ipynb",
    // Assets & artifacts
    "png",
    "jpg",
    "jpeg",
    "gif",
    "webp",
    "ico",
    "pdf",
    "wasm",
    "log",
    "out",
    "bin",
    "so",
    "dylib",
    "dll",
    "zip",
    "tar",
    "gz",
    "bz2",
    "xz",
    "zst",
];

const COMMON_LOWERCASE_WORDS: &[&str] = &[
    // Common English words (len >= 4)
    "about", "above", "after", "again", "also", "been", "before", "below", "between", "both",
    "case", "could", "does", "done", "each", "else", "every", "first", "from", "give", "have",
    "here", "into", "just", "keep", "last", "less", "like", "make", "more", "most", "must",
    "need", "next", "none", "only", "other", "over", "prev", "same", "second", "should", "some",
    "such", "take", "than", "that", "them", "then", "there", "they", "third", "this", "under",
    "used", "uses", "using", "very", "want", "were", "what", "when", "where", "which", "while",
    "will", "with", "would", // Programming & CLI common words
    "abort", "active", "agent", "agents", "append", "applied", "apply", "args", "array", "ascii",
    "async", "auto", "await", "base", "bash", "blocked", "board", "body", "bool", "branch",
    "break", "brief", "build", "busy", "byte", "bytes", "cache", "call", "card", "cargo", "change",
    "changes", "char", "chars", "check", "class", "clean", "cleaned", "clear", "clippy", "clone",
    "close", "closed", "cmake", "code", "color", "commit", "common", "config", "const", "continue",
    "copy", "core", "count", "crate", "create", "curl", "custom", "data", "date", "days", "dead",
    "debug", "default", "defaults", "delete", "diff", "dirty", "disk", "docs", "drop", "empty",
    "entries", "entry", "enum", "error", "errors", "event", "events", "exec", "exit", "export",
    "fail", "failed", "fake", "false", "fast", "field", "fields", "file", "files", "filter",
    "final", "find", "flag", "flags", "flush", "force", "format", "free", "fresh", "full",
    "function", "glob", "global", "grep", "handle", "hard", "hash", "head", "height", "help",
    "helper", "high", "home", "hook", "host", "hour", "hours", "html", "http", "https", "idle",
    "ignored", "impl", "import", "index", "info", "init", "inner", "input", "insert", "invalid",
    "item", "items", "join", "json", "jsonl", "kill", "kind", "label", "lane", "left", "limit",
    "line", "lines", "linux", "list", "live", "load", "local", "lock", "long", "loop", "lower",
    "macos", "main", "match", "medium", "merge", "merged", "meta", "method", "mins", "missing",
    "mock", "mode", "model", "models", "module", "move", "name", "names", "node", "note", "notes",
    "null", "number", "object", "open", "option", "options", "outer", "output", "page", "panel",
    "parse", "pass", "passed", "patch", "path", "paths", "paused", "pending", "pipe", "plan",
    "pool", "port", "print", "private", "process", "prompt", "public", "pull", "push", "pytest",
    "python", "queue", "queued", "quiet", "radar", "read", "ready", "real", "recv", "regex",
    "remove", "render", "repo", "reply", "reset", "result", "results", "retry", "return", "review",
    "right", "role", "root", "round", "rounds", "rsync", "rule", "rules", "runs", "rust", "save",
    "scout", "script", "secs", "select", "self", "send", "serde", "setup", "shared", "shell",
    "short", "show", "size", "skip", "skipped", "sleep", "slice", "slow", "sort", "spawn", "spec",
    "speed", "split", "stale", "start", "state", "static", "status", "stderr", "stdin", "stdout",
    "step", "steps", "stop", "store", "strict", "string", "strings", "struct", "stub", "super",
    "symbol", "sync", "synced", "table", "tail", "target", "task", "tasks", "temp", "test",
    "tests", "text", "tier", "tiers", "time", "timeout", "title", "token", "tokens", "toml",
    "tools", "total", "trace", "trait", "tree", "trim", "true", "type", "update", "upper", "usage",
    "user", "utf8", "utils", "valid", "value", "values", "verify", "version", "view", "void",
    "wait", "waiting", "warn", "week", "weeks", "width", "windows", "word", "words", "work",
    "write", "yaml", "yield", "zero",
];

const COMMON_UPPERCASE_TOKENS: &[&str] = &[
    "ASCII", "FAIL", "FALSE", "FIXME", "HEAD", "HTML", "HTTP", "HTTPS", "INFO", "JSON", "NONE",
    "NULL", "PASS", "STDERR", "STDIN", "STDOUT", "TODO", "TOML", "TRUE", "UTF8", "WARN", "YAML",
];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Extracted {
    pub paths: Vec<String>,
    pub symbols: Vec<String>,
}

/// Extract inline backtick-enclosed spans from Markdown, skipping fenced code blocks.
fn extract_backtick_spans(prompt: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    for line in prompt.lines() {
        let trimmed = line.trim_start();
        if let Some((fch, flen)) = fence {
            let count = trimmed.chars().take_while(|&c| c == fch).count();
            if count >= flen && trimmed[count * fch.len_utf8()..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            let count = trimmed.bytes().take_while(|&b| b == b'`').count();
            if !trimmed[count..].contains('`') {
                fence = Some(('`', count));
                continue;
            }
        } else if trimmed.starts_with("~~~") {
            let count = trimmed.bytes().take_while(|&b| b == b'~').count();
            fence = Some(('~', count));
            continue;
        }

        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'`' {
                i += 1;
                continue;
            }
            let open_start = i;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            let tick_len = i - open_start;
            let content_start = i;

            let mut found = None;
            let mut j = content_start;
            while j < bytes.len() {
                if bytes[j] != b'`' {
                    j += 1;
                    continue;
                }
                let close_start = j;
                while j < bytes.len() && bytes[j] == b'`' {
                    j += 1;
                }
                if j - close_start == tick_len {
                    found = Some((close_start, j));
                    break;
                }
            }

            if let Some((close_start, close_end)) = found {
                let inner = line[content_start..close_start].trim();
                if !inner.is_empty() {
                    spans.push(inner);
                }
                i = close_end;
            }
        }
    }

    spans
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn normalize_dashes(s: &str) -> String {
    s.replace(['–', '—', '−'], "-")
}

/// Parse `#L12`, `#L12-30`, `#L12-L30`, `#L12–L30` fragments into `(start, end)`.
fn parse_line_fragment(frag: &str) -> Option<(usize, usize)> {
    let cleaned = normalize_dashes(frag.trim());
    let rest = cleaned
        .strip_prefix('L')
        .or_else(|| cleaned.strip_prefix('l'))?;
    if let Some((a, b)) = rest.split_once('-') {
        let b = b
            .strip_prefix('L')
            .or_else(|| b.strip_prefix('l'))
            .unwrap_or(b);
        if !is_digits(a) || !is_digits(b) {
            return None;
        }
        let start = a.parse::<usize>().ok()?;
        let end = b.parse::<usize>().ok()?;
        (start > 0 && end >= start).then_some((start, end))
    } else if is_digits(rest) {
        let start = rest.parse::<usize>().ok()?;
        (start > 0).then_some((start, start))
    } else {
        None
    }
}

/// Parse `:12`, `:12-30`, `:12:5`, `:L12`, `:L12-30`, `:L12-L30`, `:1045,1118` suffixes.
fn parse_line_suffix_ranges(suffix: &str) -> Option<Vec<(usize, usize)>> {
    let cleaned = normalize_dashes(suffix.trim());
    if cleaned.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for part in cleaned.split(',') {
        let p = part.trim();
        if p.is_empty() {
            return None;
        }
        if let Some(range) = parse_line_fragment(p) {
            out.push(range);
        } else if let Some((a, b)) = p.split_once('-') {
            let b = b
                .strip_prefix('L')
                .or_else(|| b.strip_prefix('l'))
                .unwrap_or(b);
            if !is_digits(a) || !is_digits(b) {
                return None;
            }
            let start = a.parse::<usize>().ok()?;
            let end = b.parse::<usize>().ok()?;
            if start == 0 || end < start {
                return None;
            }
            out.push((start, end));
        } else if let Some((line, col)) = p.split_once(':') {
            if !is_digits(line) || !is_digits(col) {
                return None;
            }
            let start = line.parse::<usize>().ok()?;
            if start == 0 {
                return None;
            }
            out.push((start, start));
        } else if is_digits(p) {
            let start = p.parse::<usize>().ok()?;
            if start == 0 {
                return None;
            }
            out.push((start, start));
        } else {
            return None;
        }
    }
    (!out.is_empty()).then_some(out)
}

fn is_known_extension(ext: &str) -> bool {
    if ext.is_empty() || ext.len() > 12 || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return false;
    }
    let lower = ext.to_ascii_lowercase();
    KNOWN_EXTENSIONS.contains(&lower.as_str())
}

const KNOWN_EXTENSIONLESS_FILENAMES: &[&str] = &[
    "Makefile",
    "GNUmakefile",
    "Dockerfile",
    "Containerfile",
    "Justfile",
    "Procfile",
    "Rakefile",
    "Gemfile",
    "Brewfile",
    "Vagrantfile",
];

/// Returns true if the final path segment has a recognized file extension (or is a known extensionless file).
pub fn has_known_file_extension(path: &str) -> bool {
    let trimmed = path.trim_end_matches('/');
    let Some(last) = trimmed.rsplit('/').next().filter(|s| !s.is_empty()) else {
        return false;
    };
    if KNOWN_EXTENSIONLESS_FILENAMES.contains(&last) {
        return true;
    }
    let Some((stem, ext)) = last.rsplit_once('.') else {
        return false;
    };
    !stem.trim_start_matches('.').is_empty() && is_known_extension(ext)
}

fn has_disallowed_path_char(s: &str) -> bool {
    s.chars().any(|c| {
        c.is_control()
            || c.is_whitespace()
            || matches!(
                c,
                '*' | '?'
                    | '<'
                    | '>'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '$'
                    | '|'
                    | '\''
                    | '"'
                    | ','
                    | ';'
                    | '('
                    | ')'
                    | '='
                    | '@'
                    | '%'
                    | '!'
                    | '~'
                    | '\\'
                    | '^'
                    | '`'
                    | ':'
                    | '#'
            )
    })
}

fn strip_absolute_root<'a>(abs: &'a str, strip_roots: &[&Path]) -> Option<&'a str> {
    for root in strip_roots {
        let root_str = root.to_string_lossy();
        let prefix = root_str.trim_end_matches('/');
        if prefix.is_empty() {
            continue;
        }
        if let Some(rest) = abs.strip_prefix(prefix) {
            if let Some(rel) = rest.strip_prefix('/') {
                if !rel.is_empty() {
                    return Some(rel);
                }
            }
        }
    }
    None
}

/// Extract line ranges from `#L...` or `:line` suffixes on any target string (even if the path itself is outside repo roots).
fn extract_target_lines(raw: &str) -> Vec<(usize, usize)> {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix("file://") {
        s = rest.strip_prefix("localhost").unwrap_or(rest);
    }
    if let Some((_, frag)) = s.split_once('#') {
        if let Some(r) = parse_line_fragment(frag) {
            return vec![r];
        }
    }
    if let Some((_, suffix)) = s.rsplit_once(':') {
        if let Some(ranges) = parse_line_suffix_ranges(suffix) {
            return ranges;
        }
    }
    Vec::new()
}

/// Parse a candidate token as a repository-relative path with optional `#L...` / `:line` ranges,
/// stripping `file://` and matching `strip_roots` prefixes when present.
pub fn parse_repo_path_with_lines(
    raw: &str,
    strip_roots: &[&Path],
) -> Option<(String, Vec<(usize, usize)>)> {
    if raw.is_empty() || raw.chars().any(char::is_whitespace) {
        return None;
    }
    if raw.contains("::") || raw.starts_with('-') || raw.starts_with('~') || raw.starts_with("..") {
        return None;
    }

    let mut s = raw;
    let mut had_file_scheme = false;
    if let Some(after) = s.strip_prefix("file://") {
        had_file_scheme = true;
        s = after.strip_prefix("localhost").unwrap_or(after);
    } else if s.contains("://") {
        return None;
    }

    let mut lines = Vec::new();
    if let Some((base, frag)) = s.split_once('#') {
        let range = parse_line_fragment(frag)?;
        lines.push(range);
        s = base;
    }
    if let Some((base, suffix)) = s.split_once(':') {
        let parsed = parse_line_suffix_ranges(suffix)?;
        lines.extend(parsed);
        s = base;
    }

    if s.starts_with('/') {
        s = strip_absolute_root(s, strip_roots)?;
    } else if had_file_scheme {
        return None;
    }

    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    if s.is_empty() || s == "." || s == ".." || s.contains("//") || has_disallowed_path_char(s) {
        return None;
    }
    if Path::new(s)
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }

    if s.contains('/') {
        let trimmed = s.trim_end_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        let segments: Vec<&str> = trimmed.split('/').collect();
        if segments
            .iter()
            .any(|seg| seg.is_empty() || *seg == "." || *seg == "..")
        {
            return None;
        }
        let last = *segments.last()?;
        if !last.contains('.') {
            if segments.iter().all(|seg| is_digits(seg)) {
                return None;
            }
            if segments
                .iter()
                .all(|seg| seg.len() == 1 && seg.bytes().all(|b| b.is_ascii_alphabetic()))
            {
                return None;
            }
        }
        Some((s.to_string(), lines))
    } else {
        let (stem, ext) = s.rsplit_once('.')?;
        if stem.trim_start_matches('.').is_empty() {
            return None;
        }
        if !is_known_extension(ext) {
            return None;
        }
        Some((s.to_string(), lines))
    }
}

/// Parse a backtick span as a repository-relative path (stripping `:line` / `#L...` suffixes).
fn parse_repo_path(raw: &str) -> Option<String> {
    parse_repo_path_with_lines(raw, &[]).map(|(p, _)| p)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMention {
    pub path: String,
    pub lines: Vec<(usize, usize)>,
}

fn parse_paren_line_ranges(inside: &str) -> Vec<(usize, usize)> {
    let cleaned = normalize_dashes(inside.trim());
    if cleaned.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for piece in cleaned.split([',', '、', ';', '；']) {
        let p = piece.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(range) = parse_line_fragment(p) {
            out.push(range);
        } else if let Some((a, b)) = p.split_once('-') {
            let a = a.trim().trim_start_matches(['L', 'l']);
            let b = b.trim().trim_start_matches(['L', 'l']);
            if !is_digits(a) || !is_digits(b) {
                return Vec::new();
            }
            let (Ok(start), Ok(end)) = (a.parse::<usize>(), b.parse::<usize>()) else {
                return Vec::new();
            };
            if start == 0 || end < start {
                return Vec::new();
            }
            out.push((start, end));
        } else {
            return Vec::new();
        }
    }
    out
}

fn parse_chinese_line_ranges(rest: &str) -> Vec<(usize, usize)> {
    let Some(after_di) = rest.strip_prefix('第') else {
        return Vec::new();
    };
    let Some(hang_idx) = after_di.find('行') else {
        return Vec::new();
    };
    let inner = normalize_dashes(after_di[..hang_idx].trim());
    if inner.is_empty() {
        return Vec::new();
    }
    let inner = inner.replace(['至', '到', '~', '～'], "-");
    let mut out = Vec::new();
    for piece in inner.split([',', '、', ';', '；']) {
        let p = piece.trim();
        if p.is_empty() {
            continue;
        }
        if let Some((a, b)) = p.split_once('-') {
            let a = a.trim().trim_start_matches(['L', 'l']);
            let b = b.trim().trim_start_matches(['L', 'l']);
            if !is_digits(a) || !is_digits(b) {
                return Vec::new();
            }
            let (Ok(start), Ok(end)) = (a.parse::<usize>(), b.parse::<usize>()) else {
                return Vec::new();
            };
            if start == 0 || end < start {
                return Vec::new();
            }
            out.push((start, end));
        } else {
            let a = p.trim().trim_start_matches(['L', 'l']);
            if !is_digits(a) {
                return Vec::new();
            }
            let Ok(start) = a.parse::<usize>() else {
                return Vec::new();
            };
            if start == 0 {
                return Vec::new();
            }
            out.push((start, start));
        }
    }
    out
}

/// Parse adjacent line ranges immediately following a path token/link/backtick:
/// `(L12-30)`, `（L12–L30）`, `(12-30)`, `(L12, L40-50)`, `第 12-30 行`,
/// optionally after a ` :: \`sym\` ` connector on the same line.
fn parse_adjacent_line_ranges(after: &str) -> Vec<(usize, usize)> {
    let mut s = after.trim_start_matches(['*', '_']).trim_start();
    if let Some(after_colons) = s.strip_prefix("::") {
        let mut sym = after_colons.trim_start();
        // Skip symbol list up to `(` / `（` / `第` (stopping at sentence breaks).
        if let Some(stop) = sym.find(['(', '（', '第', '—', '\n']) {
            if !sym[..stop].contains('—') {
                sym = &sym[stop..];
            }
        }
        s = sym.trim_start();
    }
    if s.starts_with('(') || s.starts_with('（') {
        let open_len = s.chars().next().map(char::len_utf8).unwrap_or(1);
        let after_open = &s[open_len..];
        if let Some(close_idx) = after_open.find([')', '）']) {
            let inside = &after_open[..close_idx];
            let ranges = parse_paren_line_ranges(inside);
            if !ranges.is_empty() {
                return ranges;
            }
        }
    }
    if s.starts_with('第') {
        let ranges = parse_chinese_line_ranges(s);
        if !ranges.is_empty() {
            return ranges;
        }
    }
    Vec::new()
}

fn push_mention(
    mentions: &mut Vec<PathMention>,
    index: &mut std::collections::HashMap<String, usize>,
    path: String,
    lines: Vec<(usize, usize)>,
) {
    if let Some(&idx) = index.get(&path) {
        let entry = &mut mentions[idx];
        for r in lines {
            if !entry.lines.contains(&r) {
                entry.lines.push(r);
            }
        }
    } else {
        let mut dedup_lines = Vec::new();
        for r in lines {
            if !dedup_lines.contains(&r) {
                dedup_lines.push(r);
            }
        }
        index.insert(path.clone(), mentions.len());
        mentions.push(PathMention {
            path,
            lines: dedup_lines,
        });
    }
}

fn is_bare_path_byte(b: u8, next: Option<u8>) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'/' | b'_' | b'-' | b'.' | b'+' | b':' | b'#' | b'?' | b'<' | b'>' | b'{' | b'}'
        )
        || (b == b'*' && next != Some(b'*'))
}

fn trim_bare_token(raw: &str) -> &str {
    let mut s = raw;
    loop {
        let prev = s;
        if let Some(stripped) = s.strip_suffix('.') {
            s = stripped;
            continue;
        }
        if let Some(stripped) = s.strip_suffix(':') {
            s = stripped;
            continue;
        }
        if let Some(stripped) = s.strip_suffix('#') {
            s = stripped;
            continue;
        }
        if let Some(stripped) = s.strip_suffix('-') {
            s = stripped;
            continue;
        }
        if s == prev {
            break;
        }
    }
    s
}

/// Loosely scan repository-relative path mentions (with optional line ranges) from Markdown/prose text,
/// skipping fenced code blocks.
///
/// Unified scanner shared by `brief_check`, `launch` area matching, and `scout_first` prose map reading.
pub fn scan_path_mentions(text: &str, strip_roots: &[&Path]) -> Vec<PathMention> {
    let mut mentions: Vec<PathMention> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut fence: Option<(char, usize)> = None;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some((fch, flen)) = fence {
            let count = trimmed.chars().take_while(|&c| c == fch).count();
            if count >= flen && trimmed[count * fch.len_utf8()..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            let count = trimmed.bytes().take_while(|&b| b == b'`').count();
            if !trimmed[count..].contains('`') {
                fence = Some(('`', count));
                continue;
            }
        } else if trimmed.starts_with("~~~") {
            let count = trimmed.bytes().take_while(|&b| b == b'~').count();
            fence = Some(('~', count));
            continue;
        }

        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // 1. Markdown link: `[label](target)` or `` [`label`](target) ``
            if bytes[i] == b'[' {
                if let Some((label, target, link_end)) = parse_md_link(line, i) {
                    let t_clean = target.trim();
                    let is_web = t_clean.starts_with("http://")
                        || t_clean.starts_with("https://")
                        || t_clean.starts_with("mailto:")
                        || t_clean.starts_with('#');
                    if !is_web {
                        let resolved = parse_repo_path_with_lines(t_clean, strip_roots).or_else(|| {
                            let label_clean = label.trim().trim_matches('`').trim();
                            let (p, mut l_lines) =
                                parse_repo_path_with_lines(label_clean, strip_roots)?;
                            let t_lines = extract_target_lines(t_clean);
                            if l_lines.is_empty() {
                                l_lines = t_lines;
                            } else {
                                for r in t_lines {
                                    if !l_lines.contains(&r) {
                                        l_lines.push(r);
                                    }
                                }
                            }
                            Some((p, l_lines))
                        });
                        if let Some((path, mut lines)) = resolved {
                            lines.extend(parse_adjacent_line_ranges(&line[link_end..]));
                            push_mention(&mut mentions, &mut index, path, lines);
                        }
                    }
                    i = link_end;
                    continue;
                }
            }

            // 2. Inline backtick span: `` `...` ``
            if bytes[i] == b'`' {
                let open_start = i;
                while i < bytes.len() && bytes[i] == b'`' {
                    i += 1;
                }
                let tick_len = i - open_start;
                let content_start = i;
                let mut found = None;
                let mut j = content_start;
                while j < bytes.len() {
                    if bytes[j] != b'`' {
                        j += 1;
                        continue;
                    }
                    let close_start = j;
                    while j < bytes.len() && bytes[j] == b'`' {
                        j += 1;
                    }
                    if j - close_start == tick_len {
                        found = Some((close_start, j));
                        break;
                    }
                }
                if let Some((close_start, close_end)) = found {
                    let inner = line[content_start..close_start].trim();
                    let candidate = inner.split("::").next().unwrap_or(inner).trim();
                    if let Some((path, mut lines)) =
                        parse_repo_path_with_lines(candidate, strip_roots)
                    {
                        lines.extend(parse_adjacent_line_ranges(&line[close_end..]));
                        push_mention(&mut mentions, &mut index, path, lines);
                    }
                    i = close_end;
                }
                continue;
            }

            // 3. Skip web URLs (`http://`, `https://`, `mailto:`) so host/path isn't misread as bare repo path
            let rest = &line[i..];
            if rest.starts_with("http://")
                || rest.starts_with("https://")
                || rest.starts_with("mailto:")
            {
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && !matches!(bytes[i], b')' | b']' | b'>' | b'"' | b'\'')
                {
                    i += 1;
                }
                continue;
            }

            // 4. Bare `file://...` URL
            if rest.starts_with("file://") {
                let start = i;
                i += "file://".len();
                while i < bytes.len()
                    && bytes[i].is_ascii()
                    && !bytes[i].is_ascii_whitespace()
                    && !matches!(bytes[i], b')' | b']' | b'>' | b',' | b';' | b'"' | b'\'')
                {
                    i += 1;
                }
                let tok = trim_bare_token(&line[start..i]);
                if let Some((path, mut lines)) = parse_repo_path_with_lines(tok, strip_roots) {
                    lines.extend(parse_adjacent_line_ranges(&line[i..]));
                    push_mention(&mut mentions, &mut index, path, lines);
                }
                continue;
            }

            // 5. Bare path token
            let b = bytes[i];
            if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'.') {
                let preceded_by_tilde = i > 0 && bytes[i - 1] == b'~';
                let start = i;
                while i < bytes.len() {
                    let next = bytes.get(i + 1).copied();
                    if !is_bare_path_byte(bytes[i], next) {
                        break;
                    }
                    i += 1;
                }
                if !preceded_by_tilde {
                    let raw_tok = &line[start..i];
                    let before_scope = raw_tok.split("::").next().unwrap_or(raw_tok);
                    let tok = trim_bare_token(before_scope);
                    if let Some((path, mut lines)) = parse_repo_path_with_lines(tok, strip_roots) {
                        lines.extend(parse_adjacent_line_ranges(&line[i..]));
                        push_mention(&mut mentions, &mut index, path, lines);
                    }
                }
                continue;
            }

            let ch_len = line[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            i += ch_len;
        }
    }

    mentions
}

fn parse_md_link(line: &str, open_bracket: usize) -> Option<(&str, &str, usize)> {
    let bytes = line.as_bytes();
    let mut i = open_bracket + 1;
    let mut in_tick = false;
    let mut close_bracket = None;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => in_tick = !in_tick,
            b']' if !in_tick => {
                close_bracket = Some(i);
                break;
            }
            b'[' if !in_tick => return None,
            _ => {}
        }
        i += 1;
    }
    let cb = close_bracket?;
    if bytes.get(cb + 1) != Some(&b'(') {
        return None;
    }
    let target_start = cb + 2;
    let mut j = target_start;
    let mut depth = 1usize;
    while j < bytes.len() {
        match bytes[j] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    let label = &line[open_bracket + 1..cb];
                    let target = &line[target_start..j];
                    return Some((label, target, j + 1));
                }
            }
            b' ' | b'\t' => {
                // Allow optional Markdown title like `[x](path "title")` by taking path before whitespace
                if let Some(rel_close) = line[j..].find(')') {
                    let label = &line[open_bracket + 1..cb];
                    let target = &line[target_start..j];
                    return Some((label, target, j + rel_close + 1));
                }
                return None;
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Extract deduplicated normalized repo-relative path tokens from arbitrary text
/// (used by area matching in `launch.rs`, sharing the unified `scan_path_mentions` scanner).
pub fn extract_repo_path_tokens(text: &str, repo: &Path) -> Vec<String> {
    let mut roots: Vec<std::path::PathBuf> = vec![repo.to_path_buf()];
    if let Ok(canon) = std::fs::canonicalize(repo) {
        if canon != repo {
            roots.push(canon);
        }
    }
    let root_refs: Vec<&Path> = roots.iter().map(std::path::PathBuf::as_path).collect();
    let mut found = std::collections::BTreeSet::new();
    for m in scan_path_mentions(text, &root_refs) {
        let norm = m.path.trim_end_matches('/').to_string();
        if !norm.is_empty() {
            found.insert(norm);
        }
    }
    found.into_iter().collect()
}

fn is_ascii_ident(s: &str) -> bool {
    let mut bytes = s.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Parse a non-path backtick span as a code identifier (`foo_bar`, `FooBar`, `Foo::bar` -> last segment).
fn parse_symbol(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.chars().any(char::is_whitespace) {
        return None;
    }
    let s = raw.strip_suffix("()").unwrap_or(raw);
    let s = s.strip_prefix("::").unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    let segments: Vec<&str> = s.split("::").collect();
    if segments.iter().any(|seg| !is_ascii_ident(seg)) {
        return None;
    }
    let last = *segments.last()?;
    if last.len() < 4 || !last.bytes().any(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    if last.bytes().all(|b| b.is_ascii_lowercase()) && COMMON_LOWERCASE_WORDS.contains(&last) {
        return None;
    }
    if last
        .bytes()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        && COMMON_UPPERCASE_TOKENS.contains(&last)
    {
        return None;
    }
    Some(last.to_string())
}

/// Extract deduplicated repo-relative paths and candidate symbols from a brief.
pub fn extract(prompt: &str) -> Extracted {
    let mut paths = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut symbols = Vec::new();
    let mut seen_symbols = HashSet::new();

    for span in extract_backtick_spans(prompt) {
        if let Some(p) = parse_repo_path(span) {
            if seen_paths.insert(p.clone()) {
                paths.push(p);
            }
        } else if let Some(sym) = parse_symbol(span) {
            if seen_symbols.insert(sym.clone()) {
                symbols.push(sym);
            }
        }
    }

    Extracted { paths, symbols }
}

/// Run static read-only brief checks in `repo_root` and return diagnostic lines.
pub fn check(prompt: &str, repo_root: &Path) -> Vec<String> {
    let extracted = extract(prompt);
    let mut lines = Vec::new();

    for p in &extracted.paths {
        if !repo_root.join(p).exists() {
            lines.push(format!("brief: missing path {p}"));
        }
    }

    let brief_paths = extracted.paths.len();
    let wide_threshold = brief_paths + 5;

    for sym in &extracted.symbols {
        let output = Command::new("rg")
            .args(["-l", "-w", "-F", "--", sym, "."])
            .current_dir(repo_root)
            .stdin(Stdio::null())
            .output();
        match output {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                lines.push("brief: rg not found; skipping symbol checks".to_string());
                break;
            }
            Err(e) => {
                lines.push(format!("brief: symbol {sym} search failed ({e}); skipping"));
            }
            Ok(out) => match out.status.code() {
                Some(0) => {
                    let count = String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .count();
                    if count == 0 {
                        lines.push(format!("brief: symbol {sym} not found"));
                    } else if count >= wide_threshold {
                        lines.push(format!(
                            "brief: symbol {sym} is referenced in {count} files (brief names {brief_paths})"
                        ));
                    }
                }
                Some(1) => {
                    lines.push(format!("brief: symbol {sym} not found"));
                }
                other => {
                    let code = other
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".to_string());
                    lines.push(format!(
                        "brief: symbol {sym} search failed (rg exit {code}); skipping"
                    ));
                }
            },
        }
    }

    lines
}

/// Run the static brief preflight check and print diagnostics to stderr only.
pub fn run_and_report(prompt: &str, repo_root: &Path) {
    for line in check(prompt, repo_root) {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_paths_strips_line_suffixes_and_filters_non_paths() {
        let prompt = "\
修改 `crates/delegate/src/launch.rs:120`、`crates/delegate/src/launch.rs#L120-L150`、\
`./scripts/verify.sh`、`Cargo.toml`、`.delegate.json`、`crates/delegate/`。\n\
忽略 `fill.target`、`6.49.0`、`https://example.com/a/b.rs`、`/etc/hosts`、`~/x.rs`、\
`../escape.rs`、`--no-brief-check`、`skills/*/SKILL.md`、`11/26`、`A/B`、`.json`、`git status`。\n\
```rust\nlet x = `fenced/path.rs`;\n```\n";
        let ex = extract(prompt);
        assert_eq!(
            ex.paths,
            vec![
                "crates/delegate/src/launch.rs",
                "scripts/verify.sh",
                "Cargo.toml",
                ".delegate.json",
                "crates/delegate/",
            ]
        );
        assert!(ex.symbols.is_empty(), "unexpected symbols: {:?}", ex.symbols);
    }

    #[test]
    fn extract_symbols_handles_snake_camel_scoped_and_filters_common_words() {
        let prompt = "\
检查 `foo_bar`、`FooBar`、`Foo::bar`、`Foo::handle_request`、`Foo::reconcile`、\
`Foo::start`、`briefCheck`、`parse_launch()`、`frobnicate`。\n\
过滤 `start`、`reply`、`stderr`、`false`、`true`、`PASS`、`HEAD`、`JSON`、`abc`。\n";
        let ex = extract(prompt);
        assert!(ex.paths.is_empty(), "unexpected paths: {:?}", ex.paths);
        assert_eq!(
            ex.symbols,
            vec![
                "foo_bar",
                "FooBar",
                "handle_request",
                "reconcile",
                "briefCheck",
                "parse_launch",
                "frobnicate",
            ]
        );
    }

    #[test]
    fn scan_path_mentions_handles_markdown_links_file_urls_backticks_bare_and_line_forms() {
        let wt = Path::new("/tmp/fake-wt/repo");
        let text = "\
- [crates/delegate/src/main.rs](file:///tmp/fake-wt/repo/crates/delegate/src/main.rs#L12-L30) :: `apply`
- [`src/a.rs`](src/a.rs:15) 和 [L40-50](src/b.rs#L40-L50)
- [crates/delegate-tui/src/main.rs](file:///other/cache/delegate-tui/src/main.rs#L274-L316)
- 反引号 `src/c.rs:L10-20`、`src/d.rs:12-30`、`src/e.rs#L5-L9`、`src/f.rs` (L12, L40-50)
- 裸路径 src/g.rs（L12–L30）、src/h.rs (12-30)、src/i.rs 第 12-30 行、src/j.rs 第 8 行
- 忽略 https://example.com/a/b.rs、/etc/hosts、~/home/file.rs、skills/*/SKILL.md
```rust
let ignored = \"src/in_fence.rs\";
```
";
        let mentions = scan_path_mentions(text, &[wt]);
        let by_path: std::collections::BTreeMap<&str, &[(usize, usize)]> = mentions
            .iter()
            .map(|m| (m.path.as_str(), m.lines.as_slice()))
            .collect();

        assert_eq!(
            by_path.get("crates/delegate/src/main.rs").copied(),
            Some(&[(12, 30)][..])
        );
        assert_eq!(by_path.get("src/a.rs").copied(), Some(&[(15, 15)][..]));
        assert_eq!(by_path.get("src/b.rs").copied(), Some(&[(40, 50)][..]));
        assert_eq!(
            by_path.get("crates/delegate-tui/src/main.rs").copied(),
            Some(&[(274, 316)][..])
        );
        assert_eq!(by_path.get("src/c.rs").copied(), Some(&[(10, 20)][..]));
        assert_eq!(by_path.get("src/d.rs").copied(), Some(&[(12, 30)][..]));
        assert_eq!(by_path.get("src/e.rs").copied(), Some(&[(5, 9)][..]));
        assert_eq!(
            by_path.get("src/f.rs").copied(),
            Some(&[(12, 12), (40, 50)][..])
        );
        assert_eq!(by_path.get("src/g.rs").copied(), Some(&[(12, 30)][..]));
        assert_eq!(by_path.get("src/h.rs").copied(), Some(&[(12, 30)][..]));
        assert_eq!(by_path.get("src/i.rs").copied(), Some(&[(12, 30)][..]));
        assert_eq!(by_path.get("src/j.rs").copied(), Some(&[(8, 8)][..]));

        assert!(!by_path.contains_key("src/in_fence.rs"));
        assert!(!by_path.contains_key("example.com/a/b.rs"));
        assert!(!by_path.contains_key("etc/hosts"));
        assert!(!by_path.contains_key("home/file.rs"));
    }
}
