//! Antigravity CLI (`agy`) as a colleague: models registered as `agy/<id>` run there.
//!
//! Same contract as Pi: full access in the run's working tree (read-only runs work in
//! a snapshot), prompt on stdin, JSON events on stdout, one conversation per run that a
//! reply continues. agy runs its shell commands as its own children, so the run's
//! process scope, pid kills and bash guards hold unchanged.
use crate::common::*;
use serde_json::{json, Value};
use std::collections::HashSet;

pub const BIN: &str = "agy";
const PREFIX: &str = "agy/";

/// Whether a registered model id (`provider/id`) runs on agy.
pub fn is_agy(model: &str) -> bool {
    model.starts_with(PREFIX)
}

/// The colleague executable a model runs on: `agy` for `agy/…`, otherwise Pi.
pub fn colleague(model: &str) -> &'static str {
    if is_agy(model) {
        BIN
    } else {
        crate::agents::AGENT
    }
}

/// agy's model id without the registry prefix.
pub fn model_id(model: &str) -> &str {
    model.strip_prefix(PREFIX).unwrap_or(model)
}

/// agy's `--effort`. Every call thinks: off and minimal map to its lowest level.
pub fn effort(thinking: &str) -> Option<&'static str> {
    match thinking {
        "" => None,
        "off" | "minimal" | "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        _ => Some("max"),
    }
}

/// The command line. The prompt goes on stdin as one stream-json message
/// (see `input`), so its size is not bound by argv.
pub fn command(meta: &Value, conversation: Option<&str>) -> Vec<String> {
    let mut a = vec![
        BIN.to_string(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--dangerously-skip-permissions".into(),
        "--disable-slash-commands".into(),
    ];
    let model = model_id(s(meta, "model"));
    if !model.is_empty() {
        a.extend(["--model".into(), model.into()]);
    }
    if let Some(level) = effort(s(meta, "thinking")) {
        a.extend(["--effort".into(), level.into()]);
    }
    if let Some(id) = conversation.filter(|id| !id.is_empty()) {
        a.extend(["--conversation".into(), id.into()]);
    }
    a
}

/// agy can push a long command into a background task and end its turn before the
/// result arrives; the run then settles on "waiting for …". Every message says so.
const FOREGROUND: &str = "Run commands in the foreground and wait for their results; do not end your reply while a background task is still running.";

/// One stream-json user message carrying the colleague addendum, the task, any
/// images (agy reads them from disk by path) and the foreground rule.
pub fn input(colleague: &str, prompt: &str, images: &[Value]) -> String {
    let mut text = String::new();
    if !colleague.is_empty() {
        text.push_str(colleague.trim_end());
        text.push_str("\n\n---\n\n");
    }
    text.push_str(prompt);
    let paths = images
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if !paths.is_empty() {
        text.push_str("\n\nImages for this task (open them before answering):\n");
        for path in paths {
            text.push_str("- ");
            text.push_str(path);
            text.push('\n');
        }
    }
    text.push_str("\n\n");
    text.push_str(FOREGROUND);
    format!(
        "{}\n",
        json!({"event":"user","message":{"role":"user","content":text}})
    )
}

/// The conversation id agy reports, from its `init` (or any later) event.
pub fn conversation(v: &Value) -> Option<&str> {
    [
        &v["conversation_id"],
        &v["step_update"]["conversation_id"],
        &v["result"]["conversation_id"],
    ]
    .into_iter()
    .find_map(Value::as_str)
    .filter(|id| !id.is_empty())
}

/// agy links files as `file://<worktree>/path#L1`; the worktree is gone after
/// apply, so answers keep the repo-relative path.
pub fn relative_links(text: &str, workdir: &str) -> String {
    let root = workdir.trim_end_matches('/');
    if root.is_empty() {
        return text.to_string();
    }
    text.replace(&format!("file://{root}/"), "")
        .replace(&format!("file://{root}"), ".")
}

/// Maps agy's stream-json events onto delegate's event log, the same shape Pi's
/// filter writes. Stateful: one tool step is reported ACTIVE more than once.
#[derive(Default)]
pub struct Filter {
    started: HashSet<i64>,
    commands: HashSet<i64>,
    /// Reply text since the last tool step. agy's `result.response` joins every
    /// agent_response of the turn, so progress notes ("waiting for clippy…")
    /// written between commands would open the answer.
    tail: String,
}

impl Filter {
    pub fn events(&mut self, v: &Value) -> Vec<Value> {
        match s(v, "event") {
            "step_update" => self.step(&v["step_update"]),
            "result" => {
                let r = &v["result"];
                let u = &r["usage"];
                let ok = s(r, "status") == "SUCCESS";
                let mut out = vec![json!({
                    "e": "turn",
                    "provider": BIN,
                    "model": Value::Null,
                    "stopReason": if ok { "stop" } else { "error" },
                    "usage": {
                        "input": u["input_tokens"].clone(),
                        "output": u["output_tokens"].clone(),
                        "cacheRead": u["cache_read_tokens"].clone(),
                    },
                })];
                let text = if self.tail.trim().is_empty() {
                    s(r, "response")
                } else {
                    self.tail.as_str()
                };
                if ok && !text.trim().is_empty() {
                    out.push(json!({"e":"result","text":text}));
                } else if !ok {
                    let detail = [s(r, "error"), s(r, "finished_reason"), s(r, "status")]
                        .into_iter()
                        .find(|x| !x.is_empty())
                        .unwrap_or("error");
                    out.push(json!({"e":"turn_error","detail":clip(detail,300)}));
                }
                out.push(json!({"e":"settled"}));
                out
            }
            _ => vec![],
        }
    }

    fn step(&mut self, u: &Value) -> Vec<Value> {
        match s(u, "step_type") {
            "agent_response" => {
                self.tail.push_str(s(u, "text_delta"));
                return vec![];
            }
            "tool" => self.tail.clear(),
            _ => return vec![],
        }
        let index = u["step_index"].as_i64().unwrap_or(-1);
        let tool = s(u, "tool_name");
        let params = &u["tool_info"]["parameters"];
        let done = s(u, "state") != "ACTIVE";
        let failed = ["ERROR", "FAILED", "CANCELED", "CANCELLED"].contains(&s(u, "state"))
            || !s(u, "error").is_empty()
            || !s(&u["tool_info"], "error").is_empty();
        let mut out = vec![];
        if self.started.insert(index) {
            out.push(start_event(tool, params));
            if tool == "run_command" {
                self.commands.insert(index);
            }
        }
        if done {
            if self.commands.remove(&index) {
                out.push(json!({"e":"bash_done","ok":!failed}));
            }
            if failed {
                let detail = [s(u, "error"), s(&u["tool_info"], "error")]
                    .into_iter()
                    .find(|x| !x.is_empty())
                    .unwrap_or(s(u, "state"));
                out.push(json!({"e":"tool_error","tool":tool,"detail":clip(detail,240)}));
            }
        }
        out
    }
}

pub fn raw_bash_command(v: &Value) -> Option<&str> {
    if s(v, "event") != "step_update" {
        return None;
    }
    let u = &v["step_update"];
    if s(u, "step_type") != "tool" || s(u, "tool_name") != "run_command" {
        return None;
    }
    let cmd = param(&u["tool_info"]["parameters"], &["CommandLine", "Command", "command"]);
    (!cmd.is_empty()).then_some(cmd)
}

fn param<'a>(params: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .map(|k| s(params, k))
        .find(|x| !x.is_empty())
        .unwrap_or("")
}

fn start_event(tool: &str, params: &Value) -> Value {
    let path = || {
        param(
            params,
            &["AbsolutePath", "TargetFile", "File", "Path", "path", "DirectoryPath"],
        )
        .to_string()
    };
    match tool {
        "run_command" => {
            let cmd = param(params, &["CommandLine", "Command", "command"]);
            json!({"e":"bash","cmd":clip(cmd,180)})
        }
        "view_file" | "read_file" => json!({"e":"read","path":path()}),
        "write_to_file" | "write_file" | "create_file" => json!({"e":"write","path":path()}),
        "replace_file_content" | "multi_replace_file_content" | "edit_file" => {
            json!({"e":"edit","path":path()})
        }
        _ => {
            let arg = [param(params, &["Query", "Pattern", "SearchPath"]), &path()]
                .into_iter()
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
                .join(" @ ");
            json!({"e":"tool","tool":tool,"arg":clip(&arg,160)})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_runs_with_full_access_and_continues_a_conversation() {
        let meta = json!({"model":"agy/gemini-4-neon","thinking":"high","mode":"read-only"});
        assert_eq!(
            command(&meta, Some("c-1")),
            [
                "agy",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--dangerously-skip-permissions",
                "--disable-slash-commands",
                "--model",
                "gemini-4-neon",
                "--effort",
                "high",
                "--conversation",
                "c-1"
            ]
        );
        assert!(!command(&json!({"model":"agy/m"}), None).contains(&"--conversation".into()));
    }

    #[test]
    fn effort_never_turns_thinking_off() {
        assert_eq!(effort("off"), Some("low"));
        assert_eq!(effort("medium"), Some("medium"));
        assert_eq!(effort("max"), Some("max"));
        assert_eq!(effort(""), None);
    }

    #[test]
    fn input_is_one_user_message_with_addendum_task_and_images() {
        let line = input("rules", "do it", &[json!("/a.png")]);
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(s(&v, "event"), "user");
        let text = s(&v["message"], "content");
        assert!(text.starts_with("rules\n\n---\n\ndo it"));
        assert!(text.contains("- /a.png"));
        assert!(text.ends_with(FOREGROUND));
    }

    #[test]
    fn filter_maps_commands_once_and_ends_with_the_answer() {
        let mut f = Filter::default();
        let step = |state: &str| json!({"event":"step_update","step_update":{"step_index":2,"state":state,"step_type":"tool","tool_name":"run_command","tool_info":{"parameters":{"CommandLine":"cat a.txt"}}}});
        let start = f.events(&step("ACTIVE"));
        assert_eq!(start, [json!({"e":"bash","cmd":"cat a.txt"})]);
        assert!(f.events(&step("ACTIVE")).is_empty());
        assert_eq!(f.events(&step("DONE")), [json!({"e":"bash_done","ok":true})]);
        let read = json!({"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"tool","tool_name":"view_file","tool_info":{"parameters":{"AbsolutePath":"/r/x.rs"}}}});
        assert_eq!(f.events(&read), [json!({"e":"read","path":"/r/x.rs"})]);
        let end = f.events(&json!({"event":"result","result":{"conversation_id":"c","status":"SUCCESS","response":"hello\n","usage":{"input_tokens":5,"output_tokens":2,"cache_read_tokens":1}}}));
        assert_eq!(s(&end[0], "stopReason"), "stop");
        assert_eq!(end[0]["usage"]["cacheRead"], json!(1));
        assert_eq!(end[1], json!({"e":"result","text":"hello\n"}));
        assert_eq!(end[2], json!({"e":"settled"}));
    }

    #[test]
    fn the_answer_is_the_reply_after_the_last_command() {
        let mut f = Filter::default();
        let say = |i: i64, t: &str| json!({"event":"step_update","step_update":{"step_index":i,"step_type":"agent_response","state":"ACTIVE","text_delta":t}});
        f.events(&say(1, "waiting for clippy.\n"));
        f.events(&json!({"event":"step_update","step_update":{"step_index":2,"step_type":"tool","state":"DONE","tool_name":"run_command","tool_info":{"parameters":{"CommandLine":"x"}}}}));
        f.events(&say(3, "Fixed. "));
        f.events(&json!({"event":"step_update","step_update":{"step_index":4,"step_type":"system_message","state":"DONE"}}));
        f.events(&say(5, "All green.\n"));
        let end = f.events(&json!({"event":"result","result":{"status":"SUCCESS","response":"waiting for clippy.\nFixed. All green.\n","usage":{}}}));
        assert_eq!(end[1]["text"], "Fixed. All green.\n");
    }

    #[test]
    fn worktree_links_become_repo_relative() {
        let text = "[a.rs](file:///w/run/src/a.rs#L3) and [root](file:///w/run)";
        assert_eq!(relative_links(text, "/w/run/"), "[a.rs](src/a.rs#L3) and [root](.)");
        assert_eq!(relative_links(text, ""), text);
    }

    #[test]
    fn a_failed_result_is_a_turn_error() {
        let mut f = Filter::default();
        let end = f.events(&json!({"event":"result","result":{"status":"ERROR","error":"quota","response":""}}));
        assert_eq!(s(&end[0], "stopReason"), "error");
        assert_eq!(end[1], json!({"e":"turn_error","detail":"quota"}));
    }

    #[test]
    fn conversation_id_comes_from_init_or_steps() {
        assert_eq!(
            conversation(&json!({"event":"init","conversation_id":"c-9"})),
            Some("c-9")
        );
        assert_eq!(
            conversation(&json!({"event":"step_update","step_update":{"conversation_id":"c-8"}})),
            Some("c-8")
        );
        assert_eq!(conversation(&json!({"event":"init","conversation_id":""})), None);
    }
}
