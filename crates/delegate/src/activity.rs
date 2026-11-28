//! Liveness & activity classification from the tail of a run's transcript, command log,
//! stderr, and queue markers (rather than trusting only the registry state).
use crate::common::*;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const TAIL_EVENTS_BYTES: u64 = 64 * 1024;
const TAIL_TRANSCRIPT_BYTES: u64 = 32 * 1024;
const TAIL_STDERR_BYTES: u64 = 8 * 1024;

fn read_tail(path: &Path, max_bytes: u64) -> String {
    let Ok(mut f) = OpenOptions::new().read(true).open(path) else {
        return String::new();
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return String::new();
    };
    if len == 0 {
        return String::new();
    }
    let start = len.saturating_sub(max_bytes);
    if start > 0 && f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buf);
    if start > 0 {
        // Drop partial first line when seeking mid-file.
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default()
    } else {
        text.into_owned()
    }
}

fn file_mtime_epoch(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn format_short_duration(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        format!("{s}秒")
    } else if s < 3600 {
        let m = s / 60;
        let rem = s % 60;
        if rem == 0 {
            format!("{m}分")
        } else {
            format!("{m}分{rem}秒")
        }
    } else {
        let h = s / 3600;
        let m = (s % 3600) / 60;
        format!("{h}时{m}分")
    }
}

pub fn is_quota_message(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "quota",
        "insufficient_quota",
        "rate limit",
        "rate_limit",
        "too many requests",
        "resource_exhausted",
        "http 429",
        "status 429",
        " 429",
        "429 ",
        "overloaded",
        "配额",
        "限流",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_error_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    if is_quota_message(trimmed) {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    [
        "error:",
        "fatal:",
        "panic",
        "unauthorized",
        "forbidden",
        "http 401",
        "http 403",
        "http 500",
        "http 502",
        "http 503",
        "http 504",
        "econnrefused",
        "econnreset",
        "etimedout",
        "failed to",
        "api error",
        "supervisor error:",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn has_held_lane_waiting(run: &Path) -> Option<String> {
    let entries = fs::read_dir(run).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if !p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .starts_with("lane-waiting-")
        {
            continue;
        }
        let Ok(f) = OpenOptions::new().read(true).open(&p) else {
            continue;
        };
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) != 0 } {
            let v = json(&p);
            let label = s(&v, "label");
            return Some(if label.is_empty() {
                "lane".to_string()
            } else {
                label.to_string()
            });
        }
    }
    None
}

fn shorten_cmd(raw: &str, meta: &Value, max_chars: usize) -> String {
    let mut cmd = raw.replace(['\n', '\r'], " ");
    for root in [
        s(meta, "top"),
        s(meta, "workdir"),
        s(&meta["worktree"], "source"),
    ] {
        if !root.is_empty() {
            cmd = cmd.replace(&format!("{root}/"), "").replace(root, ".");
        }
    }
    clip(cmd.trim(), max_chars)
}

#[derive(Debug, Default)]
struct TranscriptTail {
    mtime: Option<i64>,
    in_flight_cmd: Option<(String, Option<i64>)>,
    error_detail: Option<String>,
    thinking: bool,
}

fn latest_transcript_file(meta: &Value) -> Option<PathBuf> {
    let dir_str = s(meta, "sessionDir");
    if dir_str.is_empty() {
        return None;
    }
    let dir = Path::new(dir_str);
    if !dir.is_dir() {
        return None;
    }
    let mut best: Option<(i64, PathBuf)> = None;
    for path in crate::agents::session_files(dir) {
        let mtime = file_mtime_epoch(&path).unwrap_or(0);
        if best.as_ref().is_none_or(|(best_t, _)| mtime >= *best_t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

/// Inspect the tail of the latest Pi session transcript JSONL for in-flight bash tool calls,
/// recent assistant thinking blocks, or provider/quota errors.
fn inspect_transcript_tail(meta: &Value) -> TranscriptTail {
    let Some(path) = latest_transcript_file(meta) else {
        return TranscriptTail::default();
    };
    let mtime = file_mtime_epoch(&path);
    let tail = read_tail(&path, TAIL_TRANSCRIPT_BYTES);
    if tail.is_empty() {
        return TranscriptTail {
            mtime,
            ..Default::default()
        };
    }

    let mut out = TranscriptTail {
        mtime,
        ..Default::default()
    };
    let mut pending_bash: Vec<(String, String, Option<i64>)> = Vec::new();

    for line in tail.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let ts = v["timestamp"]
            .as_str()
            .and_then(iso_epoch)
            .or_else(|| v["message"]["timestamp"].as_i64().map(|ms| if ms > 10_000_000_000 { ms / 1000 } else { ms }));
        let msg = if v["message"].is_object() {
            &v["message"]
        } else {
            &v
        };
        let role = s(msg, "role");
        let etype = s(&v, "type");

        if role == "assistant" {
            let err = s(msg, "errorMessage");
            if !err.is_empty() {
                out.error_detail = Some(clip(err, 200));
            } else {
                out.error_detail = None;
            }
            out.thinking = false;
            if let Some(parts) = msg["content"].as_array() {
                for part in parts {
                    match s(part, "type") {
                        "thinking" => out.thinking = true,
                        "toolCall" | "tool_use" => {
                            let name = s(part, "name");
                            if name == "bash" || name == "run_command" {
                                let args = &part["arguments"];
                                let args = if args.is_object() { args } else { &part["input"] };
                                let cmd = s(args, "command");
                                let cmd = if cmd.is_empty() { s(args, "CommandLine") } else { cmd };
                                let id = s(part, "id").to_string();
                                if !cmd.is_empty() {
                                    pending_bash.push((id, cmd.to_string(), ts));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        } else if role == "toolResult" || etype == "toolResult" || etype == "tool_result" {
            let call_id = s(msg, "toolCallId");
            if !call_id.is_empty() {
                pending_bash.retain(|(id, _, _)| id != call_id);
            } else {
                pending_bash.pop();
            }
            out.thinking = true;
            if b(msg, "isError") {
                if let Some(txt) = msg["content"]
                    .as_array()
                    .and_then(|a| a.first())
                    .map(|x| s(x, "text"))
                    .filter(|t| !t.is_empty())
                {
                    if is_quota_message(txt) {
                        out.error_detail = Some(clip(txt, 200));
                    }
                }
            }
        } else if etype == "error" || !s(&v, "error").is_empty() {
            let det = if !s(&v, "error").is_empty() {
                s(&v, "error")
            } else {
                s(&v, "message")
            };
            if !det.is_empty() {
                out.error_detail = Some(clip(det, 200));
            }
        }
    }

    if let Some((_, cmd, ts)) = pending_bash.last() {
        out.in_flight_cmd = Some((cmd.clone(), *ts));
    }
    out
}

fn format_stall_suffix(sw: &Value) -> String {
    if s(sw, "kind") == "lane" || sw["pid"].as_u64().is_some() {
        let proc_info = crate::output::format_stall_proc(sw);
        if proc_info.is_empty() {
            " · 疑似卡住".into()
        } else {
            format!(" · 疑似卡住（{proc_info}）")
        }
    } else {
        " · 卡住提醒".into()
    }
}

/// Inspect a live run's activity from the tail of `events.jsonl`, session transcript,
/// `stderr.log`, queue markers, and `stall-warning.json`.
pub fn inspect(run: &Path, meta: &Value, state: &str) -> Value {
    let now = epoch() as i64;
    let stall_warning = {
        let sw = json(run.join("stall-warning.json"));
        sw.is_object().then_some(sw)
    };

    if state == "scouting" || run.join(".scouting").exists() {
        let srun = s(meta, "scoutRun");
        let label = if !srun.is_empty() {
            format!("探路中（{srun}）")
        } else {
            "探路中".to_string()
        };
        return json!({"kind": "scouting", "label": label});
    }

    // 1. Capacity queue / measurement queue / heavy lane wait -> waiting
    if state == "waiting" || run.join(".waiting").exists() {
        let q = json(run.join("queue.json"));
        let reason = s(&q, "reason");
        let label = if !reason.is_empty() {
            format!("等待中（排队：{}）", clip(reason, 48))
        } else if let Some(ahead) = crate::measure::waiting_ahead(run) {
            format!("等待中（测量队列前方 {ahead} 个）")
        } else {
            "等待中（排队）".to_string()
        };
        let mut out = json!({"kind": "waiting", "label": label});
        if !reason.is_empty() {
            out["detail"] = json!(reason);
        }
        return out;
    }

    if let Some(lane_label) = has_held_lane_waiting(run) {
        let mut out = json!({
            "kind": "waiting",
            "label": format!("等待中（等 lane：{}）", clip(&lane_label, 48)),
            "detail": lane_label,
        });
        if let Some(sw) = stall_warning {
            out["stallWarning"] = sw;
        }
        return out;
    }

    // Read tail of run/events.jsonl
    let ev_tail = read_tail(&run.join("events.jsonl"), TAIL_EVENTS_BYTES);
    let mut evs: Vec<Value> = ev_tail
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .collect();

    // Track in-flight bash commands and latest error/retry/resume in run/events.jsonl
    let mut in_flight: Vec<(String, i64)> = Vec::new();
    let mut last_event_at: Option<i64> = None;
    let mut tail_issue: Option<(bool, String, i64)> = None; // (is_quota, detail, ts)
    let mut waiting_resume: Option<String> = None;
    let mut last_tool_label: Option<String> = None;

    for e in evs.drain(..) {
        let kind = s(&e, "e");
        let ts = iso_epoch(s(&e, "at")).unwrap_or(now);
        last_event_at = Some(ts);
        match kind {
            "bash" => {
                in_flight.push((s(&e, "cmd").to_string(), ts));
                tail_issue = None;
                waiting_resume = None;
                last_tool_label = None;
            }
            "bash_done" => {
                in_flight.pop();
                if b(&e, "ok") {
                    tail_issue = None;
                }
            }
            "read" | "edit" | "write" => {
                tail_issue = None;
                waiting_resume = None;
                let p = s(&e, "path");
                let short = Path::new(p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(p);
                last_tool_label = Some(format!("{kind} {short}"));
            }
            "tool" => {
                tail_issue = None;
                waiting_resume = None;
                last_tool_label = Some(s(&e, "tool").to_string());
            }
            "turn" => {
                if s(&e, "stopReason") != "error" {
                    tail_issue = None;
                    waiting_resume = None;
                }
            }
            "retry" => {
                let msg = s(&e, "errorMessage");
                if !msg.is_empty() {
                    tail_issue = Some((is_quota_message(msg), clip(msg, 120), ts));
                }
            }
            "turn_error" | "resume_skipped" => {
                let det = s(&e, "detail");
                let det = if det.is_empty() { s(&e, "error") } else { det };
                if !det.is_empty() {
                    tail_issue = Some((is_quota_message(det), clip(det, 120), ts));
                }
            }
            "resume" => {
                let det = s(&e, "detail");
                waiting_resume = Some(if det.is_empty() {
                    "上游重试等待".to_string()
                } else {
                    det.to_string()
                });
            }
            "queue" => {
                waiting_resume = Some(format!("等 lane（前方 {} 个）", n(&e, "ahead")));
            }
            "settled" | "result" => {
                in_flight.clear();
                tail_issue = None;
                waiting_resume = None;
            }
            _ => {}
        }
    }

    // Inspect session transcript tail & stderr tail
    let transcript = inspect_transcript_tail(meta);
    let stderr_path = run.join("stderr.log");
    let stderr_mtime = file_mtime_epoch(&stderr_path);
    let stderr_tail = read_tail(&stderr_path, TAIL_STDERR_BYTES);
    let stderr_issue: Option<(bool, String)> = stderr_tail
        .lines()
        .rev()
        .find(|line| is_error_line(line))
        .map(|line| (is_quota_message(line), clip(line.trim(), 120)));

    // 2. Waiting on upstream resume backoff
    if let Some(wait_detail) = waiting_resume {
        let mut out = json!({
            "kind": "waiting",
            "label": format!("等待中（{}）", clip(&wait_detail, 60)),
            "detail": wait_detail,
        });
        if let Some(sw) = stall_warning {
            out["stallWarning"] = sw;
        }
        return out;
    }

    // 3. Quota or error from events tail, newer transcript error, or newer stderr error
    let effective_issue = if let Some((is_q, det, _)) = tail_issue {
        Some((is_q, det))
    } else if let Some(det) = transcript.error_detail {
        if transcript.mtime.unwrap_or(0) >= last_event_at.unwrap_or(0) {
            Some((is_quota_message(&det), det))
        } else {
            None
        }
    } else if let Some((is_q, det)) = stderr_issue {
        if stderr_mtime.unwrap_or(0) >= last_event_at.unwrap_or(0) && in_flight.is_empty() {
            Some((is_q, det))
        } else {
            None
        }
    } else {
        None
    };

    if let Some((is_quota, detail)) = effective_issue {
        let (kind, prefix) = if is_quota {
            ("quota", "配额受限")
        } else {
            ("error", "报错")
        };
        let mut out = json!({
            "kind": kind,
            "label": format!("{prefix}（{}）", clip(&detail, 60)),
            "detail": detail,
        });
        if let Some(sw) = stall_warning {
            out["stallWarning"] = sw;
        }
        return out;
    }

    // 4. Running command (from events.jsonl in-flight bash, or transcript in-flight bash, or accept.log)
    if let Some((raw_cmd, started_ts)) = in_flight.last().cloned().or_else(|| {
        transcript
            .in_flight_cmd
            .map(|(c, ts)| (c, ts.or(transcript.mtime).unwrap_or(now)))
    }) {
        let elapsed = (now - started_ts).max(0);
        let cmd = shorten_cmd(&raw_cmd, meta, 56);
        let dur = format_short_duration(elapsed);
        let suffix = stall_warning
            .as_ref()
            .map(format_stall_suffix)
            .unwrap_or_default();
        let mut out = json!({
            "kind": "running_command",
            "cmd": cmd,
            "elapsedSeconds": elapsed,
            "label": format!("在跑命令：{cmd}（{dur}{suffix}）"),
        });
        if let Some(sw) = stall_warning {
            out["stallWarning"] = sw;
        }
        return out;
    }

    // Check if acceptance command is actively running (accept.log exists without [exit ...])
    let accept_log = run.join("accept.log");
    if accept_log.is_file() {
        let tail = read_tail(&accept_log, 2048);
        if !tail.contains("\n[exit ") {
            let started = file_mtime_epoch(&accept_log).unwrap_or(now);
            let elapsed = stall_warning
                .as_ref()
                .and_then(|sw| sw["elapsedSeconds"].as_i64())
                .unwrap_or_else(|| (now - started).max(0));
            let cmd = shorten_cmd(s(meta, "accept"), meta, 56);
            let dur = format_short_duration(elapsed);
            let suffix = stall_warning
                .as_ref()
                .map(format_stall_suffix)
                .unwrap_or_default();
            let mut out = json!({
                "kind": "running_command",
                "cmd": cmd,
                "elapsedSeconds": elapsed,
                "label": format!("在跑验收：{cmd}（{dur}{suffix}）"),
            });
            if let Some(sw) = stall_warning {
                out["stallWarning"] = sw;
            }
            return out;
        }
    }

    // 5. Starting / no events yet and no transcript yet
    if state == "starting" || (last_event_at.is_none() && transcript.mtime.is_none()) {
        return json!({
            "kind": "waiting",
            "label": "等待中（启动）",
        });
    }

    // 6. Thinking (model working between tool calls)
    let idle = last_event_at
        .or(transcript.mtime)
        .map(|t| (now - t).max(0))
        .unwrap_or(0);
    let label = if let Some(sw) = &stall_warning {
        let det = s(sw, "detail");
        if det.is_empty() {
            format!("在思考（卡住提醒：已 {}）", format_short_duration(idle))
        } else {
            format!("在思考（卡住提醒：{}）", clip(det, 50))
        }
    } else if transcript.thinking {
        "在思考".to_string()
    } else if let Some(tool) = last_tool_label.filter(|_| idle < 30) {
        format!("在思考（刚执行 {tool}）")
    } else {
        "在思考".to_string()
    };

    let mut out = json!({
        "kind": "thinking",
        "elapsedSeconds": idle,
        "label": label,
    });
    if let Some(sw) = stall_warning {
        out["stallWarning"] = sw;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_run(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "delegate-activity-{tag}-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn classifies_running_command_with_elapsed_time() {
        let run = temp_run("cmd");
        let meta = json!({"workdir": "/repo", "sessionDir": run.join("session")});
        let started = format_epoch(epoch() as i64 - 125, false, "%Y-%m-%dT%H:%M:%SZ");
        write(
            run.join("events.jsonl"),
            format!("{}\n", json!({"e": "bash", "cmd": "cargo test -p api", "at": started})),
        )
        .unwrap();
        let act = inspect(&run, &meta, "running");
        assert_eq!(s(&act, "kind"), "running_command");
        assert!(s(&act, "cmd").contains("cargo test -p api"));
        assert!(act["elapsedSeconds"].as_i64().unwrap_or(0) >= 120);
        assert!(s(&act, "label").contains("在跑命令"));
        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn classifies_quota_and_error_from_stderr_and_transcript() {
        let run = temp_run("quota");
        let sess_dir = run.join("session");
        fs::create_dir_all(&sess_dir).unwrap();
        let meta = json!({"workdir": "/repo", "sessionDir": sess_dir});
        write(
            run.join("stderr.log"),
            "HTTP 429 Too Many Requests: insufficient_quota exceeded\n",
        )
        .unwrap();
        let act = inspect(&run, &meta, "running");
        assert_eq!(s(&act, "kind"), "quota");
        assert!(s(&act, "label").contains("配额"));

        // Transcript error with non-quota message -> error
        let _ = fs::remove_file(run.join("stderr.log"));
        write(
            sess_dir.join("2026-11-23T01-00-00Z_s1.jsonl"),
            format!(
                "{}\n",
                json!({"type": "message", "message": {"role": "assistant", "errorMessage": "HTTP 502 Bad Gateway", "content": []}})
            ),
        )
        .unwrap();
        let act2 = inspect(&run, &meta, "running");
        assert_eq!(s(&act2, "kind"), "error");
        assert!(s(&act2, "label").contains("报错"));
        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn classifies_thinking_from_transcript_tail_and_waiting_from_queue() {
        let run = temp_run("think");
        let sess_dir = run.join("session");
        fs::create_dir_all(&sess_dir).unwrap();
        let meta = json!({"workdir": "/repo", "sessionDir": sess_dir});
        write(
            sess_dir.join("2026-11-23T01-00-00Z_s2.jsonl"),
            format!(
                "{}\n",
                json!({"type": "message", "message": {"role": "assistant", "content": [{"type": "thinking", "thinking": "checking..."}]}})
            ),
        )
        .unwrap();
        let act = inspect(&run, &meta, "running");
        assert_eq!(s(&act, "kind"), "thinking");
        assert!(s(&act, "label").contains("在思考"));

        touch(run.join(".waiting"));
        write_json(run.join("queue.json"), &json!({"reason": "model capacity", "ahead": 2})).unwrap();
        let act_q = inspect(&run, &meta, "waiting");
        assert_eq!(s(&act_q, "kind"), "waiting");
        assert!(s(&act_q, "label").contains("等待中"));
        let _ = fs::remove_dir_all(&run);
    }
}
