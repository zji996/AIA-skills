//! Single source of truth for controller host definitions, detection, caller resolution,
//! capabilities, Stop/PreToolUse protocol rendering, `delegate hosts`, and `delegate install-hooks`.

use crate::common::*;
use serde_json::{json, Map, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Formally supported controller hosts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostId {
    Claude,
    Agy,
    Codex,
    Pi,
}

impl HostId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Agy => "agy",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Some(Self::Claude),
            "agy" | "antigravity" => Some(Self::Agy),
            "codex" => Some(Self::Codex),
            "pi" | "rpi" => Some(Self::Pi),
            _ => None,
        }
    }
}

/// Output wire format for `delegate hook check --format <fmt>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookFormat {
    Claude,
    Agy,
    Codex,
    Json,
}

impl HookFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Agy => "agy",
            Self::Codex => "codex",
            Self::Json => "json",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "agy" => Some(Self::Agy),
            "codex" => Some(Self::Codex),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Result of inspecting environment variables + hook payload for the calling host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectedHost {
    Supported(HostId),
    Ignored(&'static str),
    Unknown,
}

/// Capability bits for each controller host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostCapabilities {
    pub has_stop_hook: bool,
    pub has_pre_tool_hook: bool,
    pub bg_wakes_controller: bool,
    pub check_in_50m: bool,
}

/// JSON layout of a host's hook configuration file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookConfigLayout {
    /// `{"hooks": {"Stop": [{"hooks":[...]}], "PreToolUse": [{"matcher":...,"hooks":[...]}]}}`
    NestedHooks,
    /// `{"<group>": {"Stop": [{"type":"command","command":...}], "PreToolUse": [{"matcher":...,"hooks":[...]}]}}`
    NamedGroup(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct HostHookConfig {
    pub exists_dir: &'static str,
    pub config_path: &'static str,
    pub layout: HookConfigLayout,
    pub stop_event: &'static str,
    pub stop_command: &'static str,
    pub stop_markers: &'static [&'static str],
    pub pre_tool_event: &'static str,
    pub pre_tool_matcher: &'static str,
    pub pre_tool_command: &'static str,
    pub pre_tool_markers: &'static [&'static str],
}

#[derive(Clone, Copy, Debug)]
pub struct StopProtocolSpec {
    pub input_caller_key: Option<&'static str>,
    pub loop_guard_key: Option<&'static str>,
    pub allow_output: &'static str,
    pub block_decision: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct PreToolProtocolSpec {
    pub tool_name: &'static str,
    pub command_path: &'static str,
    pub bg_signal: Option<&'static str>,
    pub allow_output: &'static str,
    pub deny_format: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct HostSpec {
    pub id: HostId,
    pub name: &'static str,
    pub detect_env: &'static [&'static str],
    pub detect_env_prefixes: &'static [&'static str],
    pub detect_payload: &'static [&'static str],
    pub caller_env: &'static [&'static str],
    pub caller_payload_keys: &'static [&'static str],
    pub process_names: &'static [&'static str],
    pub node_script_markers: &'static [&'static str],
    pub skill_dirs: &'static [&'static str],
    pub hook_config: Option<HostHookConfig>,
    pub stop_protocol: Option<StopProtocolSpec>,
    pub pre_tool_protocol: Option<PreToolProtocolSpec>,
    pub capabilities: HostCapabilities,
    pub notes: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct IgnoredHostSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub detect_env: &'static [&'static str],
    pub detect_payload: &'static [&'static str],
    pub skill_dirs: &'static [&'static str],
    pub notes: &'static str,
}

/// Host-neutral Stop hook decision produced by `hook_check` before host rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopDecision {
    Allow {
        guard: Option<String>,
        disk_reminder: Option<String>,
    },
    Block {
        reason: String,
        next_cmd: String,
        remaining: usize,
        total: usize,
    },
}

/// Parent-controller session variables stripped when launching colleagues so nested
/// Claude / Codex / Pi / rpi invocations never cross-talk with the parent session identity.
/// Antigravity runtime variables (`ANTIGRAVITY_*`) are stripped by prefix.
pub const CONTROLLER_STRIP_ENV: [&str; 6] = [
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
    "PI_SESSION_ID",
    "RPI_SESSION_ID",
    "DELEGATE_CALLER",
];

pub const CONTROLLER_STRIP_PREFIXES: [&str; 1] = ["ANTIGRAVITY_"];

/// All env vars consulted when resolving caller identity (in `ANTIGRAVITY_AGENT` priority order).
pub const CALLER_ENV_ALL: [&str; 7] = [
    "DELEGATE_CALLER",
    "ANTIGRAVITY_CONVERSATION_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
    "PI_SESSION_ID",
    "RPI_SESSION_ID",
];

/// Caller env order when `ANTIGRAVITY_AGENT` is NOT active.
const CALLER_ENV_NON_AGY: [&str; 7] = [
    "DELEGATE_CALLER",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
    "PI_SESSION_ID",
    "RPI_SESSION_ID",
    "ANTIGRAVITY_CONVERSATION_ID",
];

/// Formally supported controller hosts (exact set of 4).
pub const CONTROLLERS: [HostSpec; 4] = [
    HostSpec {
        id: HostId::Claude,
        name: "Claude Code",
        detect_env: &["CLAUDE_CODE_SESSION_ID"],
        detect_env_prefixes: &[],
        detect_payload: &["session_id", "stop_hook_active"],
        caller_env: &["CLAUDE_CODE_SESSION_ID"],
        caller_payload_keys: &["session_id"],
        process_names: &["claude"],
        node_script_markers: &["claude"],
        skill_dirs: &["~/.claude/skills"],
        hook_config: Some(HostHookConfig {
            exists_dir: "~/.claude",
            config_path: "~/.claude/settings.json",
            layout: HookConfigLayout::NestedHooks,
            stop_event: "Stop",
            stop_command: "python3 \"$HOME/.claude/skills/delegate/hooks/claude-code-stop.py\"",
            stop_markers: &[
                "delegate-unmerged.sh",
                "delegate-waiter.sh",
                "claude-code-stop.py",
                "delegate hook stop",
            ],
            pre_tool_event: "PreToolUse",
            pre_tool_matcher: "Bash",
            pre_tool_command: "python3 \"$HOME/.claude/skills/delegate/hooks/claude-code-background.py\"",
            pre_tool_markers: &["claude-code-background.py"],
        }),
        stop_protocol: Some(StopProtocolSpec {
            input_caller_key: Some("session_id"),
            loop_guard_key: Some("stop_hook_active"),
            allow_output: "",
            block_decision: "block",
        }),
        pre_tool_protocol: Some(PreToolProtocolSpec {
            tool_name: "Bash",
            command_path: "tool_input.command",
            bg_signal: Some("tool_input.run_in_background"),
            allow_output: "",
            deny_format: "hookSpecificOutput.permissionDecision=deny",
        }),
        capabilities: HostCapabilities {
            has_stop_hook: true,
            has_pre_tool_hook: true,
            bg_wakes_controller: true,
            check_in_50m: true,
        },
        notes: "Full controller support (Stop + PreToolUse + background Bash wakeup + 50m check-in).",
    },
    HostSpec {
        id: HostId::Agy,
        name: "Antigravity CLI (agy)",
        detect_env: &["ANTIGRAVITY_AGENT", "ANTIGRAVITY_CONVERSATION_ID"],
        detect_env_prefixes: &["ANTIGRAVITY_"],
        detect_payload: &["conversationId", "continuationActive", "terminationReason", "toolCall"],
        caller_env: &["ANTIGRAVITY_CONVERSATION_ID"],
        caller_payload_keys: &["conversationId"],
        process_names: &["agy", "antigravity"],
        node_script_markers: &["antigravity"],
        skill_dirs: &["~/.gemini/config/skills"],
        hook_config: Some(HostHookConfig {
            exists_dir: "~/.gemini",
            config_path: "~/.gemini/config/hooks.json",
            layout: HookConfigLayout::NamedGroup("delegate"),
            stop_event: "Stop",
            stop_command: "\"$HOME/.gemini/config/skills/delegate/bin/delegate\" hook check --format agy",
            stop_markers: &[
                "hook check --format agy",
                "claude-code-stop.py",
                "delegate hook stop",
            ],
            pre_tool_event: "PreToolUse",
            pre_tool_matcher: "run_command",
            pre_tool_command: "python3 \"$HOME/.gemini/config/skills/delegate/hooks/claude-code-background.py\" --format agy",
            pre_tool_markers: &["claude-code-background.py"],
        }),
        stop_protocol: Some(StopProtocolSpec {
            input_caller_key: Some("conversationId"),
            loop_guard_key: Some("continuationActive"),
            allow_output: "{}",
            block_decision: "continue",
        }),
        pre_tool_protocol: Some(PreToolProtocolSpec {
            tool_name: "run_command",
            command_path: "toolCall.args.CommandLine",
            bg_signal: Some("toolCall.args.WaitMsBeforeAsync<=2000&&!IsDaemon"),
            allow_output: "{\"decision\":\"allow\"}",
            deny_format: "{\"decision\":\"deny\",\"reason\":...}",
        }),
        capabilities: HostCapabilities {
            has_stop_hook: true,
            has_pre_tool_hook: true,
            bg_wakes_controller: true,
            check_in_50m: true,
        },
        notes: "Full controller support (Stop + PreToolUse + Reactive Wakeup + 50m check-in).",
    },
    HostSpec {
        id: HostId::Codex,
        name: "Codex CLI",
        detect_env: &["CODEX_THREAD_ID", "CODEX_SESSION_ID", "CODEX_VERSION"],
        detect_env_prefixes: &[],
        detect_payload: &["turn_id"],
        caller_env: &["CODEX_THREAD_ID", "CODEX_SESSION_ID"],
        caller_payload_keys: &["session_id"],
        process_names: &["codex"],
        node_script_markers: &["codex"],
        skill_dirs: &["~/.agents/skills", "~/.codex/skills"],
        hook_config: Some(HostHookConfig {
            exists_dir: "~/.codex",
            config_path: "~/.codex/hooks.json",
            layout: HookConfigLayout::NestedHooks,
            stop_event: "Stop",
            stop_command: "\"$HOME/.agents/skills/delegate/bin/delegate\" hook check --format codex",
            stop_markers: &[
                "hook check --format codex",
                "claude-code-stop.py",
                "delegate hook stop",
            ],
            pre_tool_event: "PreToolUse",
            pre_tool_matcher: "Bash",
            pre_tool_command: "python3 \"$HOME/.agents/skills/delegate/hooks/claude-code-background.py\" --format codex",
            pre_tool_markers: &["claude-code-background.py"],
        }),
        stop_protocol: Some(StopProtocolSpec {
            input_caller_key: Some("session_id"),
            loop_guard_key: Some("stop_hook_active"),
            allow_output: "",
            block_decision: "block",
        }),
        pre_tool_protocol: Some(PreToolProtocolSpec {
            tool_name: "Bash",
            command_path: "tool_input.command",
            bg_signal: None,
            allow_output: "",
            deny_format: "hookSpecificOutput.permissionDecision=deny",
        }),
        capabilities: HostCapabilities {
            has_stop_hook: true,
            has_pre_tool_hook: true,
            bg_wakes_controller: false,
            check_in_50m: false,
        },
        notes: "Codex 0.171.0 native hooks.json (Stop + PreToolUse safety guards); no background wakeup, collect via foreground `delegate wait --max <mins>`.",
    },
    HostSpec {
        id: HostId::Pi,
        name: "Pi / rpi",
        detect_env: &["PI_SESSION_ID", "RPI_SESSION_ID"],
        detect_env_prefixes: &[],
        detect_payload: &[],
        caller_env: &["PI_SESSION_ID", "RPI_SESSION_ID"],
        caller_payload_keys: &[],
        process_names: &["pi", "rpi"],
        node_script_markers: &["/pi", "pi-coding-agent"],
        skill_dirs: &["~/.agents/skills", "~/.rpi/agent/skills"],
        hook_config: None,
        stop_protocol: None,
        pre_tool_protocol: None,
        capabilities: HostCapabilities {
            has_stop_hook: false,
            has_pre_tool_hook: false,
            bg_wakes_controller: false,
            check_in_50m: false,
        },
        notes: "Skill-only controller (rpi 0.20.0 extensions require compiled Rust cdylib; colleagues isolated via `--no-skills` + `DELEGATE_AGENT`). Collect via foreground `delegate wait --max <mins>`.",
    },
];

/// External non-controller hosts: skill-only, hooks never installed, hook calls always silently allowed.
pub const IGNORED_HOSTS: [IgnoredHostSpec; 2] = [
    IgnoredHostSpec {
        id: "cursor",
        name: "Cursor",
        detect_env: &["CURSOR_VERSION", "CURSOR_PROJECT_DIR", "CURSOR_TRACE_ID"],
        detect_payload: &["cursor_version", "workspace_roots", "conversation_id"],
        skill_dirs: &["~/.agents/skills"],
        notes: "Skill-only; Cursor reads ~/.claude/settings.json and turns Stop block reasons into user messages, so delegate hooks always allow silently.",
    },
    IgnoredHostSpec {
        id: "kilo",
        name: "Kilo Code",
        detect_env: &["KILO_SESSION_ID", "KILOCODE_VERSION", "KILO_VERSION"],
        detect_payload: &["kilo_version"],
        skill_dirs: &["~/.agents/skills"],
        notes: "Skill-only; hooks are not installed and any hook call allows silently.",
    },
];

pub fn spec(id: HostId) -> &'static HostSpec {
    CONTROLLERS
        .iter()
        .find(|h| h.id == id)
        .expect("all HostId variants must be in CONTROLLERS")
}

pub fn spec_by_name(name: &str) -> Option<&'static HostSpec> {
    HostId::parse(name).map(spec)
}

pub fn ignored_by_name(name: &str) -> Option<&'static IgnoredHostSpec> {
    let lower = name.trim().to_ascii_lowercase();
    IGNORED_HOSTS.iter().find(|h| h.id == lower)
}

pub fn caller_env_display_list() -> String {
    CALLER_ENV_ALL.join(" / ")
}

fn trim_nonempty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn payload_has_present_key(payload: &Value, key: &str) -> bool {
    let Some(val) = payload.get(key) else {
        return false;
    };
    match val {
        Value::Null => false,
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

pub fn is_antigravity_agent_with(get: impl Fn(&str) -> Option<String>) -> bool {
    trim_nonempty(get("ANTIGRAVITY_AGENT"))
        .is_some_and(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
}

pub fn caller_source_with(get: impl Fn(&str) -> Option<String>) -> Option<(String, &'static str)> {
    let order = if is_antigravity_agent_with(&get) {
        &CALLER_ENV_ALL
    } else {
        &CALLER_ENV_NON_AGY
    };
    order
        .iter()
        .copied()
        .find_map(|name| trim_nonempty(get(name)).map(|value| (value, name)))
}

/// Caller resolution priority for Stop hooks:
/// `DELEGATE_CALLER` > payload caller field (`session_id` / `conversationId`) > caller env vars.
pub fn resolve_hook_caller_with(
    payload: &Value,
    get_env: impl Fn(&str) -> Option<String>,
) -> (String, String) {
    if let Some(val) = trim_nonempty(get_env("DELEGATE_CALLER")) {
        return (val, "DELEGATE_CALLER".to_string());
    }
    let sid = s(payload, "session_id").trim();
    if !sid.is_empty() {
        let is_codex = payload_has_present_key(payload, "turn_id")
            || trim_nonempty(get_env("CODEX_THREAD_ID")).is_some()
            || trim_nonempty(get_env("CODEX_SESSION_ID")).is_some()
            || trim_nonempty(get_env("CODEX_VERSION")).is_some();
        let src = if is_codex {
            "CODEX_THREAD_ID"
        } else {
            "CLAUDE_CODE_SESSION_ID"
        };
        return (sid.to_string(), src.to_string());
    }
    let conv = s(payload, "conversationId").trim();
    if !conv.is_empty() {
        return (conv.to_string(), "ANTIGRAVITY_CONVERSATION_ID".to_string());
    }
    if let Some((val, src)) = caller_source_with(get_env) {
        return (val, src.to_string());
    }
    (String::new(), String::new())
}

/// Detect which host invoked a hook from env vars and stdin JSON payload.
/// Ignored/external hosts (Cursor, Kilo) are checked first on BOTH env and payload
/// so Cursor loading `~/.claude/settings.json` is always recognized and allowed silently.
pub fn detect_host_with(
    payload: &Value,
    get_env: impl Fn(&str) -> Option<String>,
) -> DetectedHost {
    for ignored in &IGNORED_HOSTS {
        if ignored
            .detect_payload
            .iter()
            .any(|k| payload_has_present_key(payload, k))
        {
            return DetectedHost::Ignored(ignored.id);
        }
        if ignored
            .detect_env
            .iter()
            .any(|k| trim_nonempty(get_env(k)).is_some())
        {
            return DetectedHost::Ignored(ignored.id);
        }
    }

    // Supported hosts (payload first, then env)
    if payload_has_present_key(payload, "conversationId")
        || payload.get("continuationActive").is_some()
        || payload_has_present_key(payload, "terminationReason")
        || payload.get("toolCall").is_some_and(Value::is_object)
        || is_antigravity_agent_with(&get_env)
    {
        return DetectedHost::Supported(HostId::Agy);
    }

    if payload_has_present_key(payload, "turn_id")
        || trim_nonempty(get_env("CODEX_THREAD_ID")).is_some()
        || trim_nonempty(get_env("CODEX_SESSION_ID")).is_some()
        || trim_nonempty(get_env("CODEX_VERSION")).is_some()
    {
        return DetectedHost::Supported(HostId::Codex);
    }

    if trim_nonempty(get_env("CLAUDE_CODE_SESSION_ID")).is_some()
        || payload_has_present_key(payload, "session_id")
        || payload.get("stop_hook_active").is_some()
    {
        return DetectedHost::Supported(HostId::Claude);
    }

    if trim_nonempty(get_env("PI_SESSION_ID")).is_some()
        || trim_nonempty(get_env("RPI_SESSION_ID")).is_some()
    {
        return DetectedHost::Supported(HostId::Pi);
    }

    if trim_nonempty(get_env("ANTIGRAVITY_CONVERSATION_ID")).is_some() {
        return DetectedHost::Supported(HostId::Agy);
    }

    DetectedHost::Unknown
}

/// Whether a Stop hook payload carries host-specific fields outside the generic `cwd` key
/// (used to distinguish an unrecognized foreign tool payload from minimal `{}`/`{"cwd":...}` calls).
pub fn payload_has_non_cwd_keys(payload: &Value) -> bool {
    payload
        .as_object()
        .is_some_and(|obj| obj.keys().any(|k| k != "cwd"))
}

/// Default 50-minute check-in interval based on active controller host capabilities.
pub fn check_in_50m_with(get: impl Fn(&str) -> Option<String>) -> Option<f64> {
    // Only hosts with `capabilities.check_in_50m == true` get the 50m background check-in.
    for host in &CONTROLLERS {
        if !host.capabilities.check_in_50m {
            continue;
        }
        if host
            .caller_env
            .iter()
            .any(|k| trim_nonempty(get(k)).is_some())
        {
            return Some(50.0 * 60.0);
        }
    }
    None
}

/// Match a process basename + argv tokens against the host table for harness PID lookup.
pub fn matches_harness_process(base: &str, tokens: &[String], source: Option<&str>) -> bool {
    let target_host = source.and_then(|src| {
        CONTROLLERS
            .iter()
            .find(|h| h.caller_env.contains(&src))
            .map(|h| h.id)
    });
    let hosts: Vec<&HostSpec> = match target_host {
        Some(id) => vec![spec(id)],
        None => CONTROLLERS.iter().collect(),
    };

    if hosts.iter().any(|h| h.process_names.contains(&base)) {
        return true;
    }
    if matches!(base, "node" | "bun" | "deno") {
        return tokens.iter().skip(1).take(3).any(|arg| {
            let lower = arg.to_ascii_lowercase();
            hosts.iter().any(|h| {
                h.node_script_markers.iter().any(|m| {
                    if m.starts_with('/') {
                        lower.ends_with(m)
                    } else {
                        lower.contains(m)
                    }
                })
            })
        });
    }
    false
}

/// Format the waiter "next step" hint according to the host's capabilities.
pub fn waiter_next_command_hint(format: HookFormat, wcmd: &str) -> String {
    match format {
        HookFormat::Agy => {
            format!("run_command 设 WaitMsBeforeAsync=500 后台跑 `{wcmd}`")
        }
        HookFormat::Codex => {
            format!("前台分段跑 `{wcmd} --max 5m`")
        }
        HookFormat::Claude | HookFormat::Json => format!("后台跑 `{wcmd}`"),
    }
}

/// Render a neutral `StopDecision` into the wire output for `format`.
/// Returns `None` when nothing should be printed to stdout (empty allow).
pub fn render_stop_decision(
    format: HookFormat,
    decision: &StopDecision,
    json_extra: Option<&Value>,
) -> Option<String> {
    match decision {
        StopDecision::Allow {
            guard,
            disk_reminder,
        } => match format {
            HookFormat::Claude => {
                if guard.is_none() {
                    disk_reminder
                        .as_ref()
                        .map(|note| json!({ "systemMessage": note }).to_string())
                } else {
                    None
                }
            }
            HookFormat::Agy => Some("{}".to_string()),
            HookFormat::Codex => None,
            HookFormat::Json => {
                let mut doc = json!({
                    "decision": "allow",
                    "reason": "",
                    "remaining": 0,
                    "total": 0,
                });
                if let Some(g) = guard {
                    doc["guard"] = json!(g);
                }
                if let Some(extra) = json_extra.and_then(Value::as_object) {
                    if let Some(map) = doc.as_object_mut() {
                        for (k, v) in extra {
                            map.insert(k.clone(), v.clone());
                        }
                    }
                }
                if let Some(note) = disk_reminder {
                    doc["diskReminder"] = json!(note);
                }
                Some(doc.to_string())
            }
        },
        StopDecision::Block {
            reason,
            next_cmd,
            remaining,
            total,
        } => match format {
            HookFormat::Claude | HookFormat::Codex => {
                Some(json!({ "decision": "block", "reason": reason }).to_string())
            }
            HookFormat::Agy => {
                Some(json!({ "decision": "continue", "reason": reason }).to_string())
            }
            HookFormat::Json => {
                let mut doc = json!({
                    "decision": "block",
                    "reason": reason,
                    "next": next_cmd,
                    "remaining": remaining,
                    "total": total,
                });
                if let Some(extra) = json_extra.and_then(Value::as_object) {
                    if let Some(map) = doc.as_object_mut() {
                        for (k, v) in extra {
                            map.insert(k.clone(), v.clone());
                        }
                    }
                }
                Some(doc.to_string())
            }
        },
    }
}

pub fn hosts_table_json() -> Value {
    let controllers: Vec<Value> = CONTROLLERS
        .iter()
        .map(|h| {
            let hook_cfg = h.hook_config.map(|hc| {
                let layout = match hc.layout {
                    HookConfigLayout::NestedHooks => "nested_hooks",
                    HookConfigLayout::NamedGroup(g) => g,
                };
                json!({
                    "existsDir": hc.exists_dir,
                    "configPath": hc.config_path,
                    "layout": layout,
                    "stop": {
                        "event": hc.stop_event,
                        "command": hc.stop_command,
                        "markers": hc.stop_markers,
                    },
                    "preToolUse": {
                        "event": hc.pre_tool_event,
                        "matcher": hc.pre_tool_matcher,
                        "command": hc.pre_tool_command,
                        "markers": hc.pre_tool_markers,
                    },
                })
            });
            let stop_proto = h.stop_protocol.map(|sp| {
                json!({
                    "inputCallerKey": sp.input_caller_key,
                    "loopGuardKey": sp.loop_guard_key,
                    "allowOutput": sp.allow_output,
                    "blockDecision": sp.block_decision,
                })
            });
            let pre_proto = h.pre_tool_protocol.map(|pp| {
                json!({
                    "toolName": pp.tool_name,
                    "commandPath": pp.command_path,
                    "bgSignal": pp.bg_signal,
                    "allowOutput": pp.allow_output,
                    "denyFormat": pp.deny_format,
                })
            });
            json!({
                "id": h.id.as_str(),
                "name": h.name,
                "supported": true,
                "detect": {
                    "env": h.detect_env,
                    "envPrefixes": h.detect_env_prefixes,
                    "payload": h.detect_payload,
                },
                "caller": {
                    "env": h.caller_env,
                    "payloadKeys": h.caller_payload_keys,
                },
                "skillDirs": h.skill_dirs,
                "hooks": hook_cfg,
                "stopProtocol": stop_proto,
                "preToolProtocol": pre_proto,
                "capabilities": {
                    "hasStopHook": h.capabilities.has_stop_hook,
                    "hasPreToolHook": h.capabilities.has_pre_tool_hook,
                    "bgWakesController": h.capabilities.bg_wakes_controller,
                    "checkIn50m": h.capabilities.check_in_50m,
                },
                "notes": h.notes,
            })
        })
        .collect();

    let ignored: Vec<Value> = IGNORED_HOSTS
        .iter()
        .map(|h| {
            json!({
                "id": h.id,
                "name": h.name,
                "supported": false,
                "detect": {
                    "env": h.detect_env,
                    "payload": h.detect_payload,
                },
                "skillDirs": h.skill_dirs,
                "hooks": Value::Null,
                "capabilities": {
                    "hasStopHook": false,
                    "hasPreToolHook": false,
                    "bgWakesController": false,
                    "checkIn50m": false,
                },
                "notes": h.notes,
            })
        })
        .collect();

    json!({
        "controllers": controllers,
        "ignored": ignored,
    })
}

fn render_hosts_human() -> String {
    let mut lines = Vec::new();
    lines.push("正式支持的主控宿主（4 个）：".to_string());
    lines.push(format!(
        "  {:<8} {:<22} {:<5} {:<8} {:<7} {:<8} {:<28} {}",
        "host", "name", "stop", "preTool", "bgWake", "50mChk", "hookConfig", "callerEnv"
    ));
    for h in &CONTROLLERS {
        let yn = |b: bool| if b { "yes" } else { "no" };
        let cfg = h.hook_config.map_or("—", |hc| hc.config_path);
        lines.push(format!(
            "  {:<8} {:<22} {:<5} {:<8} {:<7} {:<8} {:<28} {}",
            h.id.as_str(),
            h.name,
            yn(h.capabilities.has_stop_hook),
            yn(h.capabilities.has_pre_tool_hook),
            yn(h.capabilities.bg_wakes_controller),
            yn(h.capabilities.check_in_50m),
            cfg,
            h.caller_env.join(", "),
        ));
    }
    lines.push(String::new());
    lines.push("只装 Skill、不接钩子、静默放行：".to_string());
    for h in &IGNORED_HOSTS {
        lines.push(format!(
            "  {:<8} {:<22} env=[{}] payload=[{}]",
            h.id,
            h.name,
            h.detect_env.join(", "),
            h.detect_payload.join(", "),
        ));
    }
    lines.join("\n")
}

/// CLI entrypoint for `delegate hosts [--json]`.
pub fn hosts_command(args: &[String]) -> Res<i32> {
    let mut json_mode = crate::output::json_enabled();
    for a in args {
        match a.as_str() {
            "--json" => json_mode = true,
            _ => {
                return Err(unrecognized_arg(a, &["--json"], "delegate hosts [--json]"));
            }
        }
    }
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&hosts_table_json()).map_err(|e| e.to_string())?
        );
    } else {
        println!("{}", render_hosts_human());
    }
    Ok(0)
}

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home).join(rest);
            }
        }
    }
    PathBuf::from(raw)
}

fn command_matches(cmd: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| cmd.contains(m))
}

fn strip_flat_handlers(entries: &[Value], markers: &[&str]) -> (Vec<Value>, usize) {
    let mut out = Vec::new();
    let mut removed = 0usize;
    for entry in entries {
        if let Some(cmd) = entry.get("command").and_then(Value::as_str) {
            if command_matches(cmd, markers) {
                removed += 1;
                continue;
            }
        }
        out.push(entry.clone());
    }
    (out, removed)
}

fn strip_grouped_handlers(entries: &[Value], markers: &[&str]) -> (Vec<Value>, usize) {
    let mut out = Vec::new();
    let mut removed = 0usize;
    for entry in entries {
        let Some(obj) = entry.as_object() else {
            out.push(entry.clone());
            continue;
        };
        if let Some(cmd) = obj.get("command").and_then(Value::as_str) {
            if command_matches(cmd, markers) {
                removed += 1;
                continue;
            }
        }
        if let Some(subhooks) = obj.get("hooks").and_then(Value::as_array) {
            let mut kept = Vec::new();
            for h in subhooks {
                let matched = h
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| command_matches(c, markers));
                if matched {
                    removed += 1;
                } else {
                    kept.push(h.clone());
                }
            }
            if !kept.is_empty() {
                let mut updated = obj.clone();
                updated.insert("hooks".to_string(), Value::Array(kept));
                out.push(Value::Object(updated));
            } else if subhooks.is_empty() {
                out.push(entry.clone());
            }
        } else {
            out.push(entry.clone());
        }
    }
    (out, removed)
}

/// Idempotently install or uninstall hooks for a supported host into `target_path`.
pub fn apply_host_hooks(
    host: &HostSpec,
    target_path: &Path,
    uninstall: bool,
    stop_only: bool,
) -> Res<String> {
    let Some(hc) = host.hook_config else {
        return Ok(format!(
            "Host '{}' ({}) has no hook integration (skill-only); nothing to {}.",
            host.id.as_str(),
            host.name,
            if uninstall { "uninstall" } else { "install" }
        ));
    };

    let old_raw = if target_path.is_file() {
        Some(
            fs::read_to_string(target_path)
                .map_err(|e| format!("failed to read {}: {e}", target_path.display()))?,
        )
    } else {
        if uninstall {
            return Ok(format!(
                "Nothing to uninstall: {} does not exist.",
                target_path.display()
            ));
        }
        None
    };

    let mut root: Map<String, Value> = match old_raw.as_deref().map(str::trim) {
        None | Some("") => Map::new(),
        Some(text) => {
            let val: Value = serde_json::from_str(text)
                .map_err(|e| format!("[FAIL] {} is not valid JSON: {e}", target_path.display()))?;
            val.as_object()
                .cloned()
                .ok_or_else(|| format!("[FAIL] {} must contain a JSON object", target_path.display()))?
        }
    };

    let section_key = match hc.layout {
        HookConfigLayout::NestedHooks => "hooks",
        HookConfigLayout::NamedGroup(g) => g,
    };

    let mut section: Map<String, Value> = match root.get(section_key) {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => {
            return Err(format!(
                "[FAIL] {}: '{}' must be a JSON object",
                target_path.display(),
                section_key
            ));
        }
    };

    let stop_existing: Vec<Value> = match section.get(hc.stop_event) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => {
            return Err(format!(
                "[FAIL] {}: '{}.{}' must be a JSON array",
                target_path.display(),
                section_key,
                hc.stop_event
            ));
        }
    };

    let (mut stop_list, removed_stop) = match hc.layout {
        HookConfigLayout::NestedHooks => strip_grouped_handlers(&stop_existing, hc.stop_markers),
        HookConfigLayout::NamedGroup(_) => strip_flat_handlers(&stop_existing, hc.stop_markers),
    };

    if !uninstall {
        match hc.layout {
            HookConfigLayout::NestedHooks => {
                stop_list.push(json!({
                    "hooks": [{ "type": "command", "command": hc.stop_command }]
                }));
            }
            HookConfigLayout::NamedGroup(_) => {
                stop_list.push(json!({
                    "type": "command",
                    "command": hc.stop_command
                }));
            }
        }
        section.insert(hc.stop_event.to_string(), Value::Array(stop_list));

        if !stop_only {
            let pre_existing: Vec<Value> = match section.get(hc.pre_tool_event) {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(a)) => a.clone(),
                Some(_) => {
                    return Err(format!(
                        "[FAIL] {}: '{}.{}' must be a JSON array",
                        target_path.display(),
                        section_key,
                        hc.pre_tool_event
                    ));
                }
            };
            let (mut pre_list, _) = strip_grouped_handlers(&pre_existing, hc.pre_tool_markers);
            pre_list.push(json!({
                "matcher": hc.pre_tool_matcher,
                "hooks": [{ "type": "command", "command": hc.pre_tool_command }]
            }));
            section.insert(hc.pre_tool_event.to_string(), Value::Array(pre_list));
        }
        root.insert(section_key.to_string(), Value::Object(section));
    } else {
        if stop_list.is_empty() {
            section.remove(hc.stop_event);
        } else {
            section.insert(hc.stop_event.to_string(), Value::Array(stop_list));
        }

        if !stop_only {
            if let Some(val) = section.get(hc.pre_tool_event) {
                let pre_existing = val.as_array().ok_or_else(|| {
                    format!(
                        "[FAIL] {}: '{}.{}' must be a JSON array",
                        target_path.display(),
                        section_key,
                        hc.pre_tool_event
                    )
                })?;
                let (pre_list, _) = strip_grouped_handlers(pre_existing, hc.pre_tool_markers);
                if pre_list.is_empty() {
                    section.remove(hc.pre_tool_event);
                } else {
                    section.insert(hc.pre_tool_event.to_string(), Value::Array(pre_list));
                }
            }
        }

        if section.is_empty() {
            root.remove(section_key);
        } else {
            root.insert(section_key.to_string(), Value::Object(section));
        }
    }

    let new_raw = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())?
    );
    if old_raw.as_deref() != Some(new_raw.as_str()) {
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        if target_path.is_file() {
            let mut backup_os = target_path.as_os_str().to_os_string();
            backup_os.push(".bak");
            let backup_path = PathBuf::from(backup_os);
            fs::copy(target_path, &backup_path)
                .map_err(|e| format!("failed to back up {} to {}: {e}", target_path.display(), backup_path.display()))?;
        }
        write(target_path, &new_raw)?;
    }

    if !uninstall {
        let note = if removed_stop > 0 {
            format!(" (replaced {removed_stop} previous Stop entry/entries)")
        } else {
            String::new()
        };
        Ok(format!(
            "Configured delegate {} hooks in {}{note}.",
            host.id.as_str(),
            target_path.display()
        ))
    } else {
        Ok(format!(
            "Removed delegate {} hooks from {}.",
            host.id.as_str(),
            target_path.display()
        ))
    }
}

/// CLI entrypoint for `delegate install-hooks --host <name> [--path PATH] [--stop-only] [--uninstall]`.
pub fn install_hooks_command(args: &[String]) -> Res<i32> {
    let mut host_name: Option<String> = None;
    let mut path_override: Option<String> = None;
    let mut uninstall = false;
    let mut stop_only = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = a
            .split_once('=')
            .map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "--host" => {
                let raw = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --host: expected claude|agy|codex|pi")?
                    }
                };
                host_name = Some(raw);
            }
            "--path" => {
                let raw = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or("argument --path: expected a file path")?
                    }
                };
                path_override = Some(raw);
            }
            "--uninstall" => uninstall = true,
            "--stop-only" => stop_only = true,
            _ => {
                return Err(unrecognized_arg(
                    a,
                    &["--host", "--path", "--stop-only", "--uninstall"],
                    "delegate install-hooks --host <claude|agy|codex|pi> [--path PATH] [--stop-only] [--uninstall]",
                ));
            }
        }
        i += 1;
    }

    let raw_host = host_name.ok_or(
        "install-hooks requires --host <claude|agy|codex|pi> (usage: delegate install-hooks --host <name> [--path PATH] [--stop-only] [--uninstall])",
    )?;
    if let Some(ignored) = ignored_by_name(&raw_host) {
        return Err(format!(
            "host '{}' ({}) is skill-only and intentionally does not install delegate hooks",
            ignored.id, ignored.name
        ));
    }
    let Some(host) = spec_by_name(&raw_host) else {
        return Err(format!(
            "unknown host '{raw_host}' (expected: claude, agy, codex, pi)"
        ));
    };

    let target = match (path_override, host.hook_config) {
        (Some(p), _) => expand_tilde(&p),
        (None, Some(hc)) => expand_tilde(hc.config_path),
        (None, None) => PathBuf::new(),
    };

    let msg = apply_host_hooks(host, &target, uninstall, stop_only)?;
    println!("{msg}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_table_renders_allow_and_block_for_every_hook_format() {
        let allow_plain = StopDecision::Allow {
            guard: None,
            disk_reminder: None,
        };
        let allow_disk = StopDecision::Allow {
            guard: None,
            disk_reminder: Some("disk high".into()),
        };
        let allow_guard = StopDecision::Allow {
            guard: Some("unsupported_host".into()),
            disk_reminder: Some("disk high".into()),
        };
        let block = StopDecision::Block {
            reason: "任务已交付待看".into(),
            next_cmd: "`delegate card r1`".into(),
            remaining: 1,
            total: 2,
        };

        // Claude
        assert_eq!(render_stop_decision(HookFormat::Claude, &allow_plain, None), None);
        assert_eq!(
            serde_json::from_str::<Value>(
                &render_stop_decision(HookFormat::Claude, &allow_disk, None).unwrap()
            )
            .unwrap(),
            json!({"systemMessage": "disk high"})
        );
        assert_eq!(render_stop_decision(HookFormat::Claude, &allow_guard, None), None);
        assert_eq!(
            serde_json::from_str::<Value>(
                &render_stop_decision(HookFormat::Claude, &block, None).unwrap()
            )
            .unwrap(),
            json!({"decision": "block", "reason": "任务已交付待看"})
        );

        // Agy
        assert_eq!(
            render_stop_decision(HookFormat::Agy, &allow_plain, None).as_deref(),
            Some("{}")
        );
        assert_eq!(
            render_stop_decision(HookFormat::Agy, &allow_disk, None).as_deref(),
            Some("{}")
        );
        assert_eq!(
            render_stop_decision(HookFormat::Agy, &allow_guard, None).as_deref(),
            Some("{}")
        );
        assert_eq!(
            serde_json::from_str::<Value>(
                &render_stop_decision(HookFormat::Agy, &block, None).unwrap()
            )
            .unwrap(),
            json!({"decision": "continue", "reason": "任务已交付待看"})
        );

        // Codex
        assert_eq!(render_stop_decision(HookFormat::Codex, &allow_plain, None), None);
        assert_eq!(render_stop_decision(HookFormat::Codex, &allow_disk, None), None);
        assert_eq!(render_stop_decision(HookFormat::Codex, &allow_guard, None), None);
        assert_eq!(
            serde_json::from_str::<Value>(
                &render_stop_decision(HookFormat::Codex, &block, None).unwrap()
            )
            .unwrap(),
            json!({"decision": "block", "reason": "任务已交付待看"})
        );

        // Json
        let j_allow: Value = serde_json::from_str(
            &render_stop_decision(HookFormat::Json, &allow_guard, None).unwrap(),
        )
        .unwrap();
        assert_eq!(j_allow["decision"], "allow");
        assert_eq!(j_allow["guard"], "unsupported_host");
        let j_block: Value = serde_json::from_str(
            &render_stop_decision(HookFormat::Json, &block, None).unwrap(),
        )
        .unwrap();
        assert_eq!(j_block["decision"], "block");
        assert_eq!(j_block["reason"], "任务已交付待看");
        assert_eq!(j_block["remaining"], 1);
        assert_eq!(j_block["total"], 2);
    }

    #[test]
    fn detect_host_identifies_cursor_by_env_or_payload_and_supported_hosts() {
        let empty = json!({});

        // Cursor env only (even if Claude payload is also present)
        assert_eq!(
            detect_host_with(&json!({"session_id": "s1", "stop_hook_active": false}), |k| {
                (k == "CURSOR_VERSION").then(|| "0.50.0".into())
            }),
            DetectedHost::Ignored("cursor")
        );
        assert_eq!(
            detect_host_with(&empty, |k| {
                (k == "CURSOR_PROJECT_DIR").then(|| "/tmp/proj".into())
            }),
            DetectedHost::Ignored("cursor")
        );

        // Cursor payload only (clean env)
        assert_eq!(
            detect_host_with(&json!({"cursor_version": "0.50.0"}), |_| None),
            DetectedHost::Ignored("cursor")
        );
        assert_eq!(
            detect_host_with(&json!({"workspace_roots": ["/tmp/proj"]}), |_| None),
            DetectedHost::Ignored("cursor")
        );
        assert_eq!(
            detect_host_with(&json!({"conversation_id": "conv-123"}), |_| None),
            DetectedHost::Ignored("cursor")
        );

        // Kilo env / payload
        assert_eq!(
            detect_host_with(&empty, |k| (k == "KILO_SESSION_ID").then(|| "k1".into())),
            DetectedHost::Ignored("kilo")
        );
        assert_eq!(
            detect_host_with(&json!({"kilo_version": "1.0"}), |_| None),
            DetectedHost::Ignored("kilo")
        );

        // Supported hosts
        assert_eq!(
            detect_host_with(&json!({"session_id": "c1", "stop_hook_active": false}), |_| None),
            DetectedHost::Supported(HostId::Claude)
        );
        assert_eq!(
            detect_host_with(&json!({"session_id": "x1", "turn_id": "t1"}), |_| None),
            DetectedHost::Supported(HostId::Codex)
        );
        assert_eq!(
            detect_host_with(&json!({"conversationId": "a1"}), |_| None),
            DetectedHost::Supported(HostId::Agy)
        );
        assert_eq!(
            detect_host_with(&empty, |k| (k == "RPI_SESSION_ID").then(|| "p1".into())),
            DetectedHost::Supported(HostId::Pi)
        );
        assert_eq!(
            detect_host_with(&empty, |k| (k == "PI_SESSION_ID").then(|| "p2".into())),
            DetectedHost::Supported(HostId::Pi)
        );
    }

    #[test]
    fn check_in_50m_respects_host_capabilities() {
        assert_eq!(
            check_in_50m_with(|k| (k == "CLAUDE_CODE_SESSION_ID").then(|| "c1".into())),
            Some(3000.0)
        );
        assert_eq!(
            check_in_50m_with(|k| (k == "ANTIGRAVITY_CONVERSATION_ID").then(|| "a1".into())),
            Some(3000.0)
        );
        assert_eq!(
            check_in_50m_with(|k| (k == "CODEX_THREAD_ID").then(|| "x1".into())),
            None
        );
        assert_eq!(
            check_in_50m_with(|k| (k == "PI_SESSION_ID").then(|| "p1".into())),
            None
        );
        assert_eq!(
            check_in_50m_with(|k| (k == "RPI_SESSION_ID").then(|| "p2".into())),
            None
        );
    }

    #[test]
    fn install_hooks_codex_idempotent_with_backup() {
        let dir = env::temp_dir().join(format!("delegate-hosts-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("hooks.json");
        let orig = "{\n  \"hooks\": {\n    \"PostToolUse\": [\n      {\n        \"hooks\": [\n          {\n            \"command\": \"echo keep\",\n            \"type\": \"command\"\n          }\n        ]\n      }\n    ]\n  }\n}\n";
        fs::write(&target, orig).unwrap();

        let codex = spec(HostId::Codex);
        apply_host_hooks(codex, &target, false, false).unwrap();
        let bak = dir.join("hooks.json.bak");
        assert!(bak.is_file());
        assert_eq!(fs::read_to_string(&bak).unwrap(), orig);

        let doc: Value = serde_json::from_str(&fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(doc["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(doc["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert!(doc["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("--format codex"));
        assert!(doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("--format codex"));
        assert_eq!(doc["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);

        // Second run is idempotent and does not clobber .bak
        apply_host_hooks(codex, &target, false, false).unwrap();
        assert_eq!(fs::read_to_string(&bak).unwrap(), orig);

        // Uninstall leaves PostToolUse intact
        apply_host_hooks(codex, &target, true, false).unwrap();
        let doc2: Value = serde_json::from_str(&fs::read_to_string(&target).unwrap()).unwrap();
        assert!(doc2["hooks"].get("Stop").is_none());
        assert!(doc2["hooks"].get("PreToolUse").is_none());
        assert_eq!(doc2["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }
}
