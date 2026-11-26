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

fn is_line_fragment(frag: &str) -> bool {
    let rest = match frag.strip_prefix('L').or_else(|| frag.strip_prefix('l')) {
        Some(r) => r,
        None => return false,
    };
    if let Some((a, b)) = rest.split_once('-') {
        let b = b.strip_prefix('L').or_else(|| b.strip_prefix('l')).unwrap_or(b);
        is_digits(a) && is_digits(b)
    } else {
        is_digits(rest)
    }
}

fn is_line_suffix(suffix: &str) -> bool {
    if let Some((a, b)) = suffix.split_once(':').or_else(|| suffix.split_once('-')) {
        is_digits(a) && is_digits(b)
    } else {
        is_digits(suffix)
    }
}

fn is_known_extension(ext: &str) -> bool {
    if ext.is_empty() || ext.len() > 12 || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return false;
    }
    let lower = ext.to_ascii_lowercase();
    KNOWN_EXTENSIONS.contains(&lower.as_str())
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

/// Parse a backtick span as a repository-relative path (stripping `:line` / `#L...` suffixes).
fn parse_repo_path(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.chars().any(char::is_whitespace) {
        return None;
    }
    if raw.contains("://")
        || raw.contains("::")
        || raw.starts_with('-')
        || raw.starts_with('/')
        || raw.starts_with('~')
        || raw.starts_with("..")
    {
        return None;
    }

    let mut s = raw;
    if let Some((base, frag)) = s.split_once('#') {
        if !is_line_fragment(frag) {
            return None;
        }
        s = base;
    }
    if let Some((base, suffix)) = s.split_once(':') {
        if !is_line_suffix(suffix) {
            return None;
        }
        s = base;
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
        Some(s.to_string())
    } else {
        let (stem, ext) = s.rsplit_once('.')?;
        if stem.trim_start_matches('.').is_empty() {
            return None;
        }
        if !is_known_extension(ext) {
            return None;
        }
        Some(s.to_string())
    }
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

/// Structured result from static read-only brief preflight checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    pub paths: Vec<String>,
    pub missing_paths: Vec<String>,
    pub has_symbol_hints: bool,
    pub diagnostics: Vec<String>,
}

/// Run static read-only brief checks in `repo_root` and return structured details + diagnostics.
pub fn check_detailed(prompt: &str, repo_root: &Path) -> CheckReport {
    let extracted = extract(prompt);
    let mut lines = Vec::new();
    let mut missing_paths = Vec::new();
    let mut has_symbol_hints = false;

    for p in &extracted.paths {
        if !repo_root.join(p).exists() {
            missing_paths.push(p.clone());
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
                has_symbol_hints = true;
                break;
            }
            Err(e) => {
                lines.push(format!("brief: symbol {sym} search failed ({e}); skipping"));
                has_symbol_hints = true;
            }
            Ok(out) => match out.status.code() {
                Some(0) => {
                    let count = String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .count();
                    if count == 0 {
                        has_symbol_hints = true;
                        lines.push(format!("brief: symbol {sym} not found"));
                    } else if count >= wide_threshold {
                        has_symbol_hints = true;
                        lines.push(format!(
                            "brief: symbol {sym} is referenced in {count} files (brief names {brief_paths})"
                        ));
                    }
                }
                Some(1) => {
                    has_symbol_hints = true;
                    lines.push(format!("brief: symbol {sym} not found"));
                }
                other => {
                    let code = other
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".to_string());
                    has_symbol_hints = true;
                    lines.push(format!(
                        "brief: symbol {sym} search failed (rg exit {code}); skipping"
                    ));
                }
            },
        }
    }

    CheckReport {
        paths: extracted.paths,
        missing_paths,
        has_symbol_hints,
        diagnostics: lines,
    }
}

/// Run static read-only brief checks in `repo_root` and return diagnostic lines.
pub fn check(prompt: &str, repo_root: &Path) -> Vec<String> {
    check_detailed(prompt, repo_root).diagnostics
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
}
