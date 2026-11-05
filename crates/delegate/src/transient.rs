//! Upstream outages — a stream cut mid-answer, a reset connection, a 502, a 429 Pi did not
//! retry — end an attempt without the task being wrong. The run waits for the provider to come
//! back, asks Pi whether the model answers again, and lets the same session carry on in place.
//! The recognition rules live here so a new one is added in a single place.

use crate::common::*;
use crate::runs;
use std::path::Path;

/// What every resume sends: the forked session already holds the task, the files read and the
/// edits made, so a fresh start would throw that away.
pub const PROMPT: &str = "上游连接中断了（不是你的问题，也不是任务的问题）。接着中断前的进度继续，不要从头重读，也不要重做已经完成的改动。 / The upstream connection was interrupted, not the task. Continue from the progress you already made: do not re-read what you already read or redo work you already finished.";

/// Resumes before the run falls through to the normal failure and escalation path.
pub const MAX: usize = 3;

/// Waits before each resume. `DELEGATE_RESUME_BACKOFF` replaces the whole list, in seconds and
/// with the usual `15s`/`1m` suffixes (`0` turns resuming off).
pub fn waits() -> Vec<f64> {
    setting("RESUME_BACKOFF", "15,45,120")
        .split(',')
        .filter_map(|item| seconds(item.trim()).ok())
        .take(MAX)
        .collect()
}

/// Limit for the pre-resume probe (`DELEGATE_RESUME_PROBE_TIMEOUT`, default 30s).
pub fn probe_timeout() -> f64 {
    seconds(&setting("RESUME_PROBE_TIMEOUT", "30")).unwrap_or(30.0)
}

/// The failure a resume can wait out.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// The answer stream stopped in the middle.
    Stream,
    /// No usable connection to the provider.
    Connection,
    /// A gateway in front of the model answered 502/503/504.
    Gateway,
    /// 429 throttling Pi itself did not retry.
    RateLimit,
}

/// A three-digit status code as a standalone number, so `1401` or a timestamp is not a 401.
fn status(text: &str, code: &str) -> bool {
    let digit = |c: Option<char>| c.is_some_and(|c| c.is_ascii_digit());
    text.match_indices(code).any(|(at, _)| {
        !digit(text[..at].chars().next_back()) && !digit(text[at + code.len()..].chars().next())
    })
}

fn has(text: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| text.contains(marker))
}

/// The kind of upstream failure `text` reports, or `None` when the task itself has to change.
/// Case-insensitive: Pi, rpi and the gateways in front of them word the same failure differently.
pub fn kind(text: &str) -> Option<Kind> {
    let text = text.trim().to_lowercase();
    // A gateway status is the outage itself, even when the sentence next to it says "forbidden"
    // ("HTTP 502: Upstream access forbidden"): the request never reached the model.
    if has(
        &text,
        &["bad gateway", "service unavailable", "gateway timeout"],
    ) || ["502", "503", "504"].iter().any(|code| status(&text, code))
    {
        return Some(Kind::Gateway);
    }
    // Anything the caller has to fix: waiting and continuing the same session cannot help.
    if ["400", "401", "403", "404", "422"]
        .iter()
        .any(|code| status(&text, code))
        || has(
            &text,
            &[
                "unauthorized",
                "unauthenticated",
                "invalid api key",
                "invalid_api_key",
                "invalid x-api-key",
                "incorrect api key",
                "authentication",
                "permission denied",
                "forbidden",
                "bad request",
                "invalid request",
                "invalid_request",
                "model not found",
                "no such model",
                "unknown model",
                "unsupported model",
                "does not exist",
                "insufficient_quota",
                "quota exceeded",
                "insufficient credit",
                "context length",
                "content policy",
            ],
        )
    {
        return None;
    }
    if has(
        &text,
        &[
            "response stream was interrupted",
            "stream was interrupted",
            "stream interrupted",
            "unexpected end of stream",
            "premature close",
            "socket hang up",
            // rpi (reqwest): the chunked body ends before the answer does.
            "error reading sse body",
            "error decoding response body",
            "unexpected eof",
        ],
    ) {
        return Some(Kind::Stream);
    }
    if has(
        &text,
        &[
            "econnreset",
            "connection reset",
            "econnrefused",
            "connection refused",
            "econnaborted",
            "enotfound",
            "eai_again",
            "getaddrinfo",
            "enetunreach",
            "ehostunreach",
            "network is unreachable",
            "network error",
            "fetch failed",
            "other side closed",
            "connection timed out",
            "timed out",
            "etimedout",
            "upstream connect error",
            "upstream request timeout",
            "prematurely closed",
        ],
    ) {
        return Some(Kind::Connection);
    }
    if status(&text, "429") || has(&text, &["too many requests", "rate limit", "rate_limit"]) {
        return Some(Kind::RateLimit);
    }
    None
}

/// The transient error text that ended `attempt`, ready to show the caller. `None` means the
/// attempt failed for a reason a resume cannot fix. A 429 Pi already retried with its own
/// backoff is not counted again: Pi's retry ran and did not help.
pub fn ended_by(run: &Path, attempt: i64) -> Option<String> {
    let events = runs::events(run);
    let mine = || {
        events
            .iter()
            .filter(|event| n(event, "attempt") == attempt)
            .collect::<Vec<_>>()
    };
    let mut texts = mine()
        .iter()
        .filter(|event| ["turn_error", "tool_error"].contains(&s(event, "e")))
        .map(|event| s(event, "detail").trim().to_string())
        .filter(|detail| !detail.is_empty())
        .collect::<Vec<_>>();
    // The process may report the failure only on stderr; its last lines are the most specific.
    texts.extend(
        read(run.join("stderr.log"))
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_string())
            .collect::<Vec<_>>(),
    );
    for text in texts.iter().rev() {
        let Some(found) = kind(text) else { continue };
        if found == Kind::RateLimit
            && mine().iter().any(|event| {
                s(event, "e") == "retry" && kind(s(event, "errorMessage")) == Some(found)
            })
        {
            continue;
        }
        return Some(clip(text, 300));
    }
    None
}

/// Sleep for `wait` seconds, waking early when the caller stops the run.
/// Returns whether the full wait passed.
pub fn wait(seconds: f64) -> bool {
    let until = std::time::Instant::now() + std::time::Duration::from_secs_f64(seconds);
    loop {
        if crate::lane::stopped() {
            return false;
        }
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(std::time::Duration::from_millis(200)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_outages_are_transient_and_task_problems_are_not() {
        assert_eq!(
            kind("Upstream response stream was interrupted"),
            Some(Kind::Stream)
        );
        assert_eq!(
            kind("TypeError: fetch failed: ECONNRESET"),
            Some(Kind::Connection)
        );
        assert_eq!(
            kind("HTTP 502: Upstream access forbidden"),
            Some(Kind::Gateway)
        );
        assert_eq!(kind("503 Service Unavailable"), Some(Kind::Gateway));
        assert_eq!(kind("504 Gateway Timeout"), Some(Kind::Gateway));
        assert_eq!(kind("429 Too Many Requests"), Some(Kind::RateLimit));
        assert_eq!(kind("request timed out"), Some(Kind::Connection));
        assert_eq!(
            kind("sse error: error reading sse body: error decoding response body (caused by: request or response body error; error reading a body from connection; unexpected EOF during chunk size line)"),
            Some(Kind::Stream)
        );
        // The caller has to fix these; retrying the same session would just repeat the error.
        assert_eq!(kind("401 Unauthorized: invalid api key"), None);
        assert_eq!(kind("403 Forbidden"), None);
        assert_eq!(kind("400 Bad Request: invalid_request_error"), None);
        assert_eq!(kind("404 model not found: p/typo"), None);
        assert_eq!(kind("context length exceeded"), None);
        assert_eq!(kind("the tests still fail"), None);
        // A number that only looks like a status code is not one.
        assert_eq!(kind("stopped after 1401 tokens"), None);
    }
}
