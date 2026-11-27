//! Automatic review-rework loop, controlled verdicts (`PASS` / `REWORK` / `NEEDS_CONTROLLER`),
//! `controllerReviewPaths` escalation with diff + SHA-256 sign hash, and `delegate sign`.
use crate::agents;
use crate::changes;
use crate::common::*;
use crate::events;
use crate::lane;
use crate::models;
use crate::runs;
use crate::supervise;
use crate::worktree;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc, Mutex,
};

pub const DEFAULT_RUBRIC: &str =
    include_str!("../../../skills/delegate/references/review-rubric.md");

/// Factual note attached when no clear pass/rework/needs-controller verdict is recognized from
/// the reviewer's opening or conclusion section (no follow-up retry; original `review.md` kept).
pub const VERDICT_MISSING_NOTE: &str =
    "注：未从审查开头识别出明确的通过/返工结论，已原样保留 review.md 供主控过目";

/// Single tail-appended follow-up prompt when a reviewer hits the repeated-command / polling
/// self-check (`loop_stopped`), asking it to conclude directly without running more commands.
pub const LOOP_STOP_RETRY_PROMPT: &str = "检测到你连续重复执行轮询或相同命令，已中断这轮命令。验收由 Host 独立并行执行并汇总结果，你不用跑验收，也不用等写手或验收进程。不要再调用命令或工具，直接基于已读快照和 diff 给出结论：能合（PASS）、要返工（REWORK）还是需要主控决定（NEEDS_CONTROLLER），写清理由和主控该重点看哪里。";

const REVIEW_ACCEPT_OWNED_NOTE: &str = "验收由 Host 独立并行执行并汇总结果（慢层验收失败会自动取消本轮审查）；你无需自己跑验收，也不要用 `ps`/`sleep` 等待写手或验收进程，直接基于当前只读快照与 diff 给结论。";

const EXTRA_TRIGGERS: [&str; 3] = ["controllerReviewPaths", "risk:high", "risk:critical"];
const DEFAULT_EXTRA_MIN_FIVE_HOUR_PCT: f64 = 50.0;
const DEFAULT_EXTRA_MIN_WEEKLY_PCT: f64 = 30.0;

const MAX_INLINE_DIFF_BYTES: usize = 200 * 1024;
const NEEDS_ATTENTION_REWORK_ROUND: u64 = 3;

/// Pure-Rust FIPS 180-4 SHA-256 (lowercase hex, 64 chars) — zero extra Cargo dependencies.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = String::with_capacity(64);
    for word in h {
        use std::fmt::Write;
        let _ = write!(out, "{word:08x}");
    }
    out
}

/// Glob matcher supporting `*` (single path segment), `?`, `**/` (zero or more dirs),
/// `/**` (anything underneath), bare `**`, and trailing `/` directory prefixes.
/// Polynomial time via DP so nested wildcards never backtrack exponentially.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pat = pattern.trim_start_matches("./");
    let p = path.trim_start_matches("./");
    if pat.is_empty() {
        return p.is_empty();
    }
    if pat.ends_with('/') {
        let dir = pat.trim_end_matches('/');
        return p == dir || p.starts_with(pat);
    }
    let pb = pat.as_bytes();
    let sb = p.as_bytes();
    let mut memo = vec![vec![0u8; sb.len() + 1]; pb.len() + 1];
    fn dp(pb: &[u8], sb: &[u8], pi: usize, si: usize, memo: &mut [Vec<u8>]) -> bool {
        if memo[pi][si] != 0 {
            return memo[pi][si] == 1;
        }
        let res = if pi == pb.len() {
            si == sb.len()
        } else if pb[pi..].starts_with(b"**/") {
            // Zero directories, or skip one segment (or one char across '/').
            dp(pb, sb, pi + 3, si, memo)
                || (si < sb.len()
                    && (dp(pb, sb, pi, si + 1, memo)
                        || (sb[si] == b'/' && dp(pb, sb, pi + 3, si + 1, memo))))
        } else if pb[pi..].starts_with(b"**") {
            // `**` matches anything including '/'.
            dp(pb, sb, pi + 2, si, memo) || (si < sb.len() && dp(pb, sb, pi, si + 1, memo))
        } else if pb[pi] == b'*' {
            dp(pb, sb, pi + 1, si, memo)
                || (si < sb.len() && sb[si] != b'/' && dp(pb, sb, pi, si + 1, memo))
        } else if pb[pi] == b'?' {
            si < sb.len() && sb[si] != b'/' && dp(pb, sb, pi + 1, si + 1, memo)
        } else {
            si < sb.len() && pb[pi] == sb[si] && dp(pb, sb, pi + 1, si + 1, memo)
        };
        memo[pi][si] = if res { 1 } else { 2 };
        res
    }
    dp(pb, sb, 0, 0, &mut memo)
}

fn validate_extra_review_config(val: &Value) -> Res<()> {
    match val {
        Value::Null | Value::Bool(false) => Ok(()),
        Value::Object(map) => {
            let mut enabled = true;
            for (k, v) in map {
                match k.as_str() {
                    "enabled" => {
                        enabled = v.as_bool().ok_or("review.extra.enabled must be a boolean")?;
                    }
                    "model" => {
                        let m = v.as_str().ok_or("review.extra.model must be a string")?;
                        models::validate_model(m)?;
                    }
                    "thinking" => {
                        let t = v.as_str().ok_or("review.extra.thinking must be a string")?;
                        models::validate_thinking(t)?;
                    }
                    "on" => {
                        let arr = v
                            .as_array()
                            .ok_or("review.extra.on must be a list of trigger names")?;
                        for item in arr {
                            let name = item
                                .as_str()
                                .ok_or("review.extra.on entries must be strings")?;
                            if !EXTRA_TRIGGERS.contains(&name) {
                                return Err(format!(
                                    "review.extra.on has unknown trigger {name:?} (allowed: {})",
                                    EXTRA_TRIGGERS.join(", ")
                                ));
                            }
                        }
                    }
                    "minQuota" => {
                        if !v.is_null() {
                            let mq = v
                                .as_object()
                                .ok_or("review.extra.minQuota must be an object {fiveHour?, weekly?}")?;
                            for (mk, mv) in mq {
                                match mk.as_str() {
                                    "fiveHour" | "weekly" => {
                                        let pct = mv.as_f64().ok_or_else(|| {
                                            format!("review.extra.minQuota.{mk} must be a number 0..=100")
                                        })?;
                                        if !pct.is_finite() || !(0.0..=100.0).contains(&pct) {
                                            return Err(format!(
                                                "review.extra.minQuota.{mk} must be within 0..=100"
                                            ));
                                        }
                                    }
                                    other => {
                                        return Err(format!(
                                            "review.extra.minQuota has unknown field {other:?} (allowed: fiveHour, weekly)"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    other => {
                        return Err(format!(
                            "review.extra has unknown field {other:?} (allowed: enabled, model, thinking, on, minQuota)"
                        ));
                    }
                }
            }
            if enabled {
                if map
                    .get("model")
                    .and_then(Value::as_str)
                    .is_none_or(|m| m.is_empty())
                {
                    return Err("review.extra.model is required when review.extra is enabled".into());
                }
                if map
                    .get("on")
                    .and_then(Value::as_array)
                    .is_none_or(|a| a.is_empty())
                {
                    return Err("review.extra.on must be a non-empty list when review.extra is enabled".into());
                }
            }
            Ok(())
        }
        _ => Err(
            "review.extra must be false, null, or {enabled?, model, thinking?, on, minQuota?}"
                .into(),
        ),
    }
}

pub fn validate_review_config(value: Option<&Value>) -> Res<()> {
    let Some(v) = value else { return Ok(()) };
    match v {
        Value::Null | Value::Bool(_) => Ok(()),
        Value::Object(map) => {
            for (k, val) in map {
                match k.as_str() {
                    "enabled" => {
                        if !val.is_boolean() {
                            return Err("review.enabled must be a boolean".into());
                        }
                    }
                    "tier" => {
                        let t = val
                            .as_str()
                            .ok_or("review.tier must be a string (review or scout)")?;
                        if !["review", "scout"].contains(&t) {
                            return Err(format!(
                                "review.tier must be a read-only tier (review or scout), got {t:?}"
                            ));
                        }
                    }
                    "model" => {
                        let m = val.as_str().ok_or("review.model must be a string")?;
                        models::validate_model(m)?;
                    }
                    "thinking" => {
                        let t = val.as_str().ok_or("review.thinking must be a string")?;
                        models::validate_thinking(t)?;
                    }
                    "rubric" => {
                        if !val.is_null()
                            && !val
                                .as_str()
                                .is_some_and(|s| !s.trim().is_empty() && !s.contains('\0'))
                        {
                            return Err(
                                "review.rubric must be a non-empty file path string or null".into(),
                            );
                        }
                    }
                    "extra" => {
                        validate_extra_review_config(val)?;
                    }
                    other => {
                        return Err(format!(
                            "review has unknown field {other:?} (allowed: enabled, tier, model, thinking, rubric, extra)"
                        ));
                    }
                }
            }
            Ok(())
        }
        _ => Err(
            "review must be a boolean or {enabled?, tier?, model?, thinking?, rubric?, extra?}"
                .into(),
        ),
    }
}

pub fn validate_controller_review_paths(value: Option<&Value>) -> Res<()> {
    let Some(v) = value else { return Ok(()) };
    if v.is_null() {
        return Ok(());
    }
    if !v.as_array().is_some_and(|a| {
        a.iter()
            .all(|item| item.as_str().is_some_and(|s| !s.trim().is_empty() && !s.contains('\0')))
    }) {
        return Err("controllerReviewPaths must be a list of non-empty globs".into());
    }
    Ok(())
}

pub fn is_enabled(review_val: &Value) -> bool {
    match review_val {
        Value::Bool(flag) => *flag,
        Value::Object(map) => map.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewModelSelection {
    pub tier: String,
    pub model: String,
    pub model_name: String,
    pub thinking: Option<String>,
    pub model_source: String,
    pub fallback_model: Option<String>,
    pub fallback_name: Option<String>,
    pub fallback_thinking: Option<String>,
    pub launch_fallback_from: Option<String>,
    pub launch_fallback_reason: Option<String>,
}

/// Pure model selection for automatic review, respecting four-layer precedence:
/// `start --review-model` (`cli`) > repo `.delegate.json` (`repo`) > user config (`user`) > `review` tier (`tier`).
/// If the explicitly selected model cannot be resolved in the alias/model registry, falls back to
/// the `review` tier with a non-silent reason.
pub fn resolve_review_selection(
    config_val: &Value,
    cli_model: Option<&str>,
    config_model_source: Option<&str>,
    listed: &[models::PiModel],
) -> Res<Option<ReviewModelSelection>> {
    let review_val = &config_val["review"];
    let cli_model = cli_model.filter(|m| !m.is_empty());
    let enabled = match review_val {
        Value::Bool(flag) => *flag,
        Value::Object(map) => map.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        Value::Null => cli_model.is_some(),
        _ => false,
    };
    if !enabled {
        return Ok(None);
    }
    let tier = review_val["tier"]
        .as_str()
        .filter(|t| !t.is_empty())
        .unwrap_or("review")
        .to_string();
    let explicit_thinking = review_val["thinking"].as_str().filter(|t| !t.is_empty());

    let tier_pick = models::pick(
        config_val,
        &tier,
        None,
        listed,
        agents::local_clock(),
        &|_| 0,
    );
    let tier_pick_with_thinking = if explicit_thinking.is_some() {
        models::pick(
            config_val,
            &tier,
            explicit_thinking,
            listed,
            agents::local_clock(),
            &|_| 0,
        )
    } else {
        tier_pick.clone()
    };

    let (explicit_model, model_source) = if let Some(m) = cli_model {
        (Some(m), "cli")
    } else if let Some(m) = review_val["model"].as_str().filter(|m| !m.is_empty()) {
        (Some(m), config_model_source.unwrap_or("repo"))
    } else {
        (None, "tier")
    };

    if let Some(name) = explicit_model {
        match models::resolve_model(name, config_val, listed)
            .and_then(|resolved| {
                let think = models::thinking(explicit_thinking, &json!(name), config_val)?;
                Ok((resolved, think))
            }) {
            Ok((resolved, think)) => {
                let alias = models::selected_alias(name, config_val).to_string();
                let (fb_model, fb_name, fb_thinking) = match tier_pick {
                    Ok(fb) if fb.model != resolved => {
                        let fb_alias = models::selected_alias(&fb.name, config_val).to_string();
                        (Some(fb.model), Some(fb_alias), fb.thinking)
                    }
                    _ => (None, None, None),
                };
                Ok(Some(ReviewModelSelection {
                    tier,
                    model: resolved,
                    model_name: alias,
                    thinking: think,
                    model_source: model_source.to_string(),
                    fallback_model: fb_model,
                    fallback_name: fb_name,
                    fallback_thinking: fb_thinking,
                    launch_fallback_from: None,
                    launch_fallback_reason: None,
                }))
            }
            Err(err) => {
                let fb = tier_pick.map_err(|_| err.clone())?;
                let fb_alias = models::selected_alias(&fb.name, config_val).to_string();
                let reason = if err.contains("not registered") {
                    format!("模型 {name} 未登记")
                } else {
                    format!("模型 {name} 不可用")
                };
                Ok(Some(ReviewModelSelection {
                    tier,
                    model: fb.model,
                    model_name: fb_alias,
                    thinking: fb.thinking,
                    model_source: "tier".to_string(),
                    fallback_model: None,
                    fallback_name: None,
                    fallback_thinking: None,
                    launch_fallback_from: Some(name.to_string()),
                    launch_fallback_reason: Some(reason),
                }))
            }
        }
    } else {
        let pick = tier_pick_with_thinking?;
        let alias = models::selected_alias(&pick.name, config_val).to_string();
        Ok(Some(ReviewModelSelection {
            tier,
            model: pick.model,
            model_name: alias,
            thinking: pick.thinking,
            model_source: "tier".to_string(),
            fallback_model: None,
            fallback_name: None,
            fallback_thinking: None,
            launch_fallback_from: None,
            launch_fallback_reason: None,
        }))
    }
}

/// Resolve reviewer model/agent/rubric/fallback/extra at launch time and snapshot into `meta["reviewConfig"]`.
pub fn resolve_launch_config(
    config_val: &Value,
    repo: Option<&Path>,
    cli_model: Option<&str>,
    config_model_source: Option<&str>,
) -> Res<Option<Value>> {
    let listed = agents::pi_models();
    let Some(sel) = resolve_review_selection(config_val, cli_model, config_model_source, &listed)?
    else {
        return Ok(None);
    };
    let review_val = &config_val["review"];

    let mut model = sel.model;
    let mut model_name = sel.model_name;
    let mut thinking = sel.thinking;
    let mut model_source = sel.model_source;
    let mut launch_fallback: Option<Value> = match (&sel.launch_fallback_from, &sel.launch_fallback_reason) {
        (Some(from), Some(reason)) => Some(json!({
            "from": from,
            "to": model_name,
            "toModel": model,
            "reason": reason,
        })),
        _ => None,
    };

    let mut fallback_json: Option<Value> = None;
    if let (Some(fb_model), Some(fb_name)) = (&sel.fallback_model, &sel.fallback_name) {
        let fb_colleague = crate::agy::colleague(fb_model);
        if let Ok((fb_bin, fb_ver)) = agents::agent_identity(fb_colleague) {
            fallback_json = Some(json!({
                "tier": sel.tier,
                "model": fb_model,
                "modelName": fb_name,
                "thinking": sel.fallback_thinking,
                "agent": fb_colleague,
                "agentBin": fb_bin,
                "agentVersion": fb_ver,
            }));
        }
    }

    let primary_colleague = crate::agy::colleague(&model);
    let (colleague, agent_bin, agent_version) = match agents::agent_identity(primary_colleague) {
        Ok((bin, ver)) => (primary_colleague, bin, ver),
        Err(err) => {
            if let Some(fb) = fallback_json.take() {
                let from_alias = model_name.clone();
                let from_model = model.clone();
                model = s(&fb, "model").to_string();
                model_name = s(&fb, "modelName").to_string();
                thinking = fb["thinking"].as_str().map(str::to_string);
                model_source = "tier".to_string();
                launch_fallback = Some(json!({
                    "from": from_alias,
                    "fromModel": from_model,
                    "to": model_name,
                    "toModel": model,
                    "reason": format!("启动失败：{}", clip(&err, 80)),
                }));
                let fb_col = crate::agy::colleague(&model);
                (
                    fb_col,
                    PathBuf::from(s(&fb, "agentBin")),
                    fb["agentVersion"].as_str().map(str::to_string),
                )
            } else {
                return Err(err);
            }
        }
    };

    let rubric = review_val["rubric"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|raw| {
            let p = Path::new(raw);
            if p.is_absolute() {
                p.to_path_buf()
            } else if let Some(top) = repo {
                top.join(p)
            } else {
                p.to_path_buf()
            }
        });

    let mut out = json!({
        "enabled": true,
        "tier": sel.tier,
        "model": model,
        "modelName": model_name,
        "modelSource": model_source,
        "thinking": thinking,
        "agent": colleague,
        "agentBin": agent_bin,
        "agentVersion": agent_version,
        "rubric": rubric.map(|p| p.to_string_lossy().into_owned()),
    });
    if let Some(fb) = fallback_json {
        out["fallback"] = fb;
    }
    if let Some(lfb) = launch_fallback {
        out["launchFallback"] = lfb;
    }

    // Resolve optional supplementary reviewer (`review.extra`).
    let extra_val = &review_val["extra"];
    if extra_val.is_object()
        && extra_val
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    {
        let em_raw = s(extra_val, "model");
        let em_thinking = extra_val["thinking"].as_str().filter(|t| !t.is_empty());
        let on = strings(&extra_val["on"]);
        let min_quota = extra_val.get("minQuota").cloned().unwrap_or(Value::Null);
        if !em_raw.is_empty() && !on.is_empty() {
            match models::resolve_model(em_raw, config_val, &listed).and_then(|resolved| {
                let think = models::thinking(em_thinking, &json!(em_raw), config_val)?;
                let col = crate::agy::colleague(&resolved);
                let (bin, ver) = agents::agent_identity(col)?;
                Ok((resolved, think, col, bin, ver))
            }) {
                Ok((resolved, think, col, bin, ver)) => {
                    let alias = models::selected_alias(em_raw, config_val).to_string();
                    out["extra"] = json!({
                        "enabled": true,
                        "model": resolved,
                        "modelName": alias,
                        "thinking": think,
                        "agent": col,
                        "agentBin": bin,
                        "agentVersion": ver,
                        "on": on,
                        "minQuota": min_quota,
                    });
                }
                Err(err) => {
                    out["extra"] = json!({
                        "enabled": true,
                        "modelName": em_raw,
                        "on": on,
                        "minQuota": min_quota,
                        "resolveError": clip(&err, 160),
                    });
                }
            }
        }
    }

    Ok(Some(out))
}

/// Classify whether a reviewer execution result indicates the configured review model is
/// unavailable (startup failure, upstream refusal, or quota exhaustion), returning a concise
/// Chinese reason for `审查改用 <别名>（原因）`.
pub fn classify_unavailable_reason(
    res: &Res<(String, String)>,
    round_dir: &Path,
) -> Option<String> {
    match res {
        Err(err) => Some(format!("启动失败：{}", clip(err, 80))),
        Ok((outcome, text)) => {
            if lane::stopped()
                || matches!(outcome.as_str(), "cancelled" | "timeout" | "loop_stopped")
            {
                return None;
            }
            if outcome == "ok" {
                return crate::agy::is_policy_refusal(text).then(|| "上游拒绝".to_string());
            }
            let mut diag = Vec::new();
            for ev in runs::events(round_dir) {
                if matches!(s(&ev, "e"), "turn_error" | "tool_error") {
                    let d = s(&ev, "detail").trim();
                    if !d.is_empty() {
                        diag.push(d.to_string());
                    }
                }
            }
            let stderr_txt = read(round_dir.join("stderr.log"));
            if !stderr_txt.trim().is_empty() {
                diag.push(stderr_txt);
            }
            if !text.trim().is_empty() {
                diag.push(text.clone());
            }
            let joined = diag.join("\n").to_ascii_lowercase();
            let quota_markers = [
                "insufficient_quota",
                "quota exceeded",
                "quota",
                "resource_exhausted",
                "insufficient credit",
                "rate limit",
                "rate_limit",
                "too many requests",
                "429",
                "usage limit",
                "limit reached",
                "capacity",
            ];
            if quota_markers.iter().any(|m| joined.contains(m)) {
                return Some("额度用尽".into());
            }
            let refusal_markers = [
                "prohibited use policy",
                "content policy",
                "sensitive words",
                "forbidden",
                "unauthorized",
                "unauthenticated",
                "model not found",
                "no such model",
                "unknown model",
                "unsupported model",
                "does not exist",
                "bad request",
                "bad gateway",
                "service unavailable",
                "upstream",
                "rejected",
                "refused",
                "403",
                "401",
                "400",
                "502",
                "503",
                "504",
            ];
            if refusal_markers.iter().any(|m| joined.contains(m)) {
                return Some("上游拒绝".into());
            }
            Some("启动失败".into())
        }
    }
}

/// Evaluate which `review.extra.on` triggers matched for the current change set.
pub fn evaluate_extra_triggers(
    extra_cfg: &Value,
    run: &Path,
    meta: &Value,
    total_diff: &str,
) -> Vec<String> {
    let wanted = strings(&extra_cfg["on"]);
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut matched = Vec::new();
    let mut cached_risks: Option<Vec<crate::card::RiskItem>> = None;
    for trigger in wanted {
        let hit = match trigger.as_str() {
            "controllerReviewPaths" => evaluate_controller_paths(run, meta, false)
                .ok()
                .flatten()
                .is_some(),
            "risk:high" => {
                let risks = cached_risks.get_or_insert_with(|| {
                    crate::card::evaluate_diff_rule_risks(meta, total_diff)
                });
                risks.iter().any(|r| r.severity <= crate::card::Severity::High)
            }
            "risk:critical" => {
                let risks = cached_risks.get_or_insert_with(|| {
                    crate::card::evaluate_diff_rule_risks(meta, total_diff)
                });
                risks
                    .iter()
                    .any(|r| r.severity == crate::card::Severity::Critical)
            }
            _ => false,
        };
        if hit {
            matched.push(trigger);
        }
    }
    matched
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExtraQuotaDecision {
    Allow,
    Skip {
        five_hour_pct: Option<f64>,
        weekly_pct: Option<f64>,
        reason: String,
    },
}

fn format_quota_pct_label(pct: Option<f64>) -> String {
    match pct {
        Some(v) if (v - v.round()).abs() < 0.05 => format!("{}%", v.round() as i64),
        Some(v) => format!("{v:.1}%"),
        None => "未知".to_string(),
    }
}

/// Check cached `agy` pool quota against `review.extra.minQuota` before launching
/// the supplementary reviewer. Never runs a subprocess or refreshes quota; reads cache only.
pub fn check_extra_quota(
    extra_cfg: &Value,
    quota_cache: &Value,
    active_account: Option<&str>,
) -> ExtraQuotaDecision {
    check_extra_quota_at(extra_cfg, quota_cache, active_account, epoch() as i64)
}

pub fn check_extra_quota_at(
    extra_cfg: &Value,
    quota_cache: &Value,
    active_account: Option<&str>,
    now_ep: i64,
) -> ExtraQuotaDecision {
    let mq = &extra_cfg["minQuota"];
    let extra_model = s(extra_cfg, "model");
    let is_agy = crate::agy::is_agy(extra_model);
    let min_5h = mq["fiveHour"]
        .as_f64()
        .or_else(|| is_agy.then_some(DEFAULT_EXTRA_MIN_FIVE_HOUR_PCT));
    let min_wk = mq["weekly"]
        .as_f64()
        .or_else(|| is_agy.then_some(DEFAULT_EXTRA_MIN_WEEKLY_PCT));
    if min_5h.is_none() && min_wk.is_none() {
        return ExtraQuotaDecision::Allow;
    }

    let Some(accounts) = quota_cache["accounts"].as_object() else {
        return ExtraQuotaDecision::Allow;
    };
    let acct = active_account
        .and_then(|id| accounts.get(id))
        .filter(|a| a.is_object() && b(a, "ok"))
        .or_else(|| {
            if active_account.is_none() {
                accounts
                    .get("default")
                    .filter(|a| a.is_object() && b(a, "ok"))
                    .or_else(|| {
                        accounts
                            .values()
                            .find(|a| {
                                a.is_object()
                                    && b(a, "ok")
                                    && a["entries"].as_array().is_some_and(|e| !e.is_empty())
                            })
                    })
            } else {
                None
            }
        });
    let Some(acct) = acct else {
        return ExtraQuotaDecision::Allow;
    };

    let target_pool = crate::agy_accounts::classify_model_pool(extra_model)
        .unwrap_or(crate::agy_accounts::QuotaPool::ClaudeGpt);
    let acct_fetched_ep = acct["fetchedEpoch"].as_i64().unwrap_or(0);
    let mut five_hour: Option<f64> = None;
    let mut weekly: Option<f64> = None;
    for entry in acct["entries"].as_array().into_iter().flatten() {
        if crate::agy_accounts::classify_quota_pool_name(s(entry, "pool")) != Some(target_pool) {
            continue;
        }
        let Some(pct) = entry["remainingPct"].as_f64().filter(|v| v.is_finite()) else {
            continue;
        };
        let is_passive = s(entry, "source") == "passive_error";
        let reset_at = s(entry, "resetAt");
        let reset_ep = (!reset_at.is_empty()).then(|| iso_epoch(reset_at)).flatten();
        if let Some(rep) = reset_ep {
            if now_ep >= rep + crate::agy_accounts::RESET_AT_BUFFER_SECS {
                continue;
            }
        }
        let entry_ep = entry["fetchedEpoch"]
            .as_i64()
            .filter(|&e| e > 0)
            .unwrap_or(acct_fetched_ep);
        if !is_passive && entry_ep > 0 {
            let age = now_ep - entry_ep;
            match s(entry, "window") {
                "fiveHour" if age > crate::agy_accounts::FIVE_HOUR_STALE_SECS => continue,
                "weekly" if age > crate::agy_accounts::WEEKLY_STALE_SECS => continue,
                _ => {}
            }
        }
        match s(entry, "window") {
            "fiveHour" => five_hour = Some(five_hour.map_or(pct, |cur| cur.min(pct))),
            "weekly" => weekly = Some(weekly.map_or(pct, |cur| cur.min(pct))),
            _ => {}
        }
    }
    if five_hour.is_none() && weekly.is_none() {
        return ExtraQuotaDecision::Allow;
    }
    let low_5h = min_5h.is_some_and(|th| five_hour.is_some_and(|v| v < th));
    let low_wk = min_wk.is_some_and(|th| weekly.is_some_and(|v| v < th));
    if low_5h || low_wk {
        let pool_name = match target_pool {
            crate::agy_accounts::QuotaPool::ClaudeGpt => "Claude",
            crate::agy_accounts::QuotaPool::Gemini => "Gemini",
        };
        let reason = format!(
            "补充审查跳过：{pool_name} 池剩余 5 小时 {} / 本周 {}",
            format_quota_pct_label(five_hour),
            format_quota_pct_label(weekly),
        );
        ExtraQuotaDecision::Skip {
            five_hour_pct: five_hour,
            weekly_pct: weekly,
            reason,
        }
    } else {
        ExtraQuotaDecision::Allow
    }
}

fn reviewer_quota_exhausted_error(
    res: &Res<(String, String)>,
    round_dir: &Path,
) -> Option<String> {
    let mut diag: Vec<String> = Vec::new();
    match res {
        Ok((outcome, _)) if outcome == "ok" || outcome == "cancelled" => return None,
        Ok((_, text)) => {
            if !text.trim().is_empty() {
                diag.push(text.clone());
            }
        }
        Err(err) => {
            diag.push(err.clone());
        }
    }
    for ev in crate::runs::events(round_dir).into_iter().rev() {
        if s(&ev, "e") == "turn_error" {
            let det = s(&ev, "detail");
            if !det.is_empty() {
                diag.push(det.to_string());
                break;
            }
        }
    }
    let stderr_txt = read(round_dir.join("stderr.log"));
    if !stderr_txt.trim().is_empty() {
        diag.push(stderr_txt);
    }
    let joined = diag.join("\n");
    if crate::agy_accounts::is_quota_exhausted_error(&joined) {
        Some(clip(joined.trim(), 300))
    } else {
        None
    }
}

/// Whether the supplementary reviewer's controlled verdict disagrees with the primary reviewer.
pub fn extra_verdict_disagrees(main_verdict: &str, extra_verdict: Option<&str>) -> bool {
    let valid = |v: &str| matches!(v, "PASS" | "REWORK" | "NEEDS_CONTROLLER");
    let Some(ev) = extra_verdict else {
        return false;
    };
    valid(main_verdict) && valid(ev) && main_verdict != ev
}

/// Locate the default rubric on disk next to the skill, or fall back to the compiled copy.
pub fn load_rubric(review_cfg: &Value) -> Res<(String, String)> {
    let custom = s(review_cfg, "rubric");
    if !custom.is_empty() {
        let p = Path::new(custom);
        let text = fs::read_to_string(p)
            .map_err(|e| format!("failed to read review rubric {}: {e}", p.display()))?;
        if text.trim().is_empty() {
            return Err(format!("review rubric is empty: {}", p.display()));
        }
        return Ok((text, p.display().to_string()));
    }
    // Check installed skill layout (`<skill>/bin/delegate` -> `<skill>/references/review-rubric.md`)
    // and dev cargo layout (`crates/delegate/target/...` -> `skills/delegate/references/review-rubric.md`).
    for candidate_root in [Some(script()), std::env::current_exe().ok()]
        .into_iter()
        .flatten()
    {
        let mut cur = candidate_root.parent();
        for _ in 0..6 {
            let Some(dir) = cur else { break };
            for rel in [
                "references/review-rubric.md",
                "skills/delegate/references/review-rubric.md",
            ] {
                let p = dir.join(rel);
                if p.is_file() {
                    if let Ok(text) = fs::read_to_string(&p) {
                        if !text.trim().is_empty() {
                            return Ok((text, p.display().to_string()));
                        }
                    }
                }
            }
            cur = dir.parent();
        }
    }
    Ok((
        DEFAULT_RUBRIC.to_string(),
        "references/review-rubric.md (embedded default)".to_string(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedVerdict {
    Pass,
    Rework,
    NeedsController,
    Invalid(String),
}

fn verdict_fence_marker(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|&c| c == ch).count();
    (count >= 3).then_some((ch, count))
}

fn strip_verdict_decor(raw: &str) -> &str {
    let mut s = raw.trim();
    loop {
        let prev = s;
        s = s.trim_start_matches(['#', '*', '_', '`', '>', '-', '•', ' ']);
        // Strip leading numbered list marker like `1. ` or `1) `
        if let Some(idx) = s.find(['.', ')']) {
            if idx > 0
                && idx <= 3
                && s[..idx].bytes().all(|b| b.is_ascii_digit())
                && s[idx + 1..].starts_with(char::is_whitespace)
            {
                s = s[idx + 1..].trim_start();
            }
        }
        s = s.trim_matches(['*', '_', '`', '[', ']', '【', '】', ' ']);
        if s == prev {
            break;
        }
    }
    s
}

fn strip_verdict_label_prefix(raw: &str) -> &str {
    let mut s = strip_verdict_decor(raw);
    const ZH_LABELS: &[&str] = &[
        "最终审查结论",
        "审查结论",
        "评审结论",
        "审核结论",
        "最终结论",
        "结论",
        "审查结果",
        "评审结果",
        "结果",
        "判断",
    ];
    const EN_LABELS: &[&str] = &[
        "review verdict",
        "final verdict",
        "verdict",
        "conclusion",
        "decision",
        "result",
    ];
    loop {
        let prev = s;
        let mut stripped = None;
        for lbl in ZH_LABELS {
            if let Some(rest) = s.strip_prefix(lbl) {
                let r = rest.trim_start_matches(['*', '_', '`', ']', '】', ' ']);
                if let Some(after) = r
                    .strip_prefix('：')
                    .or_else(|| r.strip_prefix(':'))
                    .or_else(|| r.strip_prefix('—'))
                    .or_else(|| r.strip_prefix('–'))
                    .or_else(|| r.strip_prefix('-'))
                {
                    stripped = Some(strip_verdict_decor(after));
                    break;
                }
            }
        }
        if stripped.is_none() {
            let lower = s.to_ascii_lowercase();
            for lbl in EN_LABELS {
                if lower.starts_with(lbl) {
                    let rest = &s[lbl.len()..];
                    let r = rest.trim_start_matches(['*', '_', '`', ']', '】', ' ']);
                    if let Some(after) = r
                        .strip_prefix(':')
                        .or_else(|| r.strip_prefix('：'))
                        .or_else(|| r.strip_prefix('—'))
                        .or_else(|| r.strip_prefix('–'))
                        .or_else(|| r.strip_prefix('-'))
                    {
                        stripped = Some(strip_verdict_decor(after));
                        break;
                    }
                }
            }
        }
        if let Some(next) = stripped {
            s = next;
        }
        if s == prev {
            break;
        }
    }
    s
}

fn ends_with_zh_negation(prefix: &str) -> bool {
    let p = prefix.trim_end();
    const NEG_SUFFIXES: &[&str] = &[
        "没有发现",
        "并未发现",
        "尚未发现",
        "未发现",
        "无发现",
        "不涉及",
        "不存在",
        "不构成",
        "不需要再",
        "不需要",
        "无需再",
        "不必再",
        "不用再",
        "暂无需",
        "无需",
        "无须",
        "不用",
        "不必",
        "不再",
        "没有",
        "并未",
        "尚未",
        "未见",
        "并无",
        "均无",
        "暂不",
        "不能",
        "无法",
        "无",
        "不",
        "未",
        "非",
        "没",
    ];
    if NEG_SUFFIXES.iter().any(|neg| p.ends_with(neg)) {
        return true;
    }
    // Clause-level negation scope: e.g. `通过，未发现任何需要修改的问题` or `没有发现明显需要主控拍板的事项`.
    let clause = p
        .rsplit(['，', '。', '；', ';', ',', '.', '：', ':', '\n'])
        .next()
        .unwrap_or(p)
        .trim_start();
    const CLAUSE_NEG_STARTS: &[&str] = &[
        "未发现",
        "没有发现",
        "无发现",
        "并未发现",
        "未见",
        "不涉及",
        "不存在",
        "无需",
        "无须",
        "不需要",
        "并无",
        "均无",
        "没有",
    ];
    CLAUSE_NEG_STARTS.iter().any(|neg| clause.starts_with(neg))
        && !clause.contains("但")
        && !clause.contains("不过")
        && !clause.contains("然而")
        && !clause.contains("只是")
}

fn contains_positive_zh_verdict(text: &str, phrase: &str) -> bool {
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(phrase) {
        let idx = from + rel;
        if !ends_with_zh_negation(&text[..idx]) {
            return true;
        }
        from = idx + phrase.len();
    }
    false
}

fn clause_describes_fixed_issue(prefix: &str) -> bool {
    let clause = prefix
        .rsplit(['，', '。', '；', ';', ',', '.', '\n'])
        .next()
        .unwrap_or(prefix);
    const FIXED_CUES: &[&str] = &[
        "已修复",
        "修复了",
        "已解决",
        "解决了",
        "已修正",
        "此前",
        "之前",
        "原本",
        "原先",
        "上轮",
    ];
    FIXED_CUES.iter().any(|cue| clause.contains(cue))
        && !clause.contains("但")
        && !clause.contains("仍")
        && !clause.contains("依然")
        && !clause.contains("还是")
}

fn contains_active_zh_fail(text: &str, phrase: &str) -> bool {
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(phrase) {
        let idx = from + rel;
        if !clause_describes_fixed_issue(&text[..idx]) {
            return true;
        }
        from = idx + phrase.len();
    }
    false
}

fn is_ascii_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn contains_ascii_word_match(text: &str, word: &str, case_sensitive: bool) -> Vec<usize> {
    let hay = if case_sensitive {
        text.to_string()
    } else {
        text.to_ascii_lowercase()
    };
    let needle = if case_sensitive {
        word.to_string()
    } else {
        word.to_ascii_lowercase()
    };
    let bytes = hay.as_bytes();
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = hay[from..].find(&needle) {
        let idx = from + rel;
        let end = idx + needle.len();
        let before_ok = idx == 0 || !is_ascii_word_char(bytes[idx - 1]);
        let after_ok = end == bytes.len() || !is_ascii_word_char(bytes[end]);
        if before_ok && after_ok {
            out.push(idx);
        }
        from = idx + needle.len();
    }
    out
}

fn ends_with_en_negation(prefix_lower: &str) -> bool {
    let p = prefix_lower.trim_end();
    const NEG: &[&str] = &[
        "did not",
        "does not",
        "failed to",
        "fail to",
        "cannot",
        "can't",
        "won't",
        "didn't",
        "doesn't",
        "don't",
        "without",
        "never",
        "not",
        "no",
    ];
    NEG.iter().any(|neg| {
        p.ends_with(neg)
            && (p.len() == neg.len()
                || !is_ascii_word_char(p.as_bytes()[p.len() - neg.len() - 1]))
    })
}

fn contains_positive_ascii_word(text: &str, word: &str, case_sensitive: bool) -> bool {
    let lower = text.to_ascii_lowercase();
    for idx in contains_ascii_word_match(text, word, case_sensitive) {
        if !ends_with_en_negation(&lower[..idx]) {
            return true;
        }
    }
    false
}

fn starts_with_token_followed_by_break(s: &str, prefix: &str) -> bool {
    let Some(rest) = s.strip_prefix(prefix) else {
        return false;
    };
    match rest.chars().next() {
        None => true,
        Some(c) => {
            c.is_whitespace()
                || matches!(
                    c,
                    '。' | '，'
                        | ','
                        | '.'
                        | '：'
                        | ':'
                        | '；'
                        | ';'
                        | '!'
                        | '！'
                        | '—'
                        | '–'
                        | '-'
                        | '('
                        | '（'
                        | ')'
                        | '）'
                        | ']'
                        | '】'
                )
        }
    }
}

fn is_standalone_zh_pass(s: &str) -> bool {
    // Match `通过` at the start of the cleaned verdict line followed by end or punctuation/space
    // (avoiding prepositional `通过检查代码发现...` or conditional `等测试通过后再看`).
    if starts_with_token_followed_by_break(s, "通过") {
        return true;
    }
    // Also accept short terminal statements like `本次改动通过。` / `可以通过`
    let trimmed_end = s.trim_end_matches(['。', '！', '!', '.', ' ']);
    if trimmed_end.chars().count() <= 16 && trimmed_end.ends_with("通过") {
        let prefix = &trimmed_end[..trimmed_end.len() - "通过".len()];
        if !ends_with_zh_negation(prefix)
            && !prefix.contains("等")
            && !prefix.contains("如果")
            && !prefix.contains("若")
            && !prefix.contains("测试")
            && !prefix.contains("验收")
            && !prefix.contains("编译")
        {
            return true;
        }
    }
    false
}

fn is_standalone_verdict_heading(title: &str) -> bool {
    let clean = strip_verdict_decor(title).trim_matches(['。', '！', '!', '.', ' ']);
    matches!(
        clean.to_ascii_uppercase().as_str(),
        "PASS" | "PASSED" | "REWORK" | "NEEDS_CONTROLLER"
    ) || matches!(
        clean,
        "通过"
            | "审查通过"
            | "评审通过"
            | "审核通过"
            | "可合入"
            | "可以合入"
            | "需返工"
            | "需要返工"
            | "建议返工"
            | "返工"
            | "不通过"
            | "未通过"
            | "需主控决定"
            | "需要主控决定"
            | "待主控决定"
    )
}

fn heading_verdict_payload(trimmed: &str) -> Option<&str> {
    let title = trimmed.trim_start_matches('#').trim();
    if title.is_empty() {
        return None;
    }
    if is_standalone_verdict_heading(title) {
        return Some(title);
    }
    // If a heading carries a `:` / `：` separator (e.g. `## 结论：通过` or `# Verdict: REWORK`),
    // only scan the right-hand side so report titles like `# 第 1 轮返工续审` never hijack the verdict.
    if let Some((_, rhs)) = title.split_once('：').or_else(|| title.split_once(':')) {
        let rhs = rhs.trim();
        if !rhs.is_empty() {
            return Some(rhs);
        }
    }
    None
}

fn parse_verdict_line(raw: &str) -> Option<ParsedVerdict> {
    let trimmed = raw.trim();
    let candidate = if trimmed.starts_with('#') {
        heading_verdict_payload(trimmed)?
    } else {
        trimmed
    };
    let s = strip_verdict_label_prefix(candidate);
    if s.is_empty() {
        return None;
    }
    let lower = s.to_ascii_lowercase();

    let has_needs_controller = contains_positive_ascii_word(s, "NEEDS_CONTROLLER", false)
        || contains_positive_ascii_word(s, "needs controller", false)
        || contains_positive_ascii_word(s, "need controller", false)
        || contains_positive_ascii_word(s, "needs a decision", false)
        || contains_positive_zh_verdict(s, "需要主控")
        || contains_positive_zh_verdict(s, "需主控")
        || contains_positive_zh_verdict(s, "待主控")
        || contains_positive_zh_verdict(s, "交主控")
        || contains_positive_zh_verdict(s, "请主控决定")
        || contains_positive_zh_verdict(s, "请主控拍板");

    // Require uppercase `REWORK` anywhere, or lowercase `rework` / bare `返工` ONLY at the start of
    // the stripped verdict statement; mid-sentence Chinese rework only matches directive forms
    // (`需返工`/`建议返工` etc.) so prose like `通过，本轮返工已修复` / `PASS: rework items verified`
    // is never misclassified as `Rework`.
    let starts_with_lower_rework = starts_with_token_followed_by_break(&lower, "rework");
    let starts_with_zh_rework = starts_with_token_followed_by_break(s, "返工");

    let has_rework = contains_positive_ascii_word(s, "REWORK", true)
        || starts_with_lower_rework
        || starts_with_zh_rework
        || lower.contains("not pass")
        || lower.contains("did not pass")
        || lower.contains("failed to pass")
        || contains_active_zh_fail(s, "不通过")
        || contains_active_zh_fail(s, "未通过")
        || contains_active_zh_fail(s, "不能通过")
        || contains_active_zh_fail(s, "无法通过")
        || contains_active_zh_fail(s, "暂不通过")
        || contains_positive_zh_verdict(s, "需返工")
        || contains_positive_zh_verdict(s, "建议返工")
        || contains_positive_zh_verdict(s, "需要返工")
        || contains_positive_zh_verdict(s, "打回返工")
        || contains_positive_zh_verdict(s, "应返工")
        || contains_positive_zh_verdict(s, "请返工")
        || contains_positive_zh_verdict(s, "需要修改")
        || contains_positive_zh_verdict(s, "需修改");

    let starts_with_lower_pass = starts_with_token_followed_by_break(&lower, "pass")
        || starts_with_token_followed_by_break(&lower, "passed");

    let has_pass = contains_positive_ascii_word(s, "PASS", true)
        || starts_with_lower_pass
        || contains_positive_zh_verdict(s, "审查通过")
        || contains_positive_zh_verdict(s, "评审通过")
        || contains_positive_zh_verdict(s, "审核通过")
        || contains_positive_zh_verdict(s, "结论通过")
        || contains_positive_zh_verdict(s, "直接通过")
        || contains_positive_zh_verdict(s, "可合入")
        || contains_positive_zh_verdict(s, "可以合入")
        || contains_positive_zh_verdict(s, "建议合入")
        || contains_positive_zh_verdict(s, "无阻塞问题")
        || contains_positive_zh_verdict(s, "无阻断问题")
        || is_standalone_zh_pass(s);

    if has_needs_controller && !has_rework {
        return Some(ParsedVerdict::NeedsController);
    }
    if has_rework && !has_needs_controller {
        return Some(ParsedVerdict::Rework);
    }
    if has_needs_controller && has_rework {
        return Some(ParsedVerdict::NeedsController);
    }
    if has_pass {
        return Some(ParsedVerdict::Pass);
    }
    None
}

fn is_verdict_section_heading(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('#') {
        return false;
    }
    let title = trimmed.trim_start_matches('#').trim();
    let lower = title.to_ascii_lowercase();
    title.contains("结论")
        || title.contains("审查结果")
        || title.contains("评审结果")
        || lower.contains("verdict")
        || lower.contains("conclusion")
}

/// Relaxed reviewer verdict parser: scans the first 5 non-empty lines and any `结论` / `Verdict`
/// section heading + first body line, recognizing both English (`PASS`/`REWORK`/`NEEDS_CONTROLLER`)
/// and Chinese natural verdicts while never guessing `Pass` on ambiguous or negated text.
pub fn parse_verdict(answer: &str) -> ParsedVerdict {
    let mut fence: Option<(char, usize)> = None;
    let mut first_non_empty = String::new();
    let mut non_empty_count = 0usize;
    let mut non_code_lines: Vec<&str> = Vec::new();

    for line in answer.lines() {
        if let Some((fc, flen)) = fence {
            if let Some((c, len)) = verdict_fence_marker(line) {
                if c == fc && len >= flen && line.trim_start()[len..].trim().is_empty() {
                    fence = None;
                }
            }
            continue;
        } else if let Some(m) = verdict_fence_marker(line) {
            fence = Some(m);
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" || trimmed == "***" {
            continue;
        }
        if first_non_empty.is_empty() {
            first_non_empty = trimmed.to_string();
        }
        non_code_lines.push(trimmed);
        if non_empty_count < 5 {
            non_empty_count += 1;
            if let Some(v) = parse_verdict_line(trimmed) {
                return v;
            }
        }
    }

    // Fallback: inspect any `## 结论` / `## Verdict` section heading and its first non-empty line.
    for (idx, &line) in non_code_lines.iter().enumerate() {
        if is_verdict_section_heading(line) {
            if let Some(v) = parse_verdict_line(line) {
                return v;
            }
            if let Some(&next_line) = non_code_lines.get(idx + 1) {
                if !next_line.starts_with('#') {
                    if let Some(v) = parse_verdict_line(next_line) {
                        return v;
                    }
                }
            }
        }
    }

    ParsedVerdict::Invalid(clip(&first_non_empty, 120))
}

#[derive(Debug, Clone)]
pub struct ControllerReviewHit {
    pub paths: Vec<String>,
    pub diff: String,
    pub hash: String,
}

fn controller_review_patterns(meta: &Value) -> Vec<String> {
    let from_meta = strings(&meta["controllerReviewPaths"]);
    if !from_meta.is_empty() {
        return from_meta;
    }
    let source = s(&meta["worktree"], "source");
    let top = if !source.is_empty() {
        source
    } else {
        s(meta, "top")
    };
    if !top.is_empty() {
        if let Ok(cfg) = crate::config::load(Some(Path::new(top))) {
            return strings(&cfg.value["controllerReviewPaths"]);
        }
    }
    Vec::new()
}

fn current_after_tree(run: &Path, meta: &Value, live_worktree: bool) -> Res<(PathBuf, String)> {
    let rec = json(run.join("changes.json"));
    let mut top = PathBuf::from(s(&rec, "top"));
    if !top.is_dir() {
        let src = s(&meta["worktree"], "source");
        if !src.is_empty() && Path::new(src).is_dir() {
            top = PathBuf::from(src);
        } else if !s(meta, "top").is_empty() && Path::new(s(meta, "top")).is_dir() {
            top = PathBuf::from(s(meta, "top"));
        }
    }
    if live_worktree && meta["worktree"].is_object() {
        let wt = PathBuf::from(s(&meta["worktree"], "path"));
        if wt.is_dir() {
            let excludes = strings(&meta["snapshotExclude"]);
            if let Some(snap) = changes::snapshot(&wt, run, &excludes) {
                let tree = s(&snap, "tree").to_string();
                if !tree.is_empty() {
                    return Ok((wt, tree));
                }
            }
        }
    }
    let after = s(&rec, "after").to_string();
    if after.is_empty() {
        return Err("run has no recorded after tree".into());
    }
    Ok((top, after))
}

/// Evaluate `controllerReviewPaths` against the chain's net changed files (`effective_total_base..after`).
/// When `live_worktree` is true and the worktree still exists, snapshots the live worktree so even
/// a 1-character post-delivery edit changes the diff and invalidates any previous signature.
pub fn evaluate_controller_paths(
    run: &Path,
    meta: &Value,
    live_worktree: bool,
) -> Res<Option<ControllerReviewHit>> {
    let patterns = controller_review_patterns(meta);
    if patterns.is_empty() {
        return Ok(None);
    }
    let Ok((top, after_tree)) = current_after_tree(run, meta, live_worktree) else {
        return Ok(None);
    };
    if !top.is_dir() {
        return Ok(None);
    }
    let (total_base, _) = worktree::effective_total_base(&top, meta);
    if total_base.is_empty() {
        return Ok(None);
    }
    let names_raw = git(
        &top,
        &["diff", "--name-only", "-z", "--no-renames", &total_base, &after_tree],
    )?;
    let mut matched: Vec<String> = names_raw
        .split(|b| *b == 0)
        .filter(|x| !x.is_empty())
        .map(|x| String::from_utf8_lossy(x).into_owned())
        .filter(|path| patterns.iter().any(|pat| glob_match(pat, path)))
        .collect();
    matched.sort();
    matched.dedup();
    if matched.is_empty() {
        return Ok(None);
    }
    let mut args: Vec<&str> = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--color=never",
        "--binary",
        &total_base,
        &after_tree,
        "--",
    ];
    for p in &matched {
        args.push(p.as_str());
    }
    let diff_bytes = git(&top, &args)?;
    let hash = format!("sha256:{}", sha256_hex(&diff_bytes));
    let diff = String::from_utf8_lossy(&diff_bytes).into_owned();
    Ok(Some(ControllerReviewHit {
        paths: matched,
        diff,
        hash,
    }))
}

/// Compute signature validity for `status`, `sign --check`, and merger.
/// When `live_worktree` is false (read paths: `status`, `snapshot`, `index`), performs pure
/// JSON comparison between `sign.json` and `review.json` with zero git subprocesses.
/// `sign`, `apply`, and `merge` pass `live_worktree = true` to verify the live worktree.
pub fn sign_status(run: &Path, meta: &Value, review: &Value, live_worktree: bool) -> Option<Value> {
    let sign_doc = json(run.join("sign.json"));
    let cr = &review["controllerReview"];
    let has_cr = cr.is_object() && !s(cr, "hash").is_empty();
    let has_sign = sign_doc.is_object() && !s(&sign_doc, "hash").is_empty();
    if !has_cr && !has_sign {
        return None;
    }

    let live = if live_worktree {
        evaluate_controller_paths(run, meta, true).ok().flatten()
    } else {
        None
    };
    let (current_hash, current_paths) = if let Some(hit) = live {
        (hit.hash, hit.paths)
    } else if has_cr {
        (s(cr, "hash").to_string(), strings(&cr["paths"]))
    } else {
        (String::new(), Vec::new())
    };
    let required = !current_hash.is_empty() || has_cr;
    let signed_hash = s(&sign_doc, "hash").to_string();
    let signed = !signed_hash.is_empty();
    let valid = signed && !current_hash.is_empty() && signed_hash == current_hash;
    let invalidated = signed && !valid;

    let mut out = json!({
        "required": required,
        "signed": signed,
        "valid": valid,
        "invalidated": invalidated,
        "hash": current_hash,
        "paths": current_paths,
    });
    if signed {
        out["signedHash"] = json!(signed_hash);
        if !s(&sign_doc, "signedAt").is_empty() {
            out["signedAt"] = sign_doc["signedAt"].clone();
        }
        if !s(&sign_doc, "signedBy").is_empty() {
            out["signedBy"] = sign_doc["signedBy"].clone();
        }
    }
    Some(out)
}

/// `delegate sign [--check] [--json] [<run>]`
pub fn sign_command(args: &[String]) -> Res<i32> {
    let mut check_only = false;
    let mut json_mode = crate::output::json_enabled();
    let mut pos = Vec::new();
    for a in args {
        match a.as_str() {
            "--check" => check_only = true,
            "--json" => json_mode = true,
            x if x.starts_with('-') => {
                return Err(unrecognized_arg(x, &["--check", "--json"], "sign"));
            }
            _ => pos.push(a.clone()),
        }
    }
    if pos.len() > 1 {
        return Err("usage: delegate sign [--check] [--json] [<run>]".into());
    }
    let reference = pos.first().map(String::as_str).unwrap_or("last");
    let run = runs::resolve_head(reference)?;
    let st = runs::state(&run);
    if runs::active(&st) {
        return Err(format!(
            "{} is still running; wait for it to finish before signing",
            run.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let meta = json(run.join("meta.json"));
    let run_id = events::run_id_for(&run, &meta);
    let review_doc = {
        let f = json(run.join("review.json"));
        if f.is_object() {
            f
        } else {
            json(run.join("summary.json"))["review"].clone()
        }
    };

    if check_only {
        if let Some(status) = sign_status(&run, &meta, &review_doc, true) {
            let valid = b(&status, "valid");
            let required = b(&status, "required");
            if json_mode {
                let mut obj = status.clone();
                obj["run"] = json!(run_id);
                println!("{obj}");
            } else if valid {
                println!(
                    "签字有效：{run_id} · 哈希 {} · 路径：{}",
                    s(&status, "hash"),
                    strings(&status["paths"]).join("、")
                );
            } else if b(&status, "invalidated") {
                println!(
                    "签字已失效：{run_id} · 原签字 {} ≠ 当前 {} · 路径：{}",
                    s(&status, "signedHash"),
                    s(&status, "hash"),
                    strings(&status["paths"]).join("、")
                );
            } else {
                println!(
                    "尚未签字：{run_id} · 当前哈希 {} · 路径：{}",
                    s(&status, "hash"),
                    strings(&status["paths"]).join("、")
                );
            }
            return Ok(if !required || valid { 0 } else { 1 });
        }
        // Also check live controllerReviewPaths even if review.json wasn't written (e.g. old run).
        if let Some(hit) = evaluate_controller_paths(&run, &meta, true)? {
            let obj = json!({
                "run": run_id,
                "required": true,
                "signed": false,
                "valid": false,
                "invalidated": false,
                "hash": hit.hash,
                "paths": hit.paths,
            });
            if json_mode {
                println!("{obj}");
            } else {
                println!(
                    "尚未签字：{run_id} · 当前哈希 {} · 路径：{}",
                    s(&obj, "hash"),
                    strings(&obj["paths"]).join("、")
                );
            }
            return Ok(1);
        }
        if json_mode {
            println!("{}", json!({"run": run_id, "required": false, "signed": false, "valid": true}));
        } else {
            println!("无需签字：{run_id}（未命中 controllerReviewPaths）");
        }
        return Ok(0);
    }

    let hit = evaluate_controller_paths(&run, &meta, true)?.or_else(|| {
        let cr = &review_doc["controllerReview"];
        if cr.is_object() && !s(cr, "hash").is_empty() {
            let diff = if !s(cr, "diff").is_empty() {
                s(cr, "diff").to_string()
            } else {
                read(s(cr, "diffFile"))
            };
            Some(ControllerReviewHit {
                paths: strings(&cr["paths"]),
                diff,
                hash: s(cr, "hash").to_string(),
            })
        } else {
            None
        }
    });
    let Some(hit) = hit else {
        return Err(format!(
            "{run_id} has no controllerReviewPaths changes to sign"
        ));
    };
    let signed_at = iso();
    let signed_by = caller().unwrap_or_else(|| "controller".into());
    let sign_doc = json!({
        "run": run_id,
        "hash": hit.hash,
        "paths": hit.paths,
        "signedAt": signed_at,
        "signedBy": signed_by,
    });
    write_json(run.join("sign.json"), &sign_doc)?;
    if !run.join("controller-review.diff").is_file() && !hit.diff.is_empty() {
        let _ = write(run.join("controller-review.diff"), &hit.diff);
    }
    crate::premerge::on_sign_success(&run, &meta);
    if json_mode {
        let mut out = sign_doc;
        out["valid"] = json!(true);
        println!("{out}");
    } else {
        println!(
            "已签字：{run_id} · 哈希 {} · 路径：{}",
            hit.hash,
            hit.paths.join("、")
        );
    }
    Ok(0)
}

/// Extract the complete chronological task brief across the `reply` chain (root -> each run's
/// delivered in-run/steer appends -> each reply -> current run, including archived ancestors).
/// Single runs with no delivered appends return their brief directly; chains with any appends or
/// replies concatenate every item in chronological order with an explicit precedence rule stating
/// that later controller decisions override earlier instructions.
fn original_task_brief(run: &Path, meta: &Value) -> String {
    enum BriefKind {
        Root,
        Reply(usize),
        Append(usize),
    }

    let mut chain = crate::runs::chain_ancestors_to_run(run);
    if chain.is_empty() {
        chain.push(run.to_path_buf());
    }
    let mut entries: Vec<(BriefKind, String, String)> = Vec::new();
    let mut reply_idx = 0usize;
    for dir in &chain {
        let node_meta = if dir == run && meta.is_object() {
            meta.clone()
        } else {
            json(dir.join("meta.json"))
        };
        let rid = crate::notes::run_id(dir, &node_meta);
        let extracted = crate::history::extract_user_prompt(dir, &node_meta);
        let text = if !extracted.trim().is_empty() {
            extracted.trim().to_string()
        } else {
            read(dir.join("prompt.md")).trim().to_string()
        };
        if !text.is_empty() {
            let kind = if entries.is_empty() {
                BriefKind::Root
            } else {
                reply_idx += 1;
                BriefKind::Reply(reply_idx)
            };
            entries.push((kind, rid.clone(), text));
        }
        let delivered_appends = crate::pending_reply::delivered_in_run_items(dir);
        for (idx, item) in delivered_appends.into_iter().enumerate() {
            let msg = item.message.trim();
            if !msg.is_empty() {
                let seq = if item.seq > 0 {
                    item.seq as usize
                } else {
                    idx + 1
                };
                entries.push((BriefKind::Append(seq), rid.clone(), msg.to_string()));
            }
        }
    }
    if entries.is_empty() {
        return String::new();
    }
    if entries.len() == 1 && matches!(entries[0].0, BriefKind::Root) {
        return entries.remove(0).2;
    }
    let last_idx = entries.len() - 1;
    let mut sections = Vec::with_capacity(entries.len() + 1);
    sections.push(
        "> 规则：以下按时间顺序列出初始说明与主控全部追加/续做回复，**后来的决定覆盖原说明**，\
         审查以最新决定为准；原说明与追加/续做冲突时，在审查结论里明说依据哪条。"
            .to_string(),
    );
    for (idx, (kind, rid, text)) in entries.into_iter().enumerate() {
        let is_last = idx == last_idx;
        let heading = match kind {
            BriefKind::Root => format!("### 初始说明（{rid}）"),
            BriefKind::Reply(n) if is_last => {
                format!("### 续做说明 #{n}（{rid}，最新——以本轮拍板/方向为准）")
            }
            BriefKind::Reply(n) => format!("### 续做说明 #{n}（{rid}）"),
            BriefKind::Append(seq) if is_last => {
                format!("### 主控追加 #{seq}（{rid}，最新——以本条决定/方向为准）")
            }
            BriefKind::Append(seq) => format!("### 主控追加 #{seq}（{rid}）"),
        };
        sections.push(format!("{heading}\n\n{text}"));
    }
    sections.join("\n\n")
}

fn split_diff_stanzas(diff: &str) -> Vec<&str> {
    let mut stanzas = Vec::new();
    let mut start: Option<usize> = None;
    let mut offset = 0usize;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            if let Some(s_idx) = start {
                stanzas.push(&diff[s_idx..offset]);
            }
            start = Some(offset);
        }
        offset += line.len();
    }
    if let Some(s_idx) = start {
        stanzas.push(&diff[s_idx..]);
    }
    stanzas
}

fn is_identical_copy_stanza(stanza: &str) -> bool {
    let mut has_sim_100 = false;
    let mut has_copy_from = false;
    let mut has_copy_to = false;
    for (idx, raw) in stanza.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if idx == 0 {
            if !line.starts_with("diff --git ") {
                return false;
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if line == "similarity index 100%" {
            has_sim_100 = true;
        } else if line.starts_with("copy from ") {
            has_copy_from = true;
        } else if line.starts_with("copy to ") {
            has_copy_to = true;
        } else {
            return false;
        }
    }
    has_sim_100 && has_copy_from && has_copy_to
}

fn unquote_git_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        trimmed[1..trimmed.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        trimmed.to_string()
    }
}

fn stanza_dest_path(stanza: &str) -> Option<String> {
    for line in stanza.lines() {
        let l = line.trim_end_matches('\r');
        if let Some(rest) = l.strip_prefix("copy to ") {
            return Some(unquote_git_path(rest));
        }
        if let Some(rest) = l.strip_prefix("rename to ") {
            return Some(unquote_git_path(rest));
        }
        if let Some(rest) = l.strip_prefix("+++ b/") {
            return Some(unquote_git_path(rest));
        }
    }
    let first = stanza.lines().next()?.trim_end_matches('\r');
    let rest = first.strip_prefix("diff --git ")?;
    if let Some((_, b_part)) = rest.rsplit_once(" b/") {
        return Some(unquote_git_path(b_part));
    }
    None
}

/// Strip 100%-identical pure-copy stanzas (`similarity index 100%` + `copy from`/`copy to` with
/// no hunks or mode changes) from a `git diff -M -C -C` output so directory copies only surface
/// files that actually changed. Returns `(filtered_diff, shown_file_count, omitted_copy_count)`.
pub fn strip_identical_copy_stanzas(diff: &str) -> (String, usize, usize) {
    let stanzas = split_diff_stanzas(diff);
    if stanzas.is_empty() {
        return (diff.to_string(), 0, 0);
    }
    let mut kept = Vec::new();
    let mut omitted = 0usize;
    for st in stanzas {
        if is_identical_copy_stanza(st) {
            omitted += 1;
        } else {
            kept.push(st);
        }
    }
    let shown = kept.len();
    let mut out = String::new();
    if omitted > 0 {
        if shown > 0 {
            out.push_str(&format!(
                "# 已省略 {omitted} 个与源文件 100% 完全一致的复制文件，以下仅显示有改动的文件\n\n"
            ));
        } else {
            out.push_str(&format!(
                "# 所有 {omitted} 个新增文件均为 100% 完全一致的复制，无内容改动\n"
            ));
        }
    }
    for st in kept {
        out.push_str(st);
    }
    (out, shown, omitted)
}

fn filter_diff_excluding_paths(diff: &str, excluded: &BTreeSet<String>) -> (String, usize) {
    if excluded.is_empty() {
        let count = split_diff_stanzas(diff).len();
        return (diff.to_string(), count);
    }
    let stanzas = split_diff_stanzas(diff);
    if stanzas.is_empty() {
        return (diff.to_string(), 0);
    }
    let mut out = String::new();
    // Preserve any leading comment header before the first `diff --git `.
    if let Some(pos) = diff.find("diff --git ") {
        if pos > 0 {
            out.push_str(&diff[..pos]);
        }
    }
    let mut shown = 0usize;
    for st in stanzas {
        let drop_it = stanza_dest_path(st).is_some_and(|p| excluded.contains(&p));
        if !drop_it {
            out.push_str(st);
            shown += 1;
        }
    }
    (out, shown)
}

/// Generate a reviewer-facing diff with rename & copy detection (`git diff -M -C -C`),
/// stripping 100%-identical pure copies and optionally excluding pure-mainline-advance files.
pub fn generate_reviewer_diff(
    top: &Path,
    from_tree: &str,
    to_tree: &str,
    exclude_paths: Option<&BTreeSet<String>>,
) -> (String, usize) {
    if !top.is_dir() || from_tree.is_empty() || to_tree.is_empty() {
        return (String::new(), 0);
    }
    let raw = git(
        top,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--color=never",
            "-M",
            "-C",
            "-C",
            from_tree,
            to_tree,
        ],
    )
    .map(|b| String::from_utf8_lossy(&b).into_owned())
    .unwrap_or_default();
    let (stripped, shown, _) = strip_identical_copy_stanzas(&raw);
    if let Some(ex) = exclude_paths.filter(|s| !s.is_empty()) {
        filter_diff_excluding_paths(&stripped, ex)
    } else {
        (stripped, shown)
    }
}

fn format_diff_for_prompt(diff_text: &str, diff_file: &Path, top: &Path, from_tree: &str, to_tree: &str) -> String {
    if diff_text.trim().is_empty() {
        return "（无改动 / empty diff）".to_string();
    }
    if diff_text.len() <= MAX_INLINE_DIFF_BYTES {
        format!(
            "完整 diff 文件：`{}`\n\n```diff\n{}\n```",
            diff_file.display(),
            diff_text.trim_end_matches('\n')
        )
    } else {
        let stat = git_text(top, &["diff", "--stat", "-M", "-C", "-C", from_tree, to_tree])
            .unwrap_or_default();
        let excerpt: String = diff_text.chars().take(60_000).collect();
        format!(
            "完整 diff 文件（{} 字节，过大已截断，请按需读取全文或对应源码文件）：`{}`\n\n```text\n{}\n```\n\n```diff\n{}\n...\n```",
            diff_text.len(),
            diff_file.display(),
            stat.trim(),
            excerpt.trim_end_matches('\n')
        )
    }
}

/// Build the full-scope initial review prompt (shared by `review.rs` and `fit_bench.rs`
/// so benchmark replays pass the exact same wording, workdir note, and diff formatting).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_full_review_prompt(
    use_ro_workspace: bool,
    workdir: &Path,
    diff_file: &Path,
    diff_text: &str,
    top: &Path,
    from_tree: &str,
    to_tree: &str,
    rubric_source: &str,
    rubric_text: &str,
    task_brief: &str,
) -> String {
    let workdir_note = format_review_workdir_note(use_ro_workspace, workdir, None, false);
    let diff_section = format_diff_for_prompt(diff_text, diff_file, top, from_tree, to_tree);
    let output_guide = "开头先给一句明确结论（通过 / 需返工 / 需主控决定，或 PASS / REWORK / NEEDS_CONTROLLER），下面自然写核查依据与重点关注位置。";
    format!(
        "请作为独立只读审查员审查以下写入任务的交付结果。\n\
         {workdir_note}\
         {REVIEW_ACCEPT_OWNED_NOTE}\n\
         严格遵守：不修改任何文件，不委派子任务；{output_guide}\n\n\
         ## 审查要点（{rubric_source}）\n\n\
         {}\n\n\
         ## 任务说明\n\n\
         {}\n\n\
         ## 本链净 diff（diff --total）\n\n\
         {diff_section}\n",
        rubric_text.trim(),
        task_brief.trim(),
    )
}

/// Build an isolated read-only checkout of `tree` for `agy` reviewers/scouts (which lack `--tools`).
pub(crate) fn prepare_ro_workspace(top: &Path, tree: &str, dest: &Path, scratch: &Path) -> Res<()> {
    if dest.exists() {
        let _ = fs::remove_dir_all(dest);
    }
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let index = scratch.join(format!(".ro-index-{}", std::process::id()));
    let res = (|| -> Res<()> {
        let out = git_command()
            .arg("-C")
            .arg(top)
            .args(["read-tree", tree])
            .env("GIT_INDEX_FILE", &index)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        let prefix = format!("{}/", dest.display());
        let out = git_command()
            .arg("-C")
            .arg(top)
            .args(["checkout-index", "-a", "-f", &format!("--prefix={prefix}")])
            .env("GIT_INDEX_FILE", &index)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(())
    })();
    let _ = fs::remove_file(&index);
    res
}

const RO_IGNORED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".venv",
    "__pycache__",
    "dist",
    "test-results",
];

fn has_ro_ignored_component(path: &str) -> bool {
    path.split('/').any(|seg| RO_IGNORED_DIRS.contains(&seg))
}

fn git_tracked_files(top: &Path) -> BTreeSet<String> {
    if !top.is_dir() {
        return BTreeSet::new();
    }
    let Ok(bytes) = git(top, &["ls-files", "-z"]) else {
        return BTreeSet::new();
    };
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

fn git_ignored_paths(top: &Path, candidates: &[String]) -> BTreeSet<String> {
    if candidates.is_empty() || !top.is_dir() {
        return BTreeSet::new();
    }
    let Ok(mut child) = git_command()
        .arg("-C")
        .arg(top)
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return BTreeSet::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(candidates.join("\0").as_bytes());
    }
    let Ok(out) = child.wait_with_output() else {
        return BTreeSet::new();
    };
    out.stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Filter candidate changed paths for a read-only reviewer/scout:
/// - Always flag git-tracked files.
/// - Exclude untracked dependency/build paths (`node_modules`, `target`, `.venv`,
///   `__pycache__`, `dist`, `test-results`) and paths ignored by `.gitignore`.
/// - Flag unignored new/modified files.
pub(crate) fn filter_ro_violations(top: &Path, paths: Vec<String>) -> Vec<String> {
    if paths.is_empty() {
        return paths;
    }
    let tracked = git_tracked_files(top);
    let mut violations = BTreeSet::new();
    let mut untracked = Vec::new();
    for p in paths {
        let norm = p.trim_start_matches("./").to_string();
        if norm.is_empty() {
            continue;
        }
        if tracked.contains(&norm) {
            violations.insert(norm);
        } else if !has_ro_ignored_component(&norm) {
            untracked.push(norm);
        }
    }
    let ignored = git_ignored_paths(top, &untracked);
    for p in untracked {
        if !ignored.contains(&p) {
            violations.insert(p);
        }
    }
    violations.into_iter().collect()
}

pub(crate) fn dir_manifest_for_repo(root: &Path, top: &Path) -> Res<BTreeMap<String, String>> {
    let tracked = git_tracked_files(top);
    fn visit(
        root: &Path,
        path: &Path,
        tracked: &BTreeSet<String>,
        out: &mut BTreeMap<String, String>,
    ) -> Res<()> {
        let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if meta.is_dir() {
            if path != root {
                let rel = path
                    .strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if RO_IGNORED_DIRS.contains(&name) {
                    let prefix = format!("{rel}/");
                    if !tracked.iter().any(|t| t.starts_with(&prefix)) {
                        return Ok(());
                    }
                }
            }
            for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
                visit(root, &entry.map_err(|e| e.to_string())?.path(), tracked, out)?;
            }
        } else {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned();
            let val = if meta.file_type().is_symlink() {
                format!(
                    "l:{}",
                    fs::read_link(path)
                        .map_err(|e| e.to_string())?
                        .to_string_lossy()
                )
            } else if meta.is_file() {
                let bytes = fs::read(path).map_err(|e| e.to_string())?;
                format!("f:{}:{}", bytes.len(), sha1_smol::Sha1::from(&bytes).digest())
            } else {
                "o".to_string()
            };
            out.insert(rel, val);
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    if root.is_dir() {
        visit(root, root, &tracked, &mut out)?;
    }
    Ok(out)
}

pub(crate) fn diff_manifests(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|k| before.get(*k) != after.get(*k))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn build_reviewer_meta(
    author_meta: &Value,
    review_cfg: &Value,
    review_dir: &Path,
    workdir: &Path,
    prompt_file: &Path,
    fork: Option<&str>,
) -> Value {
    let mut m = json!({
        "run": author_meta["run"],
        "ownerRunDir": author_meta["dir"],
        "name": format!("{} [review]", s(author_meta, "name")),
        "dir": review_dir,
        "workdir": workdir,
        "mode": "read-only",
        "shell": false,
        "isReviewer": true,
        "agent": review_cfg["agent"],
        "agentBin": review_cfg["agentBin"],
        "agentVersion": review_cfg["agentVersion"],
        "tier": review_cfg["tier"],
        "model": review_cfg["model"],
        "modelName": review_cfg["modelName"],
        "thinking": review_cfg["thinking"],
        "provider": Value::Null,
        "sessionDir": review_dir.join("session"),
        "fork": fork,
        "agentPromptFile": prompt_file,
        "images": [],
        "env": author_meta["env"],
        "agy": author_meta["agy"],
        "stall": author_meta["stall"],
        "loop": author_meta["loop"],
        "timeoutSeconds": author_meta["timeoutSeconds"],
        "top": author_meta["top"],
        "worktree": author_meta["worktree"],
        "agentDeny": [],
    });
    if s(&m, "agent").is_empty() {
        m["agent"] = json!(agents::AGENT);
    }
    if fork.is_some() {
        let owner_dir = s(author_meta, "dir");
        let pinned = if !owner_dir.is_empty() {
            s(&json(Path::new(owner_dir).join("meta.json")), "reviewAgyAccount").to_string()
        } else {
            s(author_meta, "reviewAgyAccount").to_string()
        };
        if !pinned.is_empty() {
            m["reviewAgyAccount"] = json!(pinned);
        }
    }
    m
}

fn find_session_continuation(dir: &Path, meta: &Value) -> Option<String> {
    let id = runs::events(dir)
        .into_iter()
        .rev()
        .find(|ev| s(ev, "e") == "session" && !s(ev, "id").is_empty())
        .map(|ev| s(&ev, "id").to_string())?;
    agents::continuation_target(meta, &id)
}

fn copy_pi_fork_if_needed(target: &str, dest_dir: &Path, is_agy: bool) -> Res<String> {
    if is_agy {
        return Ok(target.to_string());
    }
    let src = Path::new(target);
    if !src.is_file() {
        return Err(format!("missing previous review session file: {target}"));
    }
    let fork_dir = dest_dir.join("fork");
    fs::create_dir_all(&fork_dir).map_err(|e| e.to_string())?;
    let dst = fork_dir.join(src.file_name().unwrap_or_default());
    fs::copy(src, &dst).map_err(|e| e.to_string())?;
    let mut sidecar_name = src.as_os_str().to_owned();
    sidecar_name.push(".progress.jsonl");
    let sidecar = PathBuf::from(sidecar_name);
    if sidecar.is_file() {
        let mut dst_sidecar = dst.as_os_str().to_owned();
        dst_sidecar.push(".progress.jsonl");
        let _ = fs::copy(&sidecar, PathBuf::from(dst_sidecar));
    }
    Ok(dst.to_string_lossy().into_owned())
}

const MAX_PARSE_ERROR_CHARS: usize = 240;
const RO_VIOLATION_SAMPLE_MAX: usize = 5;
pub const REVIEW_VERDICT_ENV_ERROR: &str = "ENV_ERROR";
const REVIEW_ENV_ERROR_PREFIX: &str = "REVIEW_ENV_ERROR:";

pub fn review_env_error_retry_command(run_id: &str) -> String {
    format!("delegate reply {run_id} --review-full '重跑审查'")
}

/// Whether the reviewer can safely continue the previous review session on `current_workdir`.
///
/// - Different colleague drivers (`agy` vs Pi) can never share a session.
/// - `agy --conversation <id>` keeps the conversation's original Command Working Directory;
///   if `current_workdir` changed (e.g. a reply run using a new `review-ro/<run_id>` after the
///   parent run cleaned up its snapshot), continuing the old conversation causes `run_command`
///   to fail with `no such directory`. Start a fresh incremental session instead.
/// - Pi `--fork <session_file>` uses the spawned process's `current_dir`, so it can safely
///   switch to the new snapshot path (with the prompt explicitly naming the new path).
pub(crate) fn can_continue_review_session(
    is_agy_reviewer: bool,
    prev_continuation_is_agy: bool,
    prev_workdir: Option<&Path>,
    current_workdir: &Path,
) -> bool {
    if prev_continuation_is_agy != is_agy_reviewer {
        return false;
    }
    if is_agy_reviewer {
        return prev_workdir == Some(current_workdir);
    }
    true
}

pub(crate) fn format_review_workdir_note(
    use_ro_workspace: bool,
    workdir: &Path,
    prev_workdir: Option<&Path>,
    continuing_session: bool,
) -> String {
    let wd = workdir.display();
    if use_ro_workspace {
        if continuing_session && prev_workdir.is_some_and(|p| p != workdir) {
            format!(
                "本轮审查工作目录（只读快照，已切换至本轮新路径）：`{wd}`（上一轮快照已清理，请勿访问旧路径）。\n"
            )
        } else {
            format!("本轮审查工作目录（只读快照）：`{wd}`。\n")
        }
    } else if continuing_session {
        format!("本轮审查工作目录：`{wd}`。\n")
    } else {
        String::new()
    }
}

struct CleanupDir(PathBuf);
impl Drop for CleanupDir {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

/// Out-of-repo read-only checkout directory for `agy` reviewers so parent `.gitignore`
/// rules (such as `.local/`) never swallow `rg` / `fd` searches inside the checkout.
pub(crate) fn ro_workspace_dir(run_id: &str) -> PathBuf {
    cache_dir().join("review-ro").join(run_id)
}

fn compact_controller_review(cr: &Value) -> Option<Value> {
    if !cr.is_object() {
        return None;
    }
    let mut out = json!({
        "paths": cr["paths"],
        "hash": cr["hash"],
    });
    if !s(cr, "diffFile").is_empty() {
        out["diffFile"] = cr["diffFile"].clone();
    }
    if let Some(bytes) = cr["diffBytes"].as_u64() {
        out["diffBytes"] = json!(bytes);
    } else if let Some(diff) = cr["diff"].as_str() {
        out["diffBytes"] = json!(diff.len());
    }
    Some(out)
}

fn compact_extra_review(ex: &Value) -> Option<Value> {
    if !ex.is_object() {
        return None;
    }
    let mut out = serde_json::Map::new();
    for key in [
        "status",
        "model",
        "modelName",
        "verdict",
        "disagreed",
        "triggers",
        "skipReason",
        "fiveHourPct",
        "weeklyPct",
        "verdictFile",
        "conclusion",
    ] {
        if let Some(v) = ex.get(key) {
            if !v.is_null() {
                out.insert(key.to_string(), v.clone());
            }
        }
    }
    if let Some(err) = ex.get("error").and_then(Value::as_str).filter(|e| !e.is_empty()) {
        out.insert("error".into(), json!(clip(err, MAX_PARSE_ERROR_CHARS)));
    }
    Some(Value::Object(out))
}

/// Produce a bounded, index-safe summary from either a new 6.33 or legacy `review.json`.
/// Never inlines `rounds[]`, full `readOnlyViolation` lists, raw `controllerReview.diff`,
/// or unbounded `parseError` text.
pub fn compact_review_summary(doc: &Value) -> Value {
    if !doc.is_object() {
        return Value::Null;
    }
    let mut out = serde_json::Map::new();
    for key in [
        "status",
        "verdict",
        "scope",
        "scopeFiles",
        "syncSkip",
        "round",
        "reworkRounds",
        "model",
        "modelName",
        "thinking",
        "tier",
        "fallback",
        "needsAttention",
        "envError",
        "error",
        "retryCommand",
        "upgradedBy",
        "originalVerdict",
        "formatError",
        "formatRetried",
        "formatRetryFailed",
        "verdictFile",
        "conclusion",
        "focusCount",
        "cancelled",
        "cancelledBy",
        "stoppedDuringRework",
        "reworkOutcome",
        "roViolationCount",
        "roViolationSample",
        "roViolationFile",
        "tokens",
        "agyAccount",
    ] {
        if let Some(v) = doc.get(key) {
            if !v.is_null() {
                out.insert(key.to_string(), v.clone());
            }
        }
    }
    if let Some(err) = doc.get("parseError").and_then(Value::as_str).filter(|e| !e.is_empty()) {
        out.insert("parseError".into(), json!(clip(err, MAX_PARSE_ERROR_CHARS)));
    }
    // Backward compatibility with legacy review.json that stored raw `readOnlyViolation` arrays.
    if !out.contains_key("roViolationCount") {
        if let Some(arr) = doc.get("readOnlyViolation").and_then(Value::as_array) {
            if !arr.is_empty() {
                let sample: Vec<Value> = arr.iter().take(RO_VIOLATION_SAMPLE_MAX).cloned().collect();
                out.insert("roViolationCount".into(), json!(arr.len()));
                out.insert("roViolationSample".into(), Value::Array(sample));
            }
        }
    }
    if let Some(cr) = compact_controller_review(&doc["controllerReview"]) {
        out.insert("controllerReview".into(), cr);
    }
    if let Some(ex) = compact_extra_review(&doc["extra"]) {
        out.insert("extra".into(), ex);
    }
    Value::Object(out)
}

fn build_controller_review_json(run: &Path, hit: &ControllerReviewHit) -> Res<Value> {
    let diff_file = run.join("controller-review.diff");
    write(&diff_file, &hit.diff)?;
    Ok(json!({
        "paths": hit.paths,
        "hash": hit.hash,
        "diffFile": diff_file.to_string_lossy(),
        "diffBytes": hit.diff.len(),
    }))
}

fn apply_accept_failure_state(
    run: &Path,
    meta: &Value,
    accept_doc: &mut Value,
    state: &mut String,
    sum: &mut Value,
) {
    if lane::stopped() {
        *state = "stopped".into();
        sum["state"] = json!("stopped");
        return;
    }
    if crate::accept::is_mainline_blocked(accept_doc) {
        accept_doc["mainlineBlocked"] = json!(true);
        sum["mainlineBlock"] = crate::accept::build_mainline_block(run, meta, accept_doc);
        *state = "blocked_mainline".into();
        sum["state"] = json!("blocked_mainline");
    } else {
        *state = "rejected".into();
        sum["state"] = json!("rejected");
    }
}

fn emit_accept_event(run: &Path, meta: &Value, accept_doc: &Value) {
    let mut payload = json!({
        "name": s(meta, "name"),
        "ok": b(accept_doc, "ok"),
        "exitCode": accept_doc["exitCode"],
        "command": clip(s(accept_doc, "command"), 120),
        "notExecuted": b(accept_doc, "notExecuted"),
        "infrastructureFailure": b(accept_doc, "infrastructureFailure"),
    });
    if b(accept_doc, "tiered") {
        payload["tiered"] = json!(true);
        payload["failClosed"] = accept_doc["failClosed"].clone();
        payload["ranLayers"] = accept_doc["ranLayers"].clone();
        payload["skippedLayers"] = accept_doc["skippedLayers"].clone();
    }
    if b(accept_doc, "parallelWithReview") {
        payload["parallelWithReview"] = json!(true);
    }
    if b(accept_doc, "cancelled") {
        payload["cancelled"] = json!(true);
        payload["cancelledBy"] = accept_doc["cancelledBy"].clone();
    }
    if !b(accept_doc, "notExecuted") {
        let elapsed = accept_doc["elapsedSeconds"].as_f64().unwrap_or(0.0);
        let queued = accept_doc["queuedSeconds"].as_f64().unwrap_or(0.0);
        payload["elapsedSeconds"] = json!((elapsed * 100.0).round() / 100.0);
        payload["queuedSeconds"] = json!((queued * 100.0).round() / 100.0);
        if accept_doc["layerSeconds"].is_object() {
            payload["layerSeconds"] = accept_doc["layerSeconds"].clone();
        } else if let Some(layers) = accept_doc["layers"].as_array() {
            let mut ls = serde_json::Map::new();
            for l in layers {
                if let Some(secs) = l["seconds"].as_f64() {
                    let lname = s(l, "name");
                    if !lname.is_empty() {
                        ls.insert(lname.to_string(), json!((secs * 100.0).round() / 100.0));
                    }
                }
            }
            if !ls.is_empty() {
                payload["layerSeconds"] = Value::Object(ls);
            }
        }
    }
    events::emit(
        &events::run_id_for(run, meta),
        &events::repo_for_meta(meta),
        "accept",
        payload,
    );
}

fn diff_tree_paths(top: &Path, from_tree: &str, to_tree: &str) -> Option<Vec<String>> {
    if !top.is_dir() || from_tree.is_empty() || to_tree.is_empty() {
        return None;
    }
    let out = git(
        top,
        &["diff", "--name-only", "--no-renames", from_tree, to_tree],
    )
    .ok()?;
    Some(
        String::from_utf8_lossy(&out)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn synthetic_commit_tree(top: &Path, tree: &str, parent: Option<&str>) -> Option<String> {
    let mut cmd = git_command();
    cmd.arg("-C").arg(top).arg("commit-tree").arg(tree);
    if let Some(p) = parent {
        cmd.args(["-p", p]);
    }
    cmd.args(["-m", "delegate-range-diff"]);
    for who in ["AUTHOR", "COMMITTER"] {
        cmd.env(format!("GIT_{who}_NAME"), "delegate")
            .env(format!("GIT_{who}_EMAIL"), "delegate@localhost")
            .env(format!("GIT_{who}_DATE"), "2020-01-01T00:00:00Z");
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Deterministic Host check (zero LLM calls): returns `true` iff `git range-diff`
/// between `<old_base>..<old_head>` and `<new_base>..<new_head>` has no patch differences.
pub fn range_diff_is_empty(
    top: &Path,
    old_base: &str,
    old_head: &str,
    new_base: &str,
    new_head: &str,
) -> bool {
    if !top.is_dir()
        || old_base.is_empty()
        || old_head.is_empty()
        || new_base.is_empty()
        || new_head.is_empty()
    {
        return false;
    }
    if old_base == old_head && new_base == new_head {
        return true;
    }
    let Some(cb1) = synthetic_commit_tree(top, old_base, None) else {
        return false;
    };
    let Some(ch1) = synthetic_commit_tree(top, old_head, Some(&cb1)) else {
        return false;
    };
    let Some(cb2) = synthetic_commit_tree(top, new_base, None) else {
        return false;
    };
    let Some(ch2) = synthetic_commit_tree(top, new_head, Some(&cb2)) else {
        return false;
    };
    let Ok(out) = git_command()
        .arg("-C")
        .arg(top)
        .args([
            "range-diff",
            "--no-color",
            &format!("{cb1}..{ch1}"),
            &format!("{cb2}..{ch2}"),
        ])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut saw_equal = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Indented lines are diff hunks; `!`, `<`, `>` indicate changed/added/removed commits.
        if line.starts_with(' ') || line.starts_with('\t') {
            return false;
        }
        if line.contains(" ! ") || line.contains(" < ") || line.contains(" > ") {
            return false;
        }
        if line.contains(" = ") {
            saw_equal = true;
        } else {
            return false;
        }
    }
    saw_equal
}

/// Deterministic Host check for `--sync=agent` conflict resolution:
/// returns `true` iff `before_bytes` had conflict markers (`<<<<<<< ... ======= ... >>>>>>>`),
/// `after_bytes` has NO remaining conflict markers, and EVERY non-conflict line segment outside
/// the conflict blocks is preserved verbatim in `after_bytes` in the exact same order.
pub fn conflict_resolution_only_touched_conflicts(
    before_bytes: &[u8],
    after_bytes: &[u8],
) -> bool {
    if !worktree::conflict_labels_in_bytes(Path::new("conflict.txt"), after_bytes).is_empty() {
        return false;
    }
    let before = String::from_utf8_lossy(before_bytes);
    let after = String::from_utf8_lossy(after_bytes);
    let mut segments: Vec<Vec<&str>> = vec![Vec::new()];
    let mut in_conflict = false;
    let mut saw_divider = false;
    let mut conflict_count = 0usize;
    for line in before.lines() {
        if !in_conflict {
            if line.starts_with("<<<<<<< ") {
                in_conflict = true;
                saw_divider = false;
            } else {
                segments.last_mut().unwrap().push(line);
            }
        } else if line == "=======" {
            saw_divider = true;
        } else if line.starts_with(">>>>>>> ") && saw_divider {
            in_conflict = false;
            saw_divider = false;
            conflict_count += 1;
            segments.push(Vec::new());
        }
    }
    if in_conflict || conflict_count == 0 {
        return false;
    }
    let after_lines: Vec<&str> = after.lines().collect();
    let first = &segments[0];
    let last = segments.last().unwrap();
    if after_lines.len() < first.len() + last.len() {
        return false;
    }
    if &after_lines[..first.len()] != first.as_slice() {
        return false;
    }
    let end_limit = after_lines.len() - last.len();
    if &after_lines[end_limit..] != last.as_slice() {
        return false;
    }
    let mut pos = first.len();
    for seg in &segments[1..segments.len() - 1] {
        if seg.is_empty() {
            continue;
        }
        let Some(rel) = after_lines[pos..end_limit]
            .windows(seg.len())
            .position(|w| w == seg.as_slice())
        else {
            return false;
        };
        pos += rel + seg.len();
    }
    true
}

#[derive(Debug, Clone)]
struct InheritedReviewState {
    reviewed_tree: String,
    reviewed_base_tree: String,
    verdict: String,
    verdict_file: Option<String>,
    verdict_text: String,
    continuation: Option<String>,
    continuation_is_agy: bool,
    review_workdir: Option<PathBuf>,
    agy_account: Option<String>,
}

/// Walk the `reply` ancestor chain (stopping at `newAfterApply`) to inherit the latest
/// completed review's reviewed tree, baseline tree, verdict text, and reviewer session (P10).
fn load_inherited_review(run: &Path, meta: &Value) -> Option<InheritedReviewState> {
    if b(meta, "newAfterApply") {
        return None;
    }
    let chain = crate::runs::chain_ancestors_to_run(run);
    if chain.len() <= 1 {
        return None;
    }
    for anc in chain[..chain.len() - 1].iter().rev() {
        let anc_meta = json(anc.join("meta.json"));
        let rev = {
            let f = json(anc.join("review.json"));
            if f.is_object() {
                f
            } else {
                json(anc.join("summary.json"))["review"].clone()
            }
        };
        let verdict = s(&rev, "verdict").to_string();
        if rev.is_object() && matches!(verdict.as_str(), "PASS" | "REWORK" | "NEEDS_CONTROLLER") {
            let is_sync_skip = b(&rev, "syncSkip") || s(&rev, "scope") == "sync_skip";
            let upgraded = s(&rev, "upgradedBy");
            let explicit_tree = s(&rev, "reviewedTree");
            let has_no_executed_rounds = !is_sync_skip
                && (n(&rev, "round") == 0
                    || rev
                        .get("rounds")
                        .and_then(Value::as_array)
                        .is_some_and(|a| a.is_empty()));
            let is_unreviewed_error = matches!(
                upgraded,
                "rubricError" | "reviewerError" | "formatError" | "readOnlyViolation"
            ) || b(&rev, "formatError");
            if has_no_executed_rounds
                || (explicit_tree.is_empty()
                    && (b(&rev, "stoppedDuringRework") || is_unreviewed_error))
            {
                if b(&anc_meta, "newAfterApply") {
                    break;
                }
                continue;
            }
            let anc_changes = json(anc.join("changes.json"));
            let reviewed_tree = if !explicit_tree.is_empty() {
                explicit_tree.to_string()
            } else {
                s(&anc_changes, "after").to_string()
            };
            if reviewed_tree.is_empty() {
                if b(&anc_meta, "newAfterApply") {
                    break;
                }
                continue;
            }
            let anc_top = {
                let t = PathBuf::from(s(&anc_changes, "top"));
                if t.is_dir() {
                    t
                } else {
                    PathBuf::from(s(&anc_meta, "top"))
                }
            };
            let reviewed_base_tree = {
                let t = s(&rev, "reviewedBaseTree");
                if !t.is_empty() {
                    t.to_string()
                } else if anc_top.is_dir() {
                    worktree::effective_total_base(&anc_top, &anc_meta).0
                } else {
                    s(&anc_meta, "chainBase").to_string()
                }
            };
            let verdict_file = {
                let vf = s(&rev, "verdictFile");
                if !vf.is_empty() && Path::new(vf).is_file() {
                    Some(vf.to_string())
                } else {
                    let fallback = anc.join("review.md");
                    fallback
                        .is_file()
                        .then(|| fallback.to_string_lossy().into_owned())
                }
            };
            let verdict_text = verdict_file
                .as_deref()
                .map(read)
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| {
                    let c = s(&rev, "conclusion");
                    if !c.is_empty() {
                        format!("{verdict}\n{c}")
                    } else {
                        verdict.clone()
                    }
                });
            let mut continuation = {
                let c = s(&rev, "reviewContinuation");
                (!c.is_empty()).then(|| c.to_string())
            };
            let continuation_is_agy = rev["reviewContinuationIsAgy"]
                .as_bool()
                .unwrap_or_else(|| crate::agy::is_agy(s(&rev, "model")));
            if continuation.is_none() {
                let last_round = n(&rev, "round");
                if last_round > 0 {
                    let rdir = anc.join("review").join(format!("round-{last_round}"));
                    let probe_meta = json!({
                        "agent": if continuation_is_agy { "agy" } else { agents::AGENT },
                        "model": rev["model"],
                        "sessionDir": rdir.join("session"),
                    });
                    continuation = find_session_continuation(&rdir, &probe_meta);
                }
            }
            let review_workdir = {
                let wd = s(&rev, "reviewWorkdir");
                (!wd.is_empty()).then(|| PathBuf::from(wd))
            };
            let agy_account = {
                let a = s(&anc_meta, "reviewAgyAccount");
                if !a.is_empty() {
                    Some(a.to_string())
                } else {
                    let ra = s(&rev, "agyAccount");
                    (!ra.is_empty()).then(|| ra.to_string())
                }
            };
            return Some(InheritedReviewState {
                reviewed_tree,
                reviewed_base_tree,
                verdict,
                verdict_file,
                verdict_text,
                continuation,
                continuation_is_agy,
                review_workdir,
                agy_account,
            });
        }
        if b(&anc_meta, "newAfterApply") {
            break;
        }
    }
    None
}

/// Deterministic Host decision (zero LLM calls) for whether a child run is a pure sync round
/// that may skip review and carry forward the parent's `PASS` (only after acceptance is all green).
fn evaluate_pure_sync_round(
    top: &Path,
    run: &Path,
    inherited: &InheritedReviewState,
    before_tree: &str,
    after_tree: &str,
    new_base_tree: &str,
    review_full: bool,
) -> bool {
    if review_full || inherited.verdict != "PASS" {
        return false;
    }
    if !top.is_dir()
        || inherited.reviewed_base_tree.is_empty()
        || inherited.reviewed_tree.is_empty()
        || new_base_tree.is_empty()
        || after_tree.is_empty()
    {
        return false;
    }
    // Must actually be a baseline advance (sync).
    if inherited.reviewed_base_tree == new_base_tree {
        return false;
    }
    let Some(mainline_files) = diff_tree_paths(top, &inherited.reviewed_base_tree, new_base_tree)
    else {
        return false;
    };
    let Some(branch_files) =
        diff_tree_paths(top, &inherited.reviewed_base_tree, &inherited.reviewed_tree)
    else {
        return false;
    };
    let mainline_set: BTreeSet<String> = mainline_files.into_iter().collect();
    let branch_set: BTreeSet<String> = branch_files.into_iter().collect();
    let overlap: BTreeSet<String> = mainline_set.intersection(&branch_set).cloned().collect();

    let sync_doc = json(run.join("sync.json"));
    let sync_conflicts: BTreeSet<String> = strings(&sync_doc["conflicts"]).into_iter().collect();

    if sync_conflicts.is_empty() {
        // Mainline and branch touched the same file (even if auto-merged cleanly) -> incremental review.
        if !overlap.is_empty() {
            return false;
        }
        return range_diff_is_empty(
            top,
            &inherited.reviewed_base_tree,
            &inherited.reviewed_tree,
            new_base_tree,
            after_tree,
        );
    }

    // Conflict resolution (`--sync=agent`):
    // 1. Any non-conflict overlapping file still requires incremental review.
    if overlap.iter().any(|p| !sync_conflicts.contains(p)) {
        return false;
    }
    // 2. Author may only touch the conflicted files between post-sync `before_tree` and `after_tree`.
    if before_tree.is_empty() {
        return false;
    }
    let Some(author_changed) = diff_tree_paths(top, before_tree, after_tree) else {
        return false;
    };
    if author_changed.iter().any(|p| !sync_conflicts.contains(p)) {
        return false;
    }
    // 3. Inside every conflicted file, no non-conflict line may be modified.
    for path in &sync_conflicts {
        let Ok(before_bytes) = git(top, &["show", &format!("{before_tree}:{path}")]) else {
            return false;
        };
        let Ok(after_bytes) = git(top, &["show", &format!("{after_tree}:{path}")]) else {
            return false;
        };
        if !conflict_resolution_only_touched_conflicts(&before_bytes, &after_bytes) {
            return false;
        }
    }
    true
}

/// Files introduced solely by a clean mainline baseline advance (touched in `old_base..new_base`,
/// not touched by our branch `old_base..old_head`, and not touched by the author in `before..after`).
/// Excluded from incremental sync-round diffs so unrelated mainline commits don't pollute review.
fn pure_mainline_advance_paths(
    top: &Path,
    old_base: &str,
    old_head: &str,
    new_base: &str,
    before_tree: &str,
    after_tree: &str,
) -> BTreeSet<String> {
    if !top.is_dir()
        || old_base.is_empty()
        || new_base.is_empty()
        || old_base == new_base
        || old_head.is_empty()
    {
        return BTreeSet::new();
    }
    let Some(mainline) = diff_tree_paths(top, old_base, new_base) else {
        return BTreeSet::new();
    };
    let branch: BTreeSet<String> = diff_tree_paths(top, old_base, old_head)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let author: BTreeSet<String> = if !before_tree.is_empty() && !after_tree.is_empty() {
        diff_tree_paths(top, before_tree, after_tree)
            .unwrap_or_default()
            .into_iter()
            .collect()
    } else {
        BTreeSet::new()
    };
    mainline
        .into_iter()
        .filter(|p| !branch.contains(p) && !author.contains(p))
        .collect()
}

fn check_controller_paths_when_review_off(
    run: &Path,
    meta: &Value,
    state: &str,
    sum: &mut Value,
) -> Res<()> {
    if !matches!(state, "delivered" | "answered") {
        return Ok(());
    }
    if let Some(hit) = evaluate_controller_paths(run, meta, false)? {
        let cr_json = build_controller_review_json(run, &hit)?;
        let now = iso();
        let review_doc = json!({
            "status": "done",
            "verdict": "NEEDS_CONTROLLER",
            "round": 0,
            "reworkRounds": 0,
            "needsAttention": true,
            "upgradedBy": "controllerReviewPaths",
            "controllerReview": cr_json,
            "rounds": [],
            "startedAt": now,
            "updatedAt": now,
            "finishedAt": now,
        });
        write_json(run.join("review.json"), &review_doc)?;
        sum["review"] = compact_review_summary(&review_doc);
        let run_id = events::run_id_for(run, meta);
        let repo = events::repo_for_meta(meta);
        let name = s(meta, "name");
        events::emit(
            &run_id,
            &repo,
            "review_verdict",
            json!({
                "name": name,
                "round": 0,
                "verdict": "NEEDS_CONTROLLER",
                "upgradedBy": "controllerReviewPaths",
                "controllerReviewPaths": hit.paths,
                "signHash": hit.hash,
            }),
        );
        events::emit(
            &run_id,
            &repo,
            "needs_attention",
            json!({
                "name": name,
                "round": 0,
                "reworkRounds": 0,
                "reason": "controllerReviewPaths",
                "verdict": "NEEDS_CONTROLLER",
                "controllerReviewPaths": hit.paths,
                "signHash": hit.hash,
                "detail": format!("改动命中 controllerReviewPaths（{}），需主控审查并签字", hit.paths.join("、")),
            }),
        );
    }
    Ok(())
}

struct ReviewRoundOutcome {
    cancelled: bool,
    stopped: bool,
    verdict_str: String,
    raw_answer: String,
    raw_first_line: String,
    format_error: bool,
    format_retried: bool,
    format_retry_failed: bool,
    env_error: Option<String>,
    parse_error: Option<String>,
    upgraded_by: Option<String>,
    ro_violations: Vec<String>,
    ro_violation_sample: Vec<String>,
    ro_violation_file: Option<PathBuf>,
    verdict_file: PathBuf,
    diff_file: PathBuf,
    usage_dir: PathBuf,
    round_started: String,
    round_finished: String,
    next_continuation: Option<String>,
    conclusion: Option<String>,
    focus_count: usize,
}

struct ExtraRoundOutcome {
    cancelled: bool,
    verdict: Option<String>,
    raw_answer: String,
    conclusion: Option<String>,
    error: Option<String>,
}

enum ExtraPreDecision {
    None,
    Skipped {
        model: String,
        model_name: String,
        triggers: Vec<String>,
        five_hour_pct: Option<f64>,
        weekly_pct: Option<f64>,
        reason: String,
    },
    Error {
        model: String,
        model_name: String,
        triggers: Vec<String>,
        error: String,
    },
    Run {
        model: String,
        model_name: String,
        triggers: Vec<String>,
        extra_dir: PathBuf,
        extra_meta: Value,
        verdict_file: PathBuf,
    },
}

pub(crate) struct AgentAttemptWithRetry {
    pub(crate) res: Res<(String, String)>,
    format_retried: bool,
    format_retry_failed: bool,
}

fn attach_review_model_fields(doc: &mut Value, active_cfg: &Value, fallback: Option<&Value>) {
    doc["model"] = active_cfg["model"].clone();
    if !s(active_cfg, "modelName").is_empty() {
        doc["modelName"] = active_cfg["modelName"].clone();
    }
    if !s(active_cfg, "thinking").is_empty() {
        doc["thinking"] = active_cfg["thinking"].clone();
    }
    doc["tier"] = active_cfg["tier"].clone();
    if let Some(fb) = fallback {
        doc["fallback"] = fb.clone();
    }
}

fn run_agent_and_cleanup(
    meta: &Value,
    exec_dir: &Path,
    attempt: i64,
    holder: Arc<AtomicI32>,
    grace: Arc<Mutex<Option<f64>>>,
    cancel: Arc<AtomicBool>,
) -> Res<(String, String)> {
    let res = agents::run_agent_cancellable(meta, exec_dir, attempt, holder, grace, Some(cancel));
    let pgid = read(exec_dir.join("agent.pid")).trim().parse().unwrap_or(0);
    crate::cleanup::record(exec_dir, pgid);
    if pgid > 0 && group_members(pgid) {
        end_group(pgid, 1.0);
    }
    res
}

/// Run a single reviewer attempt without any format-retry follow-up call.
/// If the reviewer hits the repeated-polling self-check (`loop_stopped`), append
/// `LOOP_STOP_RETRY_PROMPT` ONCE at the tail of the same session so it can conclude directly.
/// If no clear verdict is recognized on a normal reply, the caller preserves `review.md` verbatim
/// and falls back safely to `NEEDS_CONTROLLER` with `VERDICT_MISSING_NOTE`.
pub(crate) fn run_reviewer_with_loop_stop_retry(
    review_meta: &Value,
    exec_dir: &Path,
    holder: Arc<AtomicI32>,
    grace: Arc<Mutex<Option<f64>>>,
    cancel: Arc<AtomicBool>,
) -> AgentAttemptWithRetry {
    let wd = Path::new(s(review_meta, "workdir"));
    if wd.as_os_str().is_empty() || !wd.is_dir() {
        return AgentAttemptWithRetry {
            res: Err(format!(
                "{REVIEW_ENV_ERROR_PREFIX}只读审查快照缺失（{}）",
                wd.display()
            )),
            format_retried: false,
            format_retry_failed: false,
        };
    }
    let first = run_agent_and_cleanup(
        review_meta,
        exec_dir,
        1,
        holder.clone(),
        grace.clone(),
        cancel.clone(),
    );
    if cancel.load(Ordering::SeqCst) || lane::stopped() {
        return AgentAttemptWithRetry {
            res: first,
            format_retried: false,
            format_retry_failed: false,
        };
    }
    let is_loop_stopped = matches!(&first, Ok((outcome, _)) if outcome == "loop_stopped");
    if !is_loop_stopped {
        return AgentAttemptWithRetry {
            res: first,
            format_retried: false,
            format_retry_failed: false,
        };
    }
    let retry_prompt_file = exec_dir.join("loop-stop-retry-prompt.md");
    let next_meta = write(&retry_prompt_file, format!("{LOOP_STOP_RETRY_PROMPT}\n"))
        .ok()
        .and_then(|()| supervise::continuation_with_file(review_meta, exec_dir, &retry_prompt_file));
    let Some(retry_meta) = next_meta else {
        return AgentAttemptWithRetry {
            res: first,
            format_retried: false,
            format_retry_failed: false,
        };
    };
    supervise::log_event(
        exec_dir,
        json!({
            "e": "loop_stop_retry",
            "n": 1,
            "detail": "审查员反复轮询触发自检，已终止轮询并同会话尾部补问 1 次直接给结论",
        }),
    );
    let second = run_agent_and_cleanup(&retry_meta, exec_dir, 2, holder, grace, cancel);
    let retry_failed = match &second {
        Ok((outcome, text)) if outcome == "ok" => {
            matches!(parse_verdict(text), ParsedVerdict::Invalid(_))
        }
        Ok((outcome, _)) if outcome == "cancelled" => false,
        _ => true,
    };
    AgentAttemptWithRetry {
        res: second,
        format_retried: true,
        format_retry_failed: retry_failed,
    }
}

fn apply_fallback_cfg(current: &Value, fallback: &Value) -> Value {
    let mut next = fallback.clone();
    next["enabled"] = json!(true);
    if !current["rubric"].is_null() {
        next["rubric"] = current["rubric"].clone();
    }
    if current["extra"].is_object() {
        next["extra"] = current["extra"].clone();
    }
    next
}

/// Entry point called from `supervise::inner` after `settle_pre_accept()` (and escalation)
/// and before `finalize_post_accept()` / `card::generate_and_save()` / `finish_run()`.
///
/// Orchestrates tiered acceptance (L1 fast -> L2 slow || read-only Reviewer in parallel),
/// mutual cancellation (a cancelled reviewer is NEVER recorded as `PASS`), and rework
/// incremental layer re-execution.
#[allow(clippy::too_many_arguments)]
pub fn run_accept_and_review(
    meta: &mut Value,
    run: &Path,
    can_accept: bool,
    state: &mut String,
    sum: &mut Value,
    rec: &mut Option<(Vec<Value>, Value)>,
    changed: &mut Vec<String>,
    answer: &mut String,
    attempts: &mut i64,
    policy_refused: &mut bool,
    policy_rewrite_attempted: &mut bool,
    holder: Arc<AtomicI32>,
    grace: Arc<Mutex<Option<f64>>>,
) -> Res<()> {
    if lane::stopped() || s(meta, "mode") != "write" || b(sum, "question") {
        return Ok(());
    }
    let review_cfg = meta["reviewConfig"].clone();
    let review_enabled = review_cfg.is_object() && b(&review_cfg, "enabled");
    let has_accept = crate::accept::has_acceptance(meta);

    if !can_accept {
        return Ok(());
    }

    // No acceptance configured: preserve "answered" state and check controllerReviewPaths.
    if !has_accept {
        if !review_enabled {
            check_controller_paths_when_review_off(run, meta, state, sum)?;
        }
        return Ok(());
    }

    // Record first-entry accept/review ETA prediction for online milestone calibration.
    crate::eta::record_pred_accept_review(run, meta);

    // Acceptance configured, auto-review OFF: run tiered fast -> slow sequentially.
    if !review_enabled {
        let passed = BTreeMap::new();
        let plan = crate::accept::plan(meta, changed, rec.is_some(), None, &passed);
        let (excluded, before_snap) = crate::accept::snapshot_for_accept(meta, run);
        let fast_outcome = if !plan.fast.is_empty() {
            Some(crate::accept::run_phase(
                meta,
                run,
                &plan.fast,
                false,
                holder.as_ref(),
                None,
                None,
            ))
        } else {
            None
        };
        let fast_ok = fast_outcome.as_ref().is_none_or(|f| f.ok);
        let slow_outcome = if fast_ok && !plan.slow.is_empty() {
            Some(crate::accept::run_phase(
                meta,
                run,
                &plan.slow,
                fast_outcome.is_some(),
                holder.as_ref(),
                None,
                None,
            ))
        } else {
            None
        };
        let (_, after_snap) = crate::accept::snapshot_for_accept(meta, run);
        let mut accept_doc = crate::accept::build_accept_summary(
            meta,
            run,
            &plan,
            fast_outcome.as_ref(),
            slow_outcome.as_ref(),
            false,
            excluded,
            before_snap.as_ref(),
            after_snap.as_ref(),
        );
        if b(&accept_doc, "infrastructureFailure") {
            sum["infrastructureFailure"] = json!(true);
            sum["error"] = json!("remote infrastructure failure; retry does not spend a rework round");
        }
        if b(&accept_doc, "ok") {
            *state = "delivered".into();
            sum["state"] = json!("delivered");
        } else {
            apply_accept_failure_state(run, meta, &mut accept_doc, state, sum);
        }
        emit_accept_event(run, meta, &accept_doc);
        sum["accept"] = accept_doc;
        check_controller_paths_when_review_off(run, meta, state, sum)?;
        return Ok(());
    }

    // Acceptance + auto-review ON: L1 fast -> L2 (slow || read-only Reviewer in parallel) + rework loop.
    let run_id = events::run_id_for(run, meta);
    let repo = events::repo_for_meta(meta);
    let run_name = s(meta, "name").to_string();
    let review_root = run.join("review");
    fs::create_dir_all(&review_root).map_err(|e| e.to_string())?;
    let ro_workspace = ro_workspace_dir(&run_id);
    let _ro_cleanup = CleanupDir(ro_workspace.clone());

    let (rubric_text, rubric_source) = match load_rubric(&review_cfg) {
        Ok(pair) => pair,
        Err(err) => {
            // Still run acceptance before escalating rubricError so acceptance state is known.
            let passed = BTreeMap::new();
            let plan = crate::accept::plan(meta, changed, rec.is_some(), None, &passed);
            let (excluded, before_snap) = crate::accept::snapshot_for_accept(meta, run);
            let fast_outcome = if !plan.fast.is_empty() {
                Some(crate::accept::run_phase(
                    meta,
                    run,
                    &plan.fast,
                    false,
                    holder.as_ref(),
                    None,
                    None,
                ))
            } else {
                None
            };
            let fast_ok = fast_outcome.as_ref().is_none_or(|f| f.ok);
            let slow_outcome = if fast_ok && !plan.slow.is_empty() {
                Some(crate::accept::run_phase(
                    meta,
                    run,
                    &plan.slow,
                    fast_outcome.is_some(),
                    holder.as_ref(),
                    None,
                    None,
                ))
            } else {
                None
            };
            let (_, after_snap) = crate::accept::snapshot_for_accept(meta, run);
            let mut accept_doc = crate::accept::build_accept_summary(
                meta,
                run,
                &plan,
                fast_outcome.as_ref(),
                slow_outcome.as_ref(),
                false,
                excluded,
                before_snap.as_ref(),
                after_snap.as_ref(),
            );
            let acc_ok = b(&accept_doc, "ok");
            if acc_ok {
                *state = "delivered".into();
                sum["state"] = json!("delivered");
            } else {
                apply_accept_failure_state(run, meta, &mut accept_doc, state, sum);
            }
            emit_accept_event(run, meta, &accept_doc);
            sum["accept"] = accept_doc;
            if !acc_ok {
                return Ok(());
            }
            let now = iso();
            let err_short = clip(&err, MAX_PARSE_ERROR_CHARS);
            let mut review_doc = json!({
                "status": "done",
                "verdict": "NEEDS_CONTROLLER",
                "round": 1,
                "reworkRounds": 0,
                "needsAttention": true,
                "upgradedBy": "rubricError",
                "parseError": err_short,
                "rounds": [],
                "startedAt": now,
                "updatedAt": now,
                "finishedAt": now,
            });
            attach_review_model_fields(
                &mut review_doc,
                &review_cfg,
                review_cfg.get("launchFallback").filter(|v| v.is_object()),
            );
            write_json(run.join("review.json"), &review_doc)?;
            sum["review"] = compact_review_summary(&review_doc);
            events::emit(
                &run_id,
                &repo,
                "review_verdict",
                json!({
                    "name": run_name,
                    "round": 1,
                    "verdict": "NEEDS_CONTROLLER",
                    "upgradedBy": "rubricError",
                    "error": err,
                }),
            );
            events::emit(
                &run_id,
                &repo,
                "needs_attention",
                json!({
                    "name": run_name,
                    "round": 1,
                    "reworkRounds": 0,
                    "reason": "rubricError",
                    "verdict": "NEEDS_CONTROLLER",
                    "detail": err,
                }),
            );
            return Ok(());
        }
    };

    let task_brief = original_task_brief(run, meta);
    let review_full = b(meta, "reviewFull");
    let inherited_review = (!review_full)
        .then(|| load_inherited_review(run, meta))
        .flatten();
    if let Some(acct) = inherited_review
        .as_ref()
        .and_then(|inh| inh.agy_account.as_deref())
    {
        if s(meta, "reviewAgyAccount").is_empty() {
            meta["reviewAgyAccount"] = json!(acct);
            let _ = write_json(run.join("meta.json"), meta);
        }
    }

    let review_started_at = iso();
    let mut rounds_history: Vec<Value> = Vec::new();
    let mut round: u64 = 1;
    let mut rework_rounds: u64 = 0;
    let mut prev_reviewed_tree: Option<String> = inherited_review
        .as_ref()
        .map(|inh| inh.reviewed_tree.clone());
    let mut last_valid_reviewed_tree: Option<String> = prev_reviewed_tree.clone();
    let mut prev_reviewed_base: Option<String> = inherited_review
        .as_ref()
        .map(|inh| inh.reviewed_base_tree.clone());
    let mut last_valid_verdict_file: Option<String> = inherited_review
        .as_ref()
        .and_then(|inh| inh.verdict_file.clone());
    let mut prev_verdict_text: Option<String> = inherited_review
        .as_ref()
        .map(|inh| inh.verdict_text.clone())
        .filter(|t| !t.trim().is_empty());
    let mut prev_review_continuation: Option<String> = inherited_review
        .as_ref()
        .and_then(|inh| inh.continuation.clone());
    let mut prev_continuation_is_agy: bool = inherited_review
        .as_ref()
        .is_some_and(|inh| inh.continuation_is_agy);
    let mut prev_review_workdir: Option<PathBuf> = inherited_review
        .as_ref()
        .and_then(|inh| inh.review_workdir.clone());
    let mut passed_layers: BTreeMap<String, Value> = BTreeMap::new();
    let mut rework_delta_files: Option<Vec<String>> = None;
    let mut active_review_cfg = review_cfg.clone();
    let mut active_fallback: Option<Value> = review_cfg
        .get("launchFallback")
        .filter(|v| v.is_object())
        .cloned();
    let extra_ro_workspace = ro_workspace_dir(&format!("{run_id}-extra"));
    let _extra_ro_cleanup = CleanupDir(extra_ro_workspace.clone());

    loop {
        let mut is_agy_reviewer = crate::agy::is_agy(s(&active_review_cfg, "model"));
        if lane::stopped() {
            *state = "stopped".into();
            sum["state"] = json!("stopped");
            let now = iso();
            let mut doc = json!({
                "status": "done",
                "verdict": Value::Null,
                "round": round,
                "reworkRounds": rework_rounds,
                "rubric": rubric_source,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": now,
                "finishedAt": now,
            });
            attach_review_model_fields(&mut doc, &active_review_cfg, active_fallback.as_ref());
            write_json(run.join("review.json"), &doc)?;
            sum["review"] = compact_review_summary(&doc);
            break;
        }

        // Phase L1: plan tiered acceptance and run fast layers BEFORE slow accept & reviewer.
        let plan = crate::accept::plan(
            meta,
            changed,
            rec.is_some(),
            rework_delta_files.as_deref(),
            &passed_layers,
        );
        let (excluded, accept_before_snap) = crate::accept::snapshot_for_accept(meta, run);
        let fast_outcome = if !plan.fast.is_empty() {
            Some(crate::accept::run_phase(
                meta,
                run,
                &plan.fast,
                false,
                holder.as_ref(),
                None,
                None,
            ))
        } else {
            None
        };
        if let Some(fo) = &fast_outcome {
            for (k, v) in &fo.passed_layers {
                let mut entry = v.clone();
                entry["passedRound"] = json!(round);
                passed_layers.insert(k.clone(), entry);
            }
            if !fo.ok {
                let (_, accept_after_snap) = crate::accept::snapshot_for_accept(meta, run);
                let mut accept_doc = crate::accept::build_accept_summary(
                    meta,
                    run,
                    &plan,
                    Some(fo),
                    None,
                    false,
                    excluded,
                    accept_before_snap.as_ref(),
                    accept_after_snap.as_ref(),
                );
                if b(&accept_doc, "infrastructureFailure") {
                    sum["infrastructureFailure"] = json!(true);
                    sum["error"] =
                        json!("remote infrastructure failure; retry does not spend a rework round");
                }
                apply_accept_failure_state(run, meta, &mut accept_doc, state, sum);
                emit_accept_event(run, meta, &accept_doc);
                sum["accept"] = accept_doc;
                if round > 1 {
                    let now = iso();
                    let mut final_doc = json!({
                        "status": "done",
                        "verdict": "REWORK",
                        "round": round - 1,
                        "reworkRounds": rework_rounds,
                        "rubric": rubric_source,
                        "needsAttention": rework_rounds >= NEEDS_ATTENTION_REWORK_ROUND,
                        "stoppedDuringRework": true,
                        "reworkOutcome": state,
                        "rounds": rounds_history,
                        "startedAt": review_started_at,
                        "updatedAt": now,
                        "finishedAt": now,
                    });
                    if let Some(tree) = &last_valid_reviewed_tree {
                        final_doc["reviewedTree"] = json!(tree);
                    }
                    if let Some(base) = &prev_reviewed_base {
                        final_doc["reviewedBaseTree"] = json!(base);
                    }
                    if let Some(vf) = &last_valid_verdict_file {
                        final_doc["verdictFile"] = json!(vf);
                    }
                    if let Some(cont) = &prev_review_continuation {
                        final_doc["reviewContinuation"] = json!(cont);
                        final_doc["reviewContinuationIsAgy"] = json!(prev_continuation_is_agy);
                    }
                    attach_review_model_fields(
                        &mut final_doc,
                        &active_review_cfg,
                        active_fallback.as_ref(),
                    );
                    write_json(run.join("review.json"), &final_doc)?;
                    sum["review"] = compact_review_summary(&final_doc);
                }
                break;
            }
        }

        // Fast layers passed (or none configured). Resolve trees & check pure sync-round skip.
        let changes_doc = json(run.join("changes.json"));
        let top = {
            let t = PathBuf::from(s(&changes_doc, "top"));
            if t.is_dir() {
                t
            } else {
                PathBuf::from(s(meta, "top"))
            }
        };
        let before_tree = s(&changes_doc, "base").to_string();
        let after_tree = s(&changes_doc, "after").to_string();
        let (current_total_base, _) = if top.is_dir() {
            worktree::effective_total_base(&top, meta)
        } else {
            (s(meta, "chainBase").to_string(), "chain-base")
        };

        // Pure sync round (Host deterministic check, zero LLM calls):
        // Run slow acceptance WITHOUT launching the reviewer; only pass straight through when
        // acceptance is ALL GREEN.
        let is_pure_sync = round == 1
            && inherited_review.as_ref().is_some_and(|inh| {
                evaluate_pure_sync_round(
                    &top,
                    run,
                    inh,
                    &before_tree,
                    &after_tree,
                    &current_total_base,
                    review_full,
                )
            });
        if is_pure_sync {
            let slow_outcome = if !plan.slow.is_empty() {
                Some(crate::accept::run_phase(
                    meta,
                    run,
                    &plan.slow,
                    fast_outcome.is_some(),
                    holder.as_ref(),
                    None,
                    None,
                ))
            } else {
                None
            };
            let (_, accept_after_snap) = crate::accept::snapshot_for_accept(meta, run);
            let mut accept_doc = crate::accept::build_accept_summary(
                meta,
                run,
                &plan,
                fast_outcome.as_ref(),
                slow_outcome.as_ref(),
                false,
                excluded,
                accept_before_snap.as_ref(),
                accept_after_snap.as_ref(),
            );
            let accept_ok = b(&accept_doc, "ok");
            if b(&accept_doc, "infrastructureFailure") {
                sum["infrastructureFailure"] = json!(true);
                sum["error"] =
                    json!("remote infrastructure failure; retry does not spend a rework round");
            }
            if !accept_ok {
                apply_accept_failure_state(run, meta, &mut accept_doc, state, sum);
                emit_accept_event(run, meta, &accept_doc);
                sum["accept"] = accept_doc;
                break;
            }
            emit_accept_event(run, meta, &accept_doc);
            sum["accept"] = accept_doc;

            let now = iso();
            let inh = inherited_review.as_ref().unwrap();
            let mut final_doc = json!({
                "status": "done",
                "verdict": "PASS",
                "scope": "sync_skip",
                "scopeFiles": 0,
                "syncSkip": true,
                "round": 0,
                "reworkRounds": 0,
                "rubric": rubric_source,
                "reviewedTree": after_tree,
                "reviewedBaseTree": current_total_base,
                "conclusion": "同步轮，沿用上一轮审查 PASS",
                "needsAttention": false,
                "rounds": [],
                "startedAt": review_started_at,
                "updatedAt": now,
                "finishedAt": now,
            });
            if let Some(vf) = &inh.verdict_file {
                final_doc["verdictFile"] = json!(vf);
            }
            if let Some(cont) = &inh.continuation {
                final_doc["reviewContinuation"] = json!(cont);
                final_doc["reviewContinuationIsAgy"] = json!(inh.continuation_is_agy);
            }
            if let Some(wd) = &inh.review_workdir {
                final_doc["reviewWorkdir"] = json!(wd.to_string_lossy());
            }
            attach_review_model_fields(&mut final_doc, &active_review_cfg, active_fallback.as_ref());
            write_json(run.join("review.json"), &final_doc)?;
            sum["review"] = compact_review_summary(&final_doc);
            events::emit(
                &run_id,
                &repo,
                "review_verdict",
                json!({
                    "name": run_name,
                    "round": 0,
                    "verdict": "PASS",
                    "scope": "sync_skip",
                    "syncSkip": true,
                    "reworkRounds": 0,
                }),
            );
            *state = "delivered".into();
            sum["state"] = json!("delivered");
            break;
        }

        // Prepare review round inputs.
        let round_dir = review_root.join(format!("round-{round}"));
        fs::create_dir_all(&round_dir).map_err(|e| e.to_string())?;
        let round_started = iso();
        let incremental = !review_full && prev_reviewed_tree.is_some();
        let review_scope = if incremental { "incremental" } else { "full" };

        let (diff_from, diff_label) = if incremental {
            (prev_reviewed_tree.clone().unwrap(), "incremental")
        } else {
            (current_total_base.clone(), "total")
        };
        let exclude_mainline = if incremental {
            prev_reviewed_base.as_deref().map(|old_base| {
                pure_mainline_advance_paths(
                    &top,
                    old_base,
                    &diff_from,
                    &current_total_base,
                    &before_tree,
                    &after_tree,
                )
            })
        } else {
            None
        };
        let (diff_text, scope_files) =
            generate_reviewer_diff(&top, &diff_from, &after_tree, exclude_mainline.as_ref());
        let diff_file = round_dir.join(format!("{diff_label}.diff"));
        write(&diff_file, &diff_text)?;

        let mut live_review = json!({
            "status": "reviewing",
            "verdict": if rework_rounds > 0 { json!("REWORK") } else { Value::Null },
            "scope": review_scope,
            "scopeFiles": scope_files,
            "round": round,
            "reworkRounds": rework_rounds,
            "rubric": rubric_source,
            "needsAttention": rework_rounds >= NEEDS_ATTENTION_REWORK_ROUND,
            "rounds": rounds_history,
            "startedAt": review_started_at,
            "updatedAt": round_started,
        });
        attach_review_model_fields(&mut live_review, &active_review_cfg, active_fallback.as_ref());
        write_json(run.join("review.json"), &live_review)?;

        events::emit(
            &run_id,
            &repo,
            "review_started",
            json!({
                "name": run_name,
                "round": round,
                "reworkRounds": rework_rounds,
                "incremental": incremental,
                "scope": review_scope,
                "scopeFiles": scope_files,
                "model": active_review_cfg["model"],
                "modelName": active_review_cfg["modelName"],
                "tier": active_review_cfg["tier"],
                "parallelWithSlowAccept": !plan.slow.is_empty(),
            }),
        );

        // Prepare isolated read-only workspace whenever running agy OR running in parallel with slow accept.
        // Do this BEFORE building the prompt and deciding session continuation so we know the exact
        // workdir for this round and fail closed as `ENV_ERROR` (not `NEEDS_CONTROLLER`) if missing.
        let parallel_with_slow = !plan.slow.is_empty();
        let excludes = strings(&meta["snapshotExclude"]);
        let use_ro_workspace = (is_agy_reviewer || parallel_with_slow)
            && top.is_dir()
            && !after_tree.is_empty();
        let ro_prep_result =
            if use_ro_workspace {
                match prepare_ro_workspace(&top, &after_tree, &ro_workspace, &round_dir) {
                    Ok(()) => {
                        if !ro_workspace.is_dir() {
                            Err(format!(
                                "只读审查快照缺失（{}）",
                                ro_workspace.display()
                            ))
                        } else {
                            match dir_manifest_for_repo(&ro_workspace, &top) {
                                Ok(manifest) => Ok((ro_workspace.clone(), None, Some(manifest))),
                                Err(err) => Err(format!(
                                    "读取只读审查快照清单失败（{}）：{err}",
                                    ro_workspace.display()
                                )),
                            }
                        }
                    }
                    Err(err) => Err(format!(
                        "只读审查快照缺失或准备失败（{}）：{err}",
                        ro_workspace.display()
                    )),
                }
            } else {
                let wd = PathBuf::from(s(meta, "workdir"));
                if wd.as_os_str().is_empty() || !wd.is_dir() {
                    Err(format!("审查工作目录缺失（{}）", wd.display()))
                } else {
                    let snap = if top.is_dir() {
                        changes::snapshot(&top, &round_dir, &excludes)
                    } else {
                        None
                    };
                    Ok((wd, snap, None))
                }
            };

        let (reviewer_workdir, before_pi_snap, before_ro_manifest) = match ro_prep_result {
            Ok(triple) => triple,
            Err(env_err) => {
                // Still run slow acceptance if configured so acceptance status is accurate.
                let slow_outcome = if parallel_with_slow {
                    Some(crate::accept::run_phase(
                        meta,
                        run,
                        &plan.slow,
                        fast_outcome.is_some(),
                        holder.as_ref(),
                        None,
                        None,
                    ))
                } else {
                    None
                };
                let (_, accept_after_snap) = crate::accept::snapshot_for_accept(meta, run);
                let mut accept_doc = crate::accept::build_accept_summary(
                    meta,
                    run,
                    &plan,
                    fast_outcome.as_ref(),
                    slow_outcome.as_ref(),
                    false,
                    excluded,
                    accept_before_snap.as_ref(),
                    accept_after_snap.as_ref(),
                );
                let accept_ok = b(&accept_doc, "ok");
                if b(&accept_doc, "infrastructureFailure") {
                    sum["infrastructureFailure"] = json!(true);
                    sum["error"] =
                        json!("remote infrastructure failure; retry does not spend a rework round");
                }
                if !accept_ok {
                    apply_accept_failure_state(run, meta, &mut accept_doc, state, sum);
                }
                emit_accept_event(run, meta, &accept_doc);
                sum["accept"] = accept_doc;

                let now = iso();
                let err_short = clip(&env_err, MAX_PARSE_ERROR_CHARS);
                let retry_cmd = review_env_error_retry_command(&run_id);
                let round_entry = json!({
                    "round": round,
                    "verdict": REVIEW_VERDICT_ENV_ERROR,
                    "scope": review_scope,
                    "scopeFiles": scope_files,
                    "diffFile": diff_file.to_string_lossy(),
                    "incremental": incremental,
                    "envError": true,
                    "error": err_short,
                    "retryCommand": retry_cmd,
                    "startedAt": round_started,
                    "finishedAt": now,
                });
                rounds_history.push(round_entry);
                let mut final_doc = json!({
                    "status": "env_error",
                    "verdict": REVIEW_VERDICT_ENV_ERROR,
                    "envError": true,
                    "error": err_short,
                    "parseError": err_short,
                    "retryCommand": retry_cmd,
                    "scope": review_scope,
                    "scopeFiles": scope_files,
                    "round": round,
                    "reworkRounds": rework_rounds,
                    "rubric": rubric_source,
                    "needsAttention": true,
                    "rounds": rounds_history,
                    "startedAt": review_started_at,
                    "updatedAt": now,
                    "finishedAt": now,
                });
                if let Some(tree) = &last_valid_reviewed_tree {
                    final_doc["reviewedTree"] = json!(tree);
                }
                if let Some(base) = &prev_reviewed_base {
                    final_doc["reviewedBaseTree"] = json!(base);
                }
                attach_review_model_fields(
                    &mut final_doc,
                    &active_review_cfg,
                    active_fallback.as_ref(),
                );
                crate::usage::attach_review_totals(&rounds_history, &mut final_doc);
                write_json(run.join("review.json"), &final_doc)?;
                sum["review"] = compact_review_summary(&final_doc);
                events::emit(
                    &run_id,
                    &repo,
                    "review_verdict",
                    json!({
                        "name": run_name,
                        "round": round,
                        "verdict": REVIEW_VERDICT_ENV_ERROR,
                        "envError": true,
                        "error": err_short,
                        "retryCommand": retry_cmd,
                    }),
                );
                events::emit(
                    &run_id,
                    &repo,
                    "needs_attention",
                    json!({
                        "name": run_name,
                        "round": round,
                        "reworkRounds": rework_rounds,
                        "reason": "reviewEnvError",
                        "verdict": REVIEW_VERDICT_ENV_ERROR,
                        "detail": err_short,
                    }),
                );
                *state = if accept_ok { "delivered" } else { "rejected" }.into();
                sum["state"] = json!(*state);
                break;
            }
        };

        let can_continue = incremental
            && can_continue_review_session(
                is_agy_reviewer,
                prev_continuation_is_agy,
                prev_review_workdir.as_deref(),
                &reviewer_workdir,
            );
        let fork_target = if can_continue {
            match prev_review_continuation.as_deref() {
                Some(prev) => copy_pi_fork_if_needed(prev, &round_dir, is_agy_reviewer).ok(),
                None => None,
            }
        } else {
            None
        };
        let workdir_note = format_review_workdir_note(
            use_ro_workspace,
            &reviewer_workdir,
            prev_review_workdir.as_deref(),
            fork_target.is_some(),
        );
        let diff_section = format_diff_for_prompt(&diff_text, &diff_file, &top, &diff_from, &after_tree);
        let prev_verdict_section = prev_verdict_text
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(|t| format!("## 上一轮审查结论原文\n\n{}\n\n", t.trim()))
            .unwrap_or_default();
        let reply_turn_section = if round == 1
            && inherited_review.is_some()
            && !task_brief.trim().is_empty()
        {
            format!("## 任务说明\n\n{}\n\n", task_brief.trim())
        } else {
            String::new()
        };
        let incremental_rule = "上一轮已 PASS 的部分不重审，只核对本轮改动是否引入问题、是否修好上一轮指出的问题。";
        let output_guide = "开头先给一句明确结论（通过 / 需返工 / 需主控决定，或 PASS / REWORK / NEEDS_CONTROLLER），下面自然写核查依据与重点关注位置。";
        let prompt_body = if fork_target.is_some() {
            let lead = if round > 1 {
                format!("作者已完成第 {} 轮返工。下面是仅限本轮相对上一轮已审快照的增量 diff，请基于此前审查结论续审：", round - 1)
            } else {
                "本轮为上一轮审查之后的续做（reply）。下面是仅限本轮相对上一轮已审快照的增量 diff，请基于此前审查结论续审：".to_string()
            };
            format!(
                "{lead}\n\
                 {workdir_note}\
                 - {REVIEW_ACCEPT_OWNED_NOTE}\n\
                 - {incremental_rule}\n\
                 - {output_guide}\n\n\
                 {reply_turn_section}{prev_verdict_section}## 本轮增量 diff（第 {round} 轮审查）\n\n\
                 {diff_section}\n"
            )
        } else if incremental {
            format!(
                "请作为独立只读审查员进行续审（已提供上下文、上一轮审查结论原文与本轮增量 diff）。\n\
                 {workdir_note}\
                 {REVIEW_ACCEPT_OWNED_NOTE}\n\
                 {incremental_rule}\n\
                 {output_guide}\n\n\
                 ## 审查要点（{rubric_source}）\n\n\
                 {}\n\n\
                 ## 任务说明\n\n\
                 {}\n\n\
                 {prev_verdict_section}## 本轮增量 diff\n\n\
                 {diff_section}\n",
                rubric_text.trim(),
                task_brief.trim(),
            )
        } else {
            build_full_review_prompt(
                use_ro_workspace,
                &reviewer_workdir,
                &diff_file,
                &diff_text,
                &top,
                &diff_from,
                &after_tree,
                &rubric_source,
                &rubric_text,
                &task_brief,
            )
        };
        let review_prompt_file = round_dir.join("prompt.md");
        write(&review_prompt_file, &prompt_body)?;

        let mut review_meta = build_reviewer_meta(
            meta,
            &active_review_cfg,
            &round_dir,
            &reviewer_workdir,
            &review_prompt_file,
            fork_target.as_deref(),
        );
        if fork_target.is_none() {
            review_meta["freshAgySession"] = json!(true);
        }
        crate::deny::prepare(&review_meta, &round_dir)?;

        // Evaluate optional supplementary reviewer (`review.extra`) triggers + quota cache.
        let extra_pre = {
            let extra_cfg = &active_review_cfg["extra"];
            if extra_cfg.is_object() && b(extra_cfg, "enabled") {
                let total_diff_for_extra = if diff_label == "total" {
                    diff_text.clone()
                } else {
                    let (total_base, _) = worktree::effective_total_base(&top, meta);
                    if !top.is_dir() || total_base.is_empty() || after_tree.is_empty() {
                        String::new()
                    } else {
                        git(
                            &top,
                            &[
                                "diff",
                                "--no-ext-diff",
                                "--no-textconv",
                                "--no-renames",
                                "--color=never",
                                &total_base,
                                &after_tree,
                            ],
                        )
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default()
                    }
                };
                let triggers = evaluate_extra_triggers(extra_cfg, run, meta, &total_diff_for_extra);
                if triggers.is_empty() {
                    ExtraPreDecision::None
                } else {
                    let em_model = s(extra_cfg, "model").to_string();
                    let em_name = {
                        let mn = s(extra_cfg, "modelName");
                        if !mn.is_empty() { mn.to_string() } else { em_model.clone() }
                    };
                    let res_err = s(extra_cfg, "resolveError");
                    if !res_err.is_empty() || em_model.is_empty() {
                        ExtraPreDecision::Error {
                            model: em_model,
                            model_name: em_name,
                            triggers,
                            error: if res_err.is_empty() {
                                "补充审查模型未解析".to_string()
                            } else {
                                res_err.to_string()
                            },
                        }
                    } else {
                        let quota_cache = crate::agy_accounts::load_quota_cache();
                        let pool_acct = crate::agy_accounts::current_account_for_model(&em_model);
                        match check_extra_quota(extra_cfg, &quota_cache, pool_acct.as_deref()) {
                            ExtraQuotaDecision::Skip {
                                five_hour_pct,
                                weekly_pct,
                                reason,
                            } => ExtraPreDecision::Skipped {
                                model: em_model,
                                model_name: em_name,
                                triggers,
                                five_hour_pct,
                                weekly_pct,
                                reason,
                            },
                            ExtraQuotaDecision::Allow => {
                                let extra_dir = round_dir.join("extra");
                                fs::create_dir_all(&extra_dir).map_err(|e| e.to_string())?;
                                let (total_base, _) = worktree::effective_total_base(&top, meta);
                                let (extra_rev_diff, _) = if diff_label == "total" {
                                    (diff_text.clone(), scope_files)
                                } else {
                                    generate_reviewer_diff(&top, &total_base, &after_tree, None)
                                };
                                let extra_diff_file = extra_dir.join("total.diff");
                                write(&extra_diff_file, &extra_rev_diff)?;
                                let extra_diff_sec = format_diff_for_prompt(
                                    &extra_rev_diff,
                                    &extra_diff_file,
                                    &top,
                                    &total_base,
                                    &after_tree,
                                );
                                let extra_prompt_body = format!(
                                    "请作为独立只读补充审查员审查以下写入任务的交付结果（提供第二意见）。\n\
                                     {REVIEW_ACCEPT_OWNED_NOTE}\n\
                                     严格遵守：不修改任何文件，不委派子任务；{output_guide}\n\n\
                                     ## 审查要点（{rubric_source}）\n\n\
                                     {}\n\n\
                                     ## 任务说明\n\n\
                                     {}\n\n\
                                     ## 本链净 diff（diff --total）\n\n\
                                     {extra_diff_sec}\n",
                                    rubric_text.trim(),
                                    task_brief.trim(),
                                );
                                let extra_prompt_file = extra_dir.join("prompt.md");
                                write(&extra_prompt_file, &extra_prompt_body)?;
                                let extra_workdir_res: Result<PathBuf, String> =
                                    if top.is_dir() && !after_tree.is_empty() {
                                        prepare_ro_workspace(
                                            &top,
                                            &after_tree,
                                            &extra_ro_workspace,
                                            &extra_dir,
                                        )
                                        .and_then(|()| {
                                            if extra_ro_workspace.is_dir() {
                                                Ok(extra_ro_workspace.clone())
                                            } else {
                                                Err(format!(
                                                    "补充审查只读快照缺失（{}）",
                                                    extra_ro_workspace.display()
                                                ))
                                            }
                                        })
                                    } else {
                                        Ok(PathBuf::from(s(meta, "workdir")))
                                    };
                                match extra_workdir_res {
                                    Ok(extra_workdir) => {
                                        let mut extra_meta = build_reviewer_meta(
                                            meta,
                                            extra_cfg,
                                            &extra_dir,
                                            &extra_workdir,
                                            &extra_prompt_file,
                                            None,
                                        );
                                        extra_meta["isExtraReviewer"] = json!(true);
                                        crate::deny::prepare(&extra_meta, &extra_dir)?;
                                        ExtraPreDecision::Run {
                                            model: em_model,
                                            model_name: em_name,
                                            triggers,
                                            extra_dir,
                                            extra_meta,
                                            verdict_file: review_root
                                                .join(format!("round-{round}-extra-verdict.md")),
                                        }
                                    }
                                    Err(err) => ExtraPreDecision::Error {
                                        model: em_model,
                                        model_name: em_name,
                                        triggers,
                                        error: clip(&err, MAX_PARSE_ERROR_CHARS),
                                    },
                                }
                            }
                        }
                    }
                }
            } else {
                ExtraPreDecision::None
            }
        };

        // Phase L2: run slow acceptance, main read-only reviewer, and optional extra reviewer in parallel.
        let slow_holder = Arc::new(AtomicI32::new(0));
        let rev_holder = Arc::new(AtomicI32::new(0));
        let extra_holder = Arc::new(AtomicI32::new(0));
        let slow_cancel = Arc::new(AtomicBool::new(false));
        let rev_cancel = Arc::new(AtomicBool::new(false));
        supervise::register_stop_holder(slow_holder.clone());
        supervise::register_stop_holder(rev_holder.clone());
        supervise::register_stop_holder(extra_holder.clone());

        let extra_running = matches!(extra_pre, ExtraPreDecision::Run { .. });
        let append_slow_log = fast_outcome.is_some();
        let (slow_outcome, rev_outcome, extra_outcome) = std::thread::scope(|scope| {
            let slow_handle = if parallel_with_slow {
                let slow_holder_t = slow_holder.clone();
                let slow_cancel_t = slow_cancel.clone();
                let rev_cancel_t = rev_cancel.clone();
                let rev_holder_t = rev_holder.clone();
                let extra_holder_t = extra_holder.clone();
                let meta_ref: &Value = meta;
                let plan_slow = &plan.slow;
                let round_dir_ref: &Path = &round_dir;
                Some(scope.spawn(move || {
                    let res = crate::accept::run_phase(
                        meta_ref,
                        run,
                        plan_slow,
                        append_slow_log,
                        slow_holder_t.as_ref(),
                        Some(slow_cancel_t.as_ref()),
                        Some("审查员判定需要返工/失败，取消慢层验收"),
                    );
                    if !res.ok && !res.cancelled && !lane::stopped() {
                        // Slow acceptance failed -> cancel reviewers immediately (never PASS).
                        rev_cancel_t.store(true, Ordering::SeqCst);
                        let rpid = rev_holder_t.load(Ordering::SeqCst);
                        if rpid > 0 {
                            crate::cleanup::record(round_dir_ref, rpid);
                            kill_group(rpid, 1.0);
                        }
                        let epid = extra_holder_t.load(Ordering::SeqCst);
                        if epid > 0 {
                            crate::cleanup::record(round_dir_ref, epid);
                            kill_group(epid, 1.0);
                        }
                    }
                    res
                }))
            } else {
                None
            };

            let extra_handle = match &extra_pre {
                ExtraPreDecision::Run {
                    extra_dir,
                    extra_meta,
                    ..
                } => {
                    let eh = extra_holder.clone();
                    let eg = grace.clone();
                    let ec = rev_cancel.clone();
                    let ed = extra_dir.clone();
                    let em = extra_meta.clone();
                    Some(scope.spawn(move || {
                        let attempt = run_reviewer_with_loop_stop_retry(&em, &ed, eh, eg, ec.clone());
                        if ec.load(Ordering::SeqCst) {
                            return ExtraRoundOutcome {
                                cancelled: true,
                                verdict: None,
                                raw_answer: String::new(),
                                conclusion: None,
                                error: None,
                            };
                        }
                        match attempt.res {
                            Ok((outcome, text)) if outcome == "ok" => {
                                let v = match parse_verdict(&text) {
                                    ParsedVerdict::Pass => Some("PASS".to_string()),
                                    ParsedVerdict::Rework => Some("REWORK".to_string()),
                                    ParsedVerdict::NeedsController => {
                                        Some("NEEDS_CONTROLLER".to_string())
                                    }
                                    ParsedVerdict::Invalid(_) => {
                                        return ExtraRoundOutcome {
                                            cancelled: false,
                                            verdict: None,
                                            raw_answer: text,
                                            conclusion: None,
                                            error: Some(VERDICT_MISSING_NOTE.to_string()),
                                        };
                                    }
                                };
                                let concl = crate::card::extract_reviewer_conclusion(&text);
                                ExtraRoundOutcome {
                                    cancelled: false,
                                    verdict: v,
                                    raw_answer: text,
                                    conclusion: concl,
                                    error: None,
                                }
                            }
                            Ok((outcome, _)) if outcome == "cancelled" => ExtraRoundOutcome {
                                cancelled: true,
                                verdict: None,
                                raw_answer: String::new(),
                                conclusion: None,
                                error: None,
                            },
                            Ok((outcome, _)) => ExtraRoundOutcome {
                                cancelled: false,
                                verdict: None,
                                raw_answer: String::new(),
                                conclusion: None,
                                error: Some(format!("补充审查结束状态：{outcome}")),
                            },
                            Err(err) => ExtraRoundOutcome {
                                cancelled: false,
                                verdict: None,
                                raw_answer: String::new(),
                                conclusion: None,
                                error: Some(clip(&err, MAX_PARSE_ERROR_CHARS)),
                            },
                        }
                    }))
                }
                _ => None,
            };

            // Run primary reviewer; if configured review model is unavailable (startup failure,
            // upstream refusal, quota exhaustion), fall back to `review` tier and record reason.
            let mut active_exec_dir = round_dir.clone();
            let mut active_exec_meta = review_meta.clone();
            let mut attempt = run_reviewer_with_loop_stop_retry(
                &active_exec_meta,
                &active_exec_dir,
                rev_holder.clone(),
                grace.clone(),
                rev_cancel.clone(),
            );
            // Reviewers NEVER use writer handoff; if the primary agy reviewer hits hard quota
            // exhaustion and another non-Red account is available, retry ONCE on the other account
            // with the FULL review template (`fork_target = None`) before considering `review.fallback`.
            if is_agy_reviewer
                && !rev_cancel.load(Ordering::SeqCst)
                && !lane::stopped()
                && crate::agy_accounts::is_auto_mode_enabled()
                && reviewer_quota_exhausted_error(&attempt.res, &active_exec_dir).is_some()
            {
                let rev_model = s(&active_review_cfg, "model").to_string();
                let from_acct = {
                    let owner_disk = json(run.join("meta.json"));
                    s(&owner_disk, "reviewAgyAccount").to_string()
                };
                if !from_acct.is_empty() {
                    let _ = crate::agy_accounts::mark_slot_quota_exhausted_passive(
                        &from_acct,
                        &rev_model,
                        epoch() as i64,
                    );
                    if let Some(to_acct) = crate::agy_accounts::select_handoff_target(
                        &from_acct,
                        &rev_model,
                        None,
                        true,
                    ) {
                        if let Some(pool) = crate::agy_accounts::classify_model_pool(&rev_model) {
                            let _ = crate::agy_accounts::set_pool_current_in(
                                &state_dir(),
                                pool,
                                &to_acct,
                            );
                        }
                        let retry_dir = round_dir.join("acct-retry");
                        if fs::create_dir_all(&retry_dir).is_ok() {
                            let retry_workdir_note = format_review_workdir_note(
                                use_ro_workspace,
                                &reviewer_workdir,
                                prev_review_workdir.as_deref(),
                                false,
                            );
                            let retry_prompt_file = if fork_target.is_some() {
                                let full_body = if incremental {
                                    format!(
                                        "请作为独立只读审查员进行续审（已提供上下文、上一轮审查结论原文与本轮增量 diff）。\n\
                                         {retry_workdir_note}\
                                         {REVIEW_ACCEPT_OWNED_NOTE}\n\
                                         {incremental_rule}\n\
                                         {output_guide}\n\n\
                                         ## 审查要点（{rubric_source}）\n\n\
                                         {}\n\n\
                                         ## 任务说明\n\n\
                                         {}\n\n\
                                         {prev_verdict_section}## 本轮增量 diff\n\n\
                                         {diff_section}\n",
                                        rubric_text.trim(),
                                        task_brief.trim(),
                                    )
                                } else {
                                    format!(
                                        "请作为独立只读审查员审查以下写入任务的交付结果。\n\
                                         {retry_workdir_note}\
                                         {REVIEW_ACCEPT_OWNED_NOTE}\n\
                                         严格遵守：不修改任何文件，不委派子任务；{output_guide}\n\n\
                                         ## 审查要点（{rubric_source}）\n\n\
                                         {}\n\n\
                                         ## 任务说明\n\n\
                                         {}\n\n\
                                         ## 本链净 diff（diff --total）\n\n\
                                         {diff_section}\n",
                                        rubric_text.trim(),
                                        task_brief.trim(),
                                    )
                                };
                                let p = retry_dir.join("prompt.md");
                                let _ = write(&p, &full_body);
                                p
                            } else {
                                review_prompt_file.clone()
                            };
                            let reason = "审查中额度耗尽，完整模板换号重审".to_string();
                            let at = iso();
                            let pool_key = crate::agy_accounts::classify_model_pool(&rev_model)
                                .map(|p| p.key().to_string());
                            let mut sw_record = json!({
                                "kind": "handoff",
                                "from": from_acct,
                                "to": to_acct,
                                "reason": reason,
                                "at": at,
                            });
                            if let Some(ref pk) = pool_key {
                                sw_record["pool"] = json!(pk);
                            }
                            let mut owner_disk = json(run.join("meta.json"));
                            if owner_disk.is_object() {
                                owner_disk["reviewAgyAccountSwitch"] = sw_record;
                                let _ = write_json(run.join("meta.json"), &owner_disk);
                            }
                            let mut ev_payload = json!({
                                "role": "reviewer",
                                "mode": "review",
                                "from": from_acct,
                                "to": to_acct,
                                "model": rev_model,
                                "reason": reason,
                            });
                            if let Some(ref pk) = pool_key {
                                ev_payload["pool"] = json!(pk);
                            }
                            let mut run_ev = ev_payload.clone();
                            run_ev["e"] = json!("agy_account_handoff");
                            run_ev["run"] = json!(&run_id);
                            supervise::log_event(run, run_ev);
                            events::emit(&run_id, &repo, "agy_account_handoff", ev_payload);

                            let mut retry_meta = build_reviewer_meta(
                                meta,
                                &active_review_cfg,
                                &retry_dir,
                                &reviewer_workdir,
                                &retry_prompt_file,
                                None,
                            );
                            retry_meta["freshAgySession"] = json!(true);
                            retry_meta["preferredAgyAccount"] = json!(&to_acct);
                            retry_meta["isAgyHandoff"] = json!(true);
                            let _ = crate::deny::prepare(&retry_meta, &retry_dir);
                            active_exec_dir = retry_dir;
                            active_exec_meta = retry_meta;
                            attempt = run_reviewer_with_loop_stop_retry(
                                &active_exec_meta,
                                &active_exec_dir,
                                rev_holder.clone(),
                                grace.clone(),
                                rev_cancel.clone(),
                            );
                            let retry_usage = active_exec_dir.join("usage.json");
                            if retry_usage.is_file() {
                                let _ = fs::copy(&retry_usage, round_dir.join("usage.json"));
                            }
                        }
                    }
                }
            }
            if !rev_cancel.load(Ordering::SeqCst)
                && !lane::stopped()
                && active_fallback.is_none()
                && active_review_cfg["fallback"].is_object()
                && !s(&active_review_cfg["fallback"], "model").is_empty()
                && s(&active_review_cfg["fallback"], "model") != s(&active_review_cfg, "model")
            {
                if let Some(reason) = classify_unavailable_reason(&attempt.res, &active_exec_dir) {
                    let fb_cfg = active_review_cfg["fallback"].clone();
                    let from_alias = {
                        let n = s(&active_review_cfg, "modelName");
                        if !n.is_empty() { n } else { s(&active_review_cfg, "model") }
                    }
                    .to_string();
                    let from_model = s(&active_review_cfg, "model").to_string();
                    let to_alias = {
                        let n = s(&fb_cfg, "modelName");
                        if !n.is_empty() { n } else { s(&fb_cfg, "model") }
                    }
                    .to_string();
                    let to_model = s(&fb_cfg, "model").to_string();
                    active_fallback = Some(json!({
                        "from": from_alias,
                        "fromModel": from_model,
                        "to": to_alias,
                        "toModel": to_model,
                        "reason": reason,
                    }));
                    active_review_cfg = apply_fallback_cfg(&active_review_cfg, &fb_cfg);
                    is_agy_reviewer = crate::agy::is_agy(s(&active_review_cfg, "model"));
                    let fallback_dir = round_dir.join("fallback");
                    if fs::create_dir_all(&fallback_dir).is_ok() {
                        let fb_workdir = if use_ro_workspace {
                            ro_workspace.clone()
                        } else if is_agy_reviewer && top.is_dir() && !after_tree.is_empty() {
                            let _ = prepare_ro_workspace(&top, &after_tree, &ro_workspace, &fallback_dir);
                            ro_workspace.clone()
                        } else {
                            reviewer_workdir.clone()
                        };
                        let fb_workdir_note = format_review_workdir_note(
                            use_ro_workspace || (is_agy_reviewer && top.is_dir() && !after_tree.is_empty()),
                            &fb_workdir,
                            prev_review_workdir.as_deref(),
                            false,
                        );
                        let fb_prompt_file = if fork_target.is_some() {
                            let fb_body = format!(
                                "请作为独立只读审查员进行第 {round} 轮续审（已回退到备用审查模型，提供完整上下文、上一轮审查结论原文与本轮增量 diff）。\n\
                                 {fb_workdir_note}\
                                 {REVIEW_ACCEPT_OWNED_NOTE}\n\
                                 {incremental_rule}\n\
                                 {output_guide}\n\n\
                                 ## 审查要点（{rubric_source}）\n\n\
                                 {}\n\n\
                                 ## 任务说明\n\n\
                                 {}\n\n\
                                 {prev_verdict_section}## 本轮增量 diff\n\n\
                                 {diff_section}\n",
                                rubric_text.trim(),
                                task_brief.trim(),
                            );
                            let p = fallback_dir.join("prompt.md");
                            let _ = write(&p, &fb_body);
                            p
                        } else {
                            review_prompt_file.clone()
                        };
                        let mut fb_meta = build_reviewer_meta(
                            meta,
                            &active_review_cfg,
                            &fallback_dir,
                            &fb_workdir,
                            &fb_prompt_file,
                            None,
                        );
                        fb_meta["freshAgySession"] = json!(true);
                        let _ = crate::deny::prepare(&fb_meta, &fallback_dir);
                        active_exec_dir = fallback_dir;
                        active_exec_meta = fb_meta;
                        attempt = run_reviewer_with_loop_stop_retry(
                            &active_exec_meta,
                            &active_exec_dir,
                            rev_holder.clone(),
                            grace.clone(),
                            rev_cancel.clone(),
                        );
                        let fb_usage = active_exec_dir.join("usage.json");
                        if fb_usage.is_file() {
                            let _ = fs::copy(&fb_usage, round_dir.join("usage.json"));
                        }
                    }
                }
            }

            let cancelled_by_accept = rev_cancel.load(Ordering::SeqCst);
            let round_finished = iso();
            let verdict_file = review_root.join(format!("round-{round}-verdict.md"));

            let rev_out = if cancelled_by_accept {
                ReviewRoundOutcome {
                    cancelled: true,
                    stopped: false,
                    verdict_str: String::new(),
                    raw_answer: String::new(),
                    raw_first_line: String::new(),
                    format_error: false,
                    format_retried: attempt.format_retried,
                    format_retry_failed: false,
                    env_error: None,
                    parse_error: None,
                    upgraded_by: None,
                    ro_violations: Vec::new(),
                    ro_violation_sample: Vec::new(),
                    ro_violation_file: None,
                    verdict_file,
                    diff_file: diff_file.clone(),
                    usage_dir: active_exec_dir.clone(),
                    round_started: round_started.clone(),
                    round_finished,
                    next_continuation: None,
                    conclusion: None,
                    focus_count: 0,
                }
            } else {
                let raw_ro_violations: Vec<String> = if let Some(before_m) = before_ro_manifest {
                    let after_m = dir_manifest_for_repo(&ro_workspace, &top).unwrap_or_default();
                    diff_manifests(&before_m, &after_m)
                } else if let Some(before_s) = before_pi_snap {
                    if let Some(after_s) = changes::snapshot(&top, &active_exec_dir, &excludes) {
                        if s(&before_s, "tree") != s(&after_s, "tree") {
                            changes::tree_changes(
                                &top,
                                &before_s,
                                &after_s,
                                meta["worktree"].is_object(),
                            )
                            .unwrap_or_default()
                            .into_iter()
                            .map(|c| s(&c, "path").to_string())
                            .collect()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };
                let ro_violations = filter_ro_violations(&top, raw_ro_violations);
                let next_continuation =
                    find_session_continuation(&active_exec_dir, &active_exec_meta);

                let mut format_error = false;
                let mut format_retry_failed = attempt.format_retry_failed;
                let mut env_error: Option<String> = None;
                let mut parse_error: Option<String> = None;
                let mut upgraded_by: Option<String> = None;
                let mut raw_first_line = String::new();
                let mut stopped_flag = false;

                let (mut verdict_str, raw_answer) = match attempt.res {
                    Ok((agent_outcome, text)) if agent_outcome == "ok" => {
                        match parse_verdict(&text) {
                            ParsedVerdict::Pass => {
                                raw_first_line = "PASS".into();
                                format_retry_failed = false;
                                ("PASS".to_string(), text)
                            }
                            ParsedVerdict::Rework => {
                                raw_first_line = "REWORK".into();
                                format_retry_failed = false;
                                ("REWORK".to_string(), text)
                            }
                            ParsedVerdict::NeedsController => {
                                raw_first_line = "NEEDS_CONTROLLER".into();
                                format_retry_failed = false;
                                ("NEEDS_CONTROLLER".to_string(), text)
                            }
                            ParsedVerdict::Invalid(first) => {
                                raw_first_line = clip(&first, MAX_PARSE_ERROR_CHARS);
                                format_error = true;
                                if attempt.format_retried {
                                    format_retry_failed = true;
                                    parse_error = Some(clip(
                                        &format!(
                                            "补问 1 次仍无结论：首行 {:?}（期望通过/返工/需主控决定）",
                                            clip(&first, 120)
                                        ),
                                        MAX_PARSE_ERROR_CHARS,
                                    ));
                                } else {
                                    parse_error = Some(VERDICT_MISSING_NOTE.to_string());
                                }
                                upgraded_by = Some("formatError".into());
                                ("NEEDS_CONTROLLER".to_string(), text)
                            }
                        }
                    }
                    Ok((agent_outcome, _)) if agent_outcome == "cancelled" => {
                        let extra_res = extra_handle.and_then(|h| h.join().ok());
                        let slow_res = slow_handle.map(|h| {
                            h.join()
                                .unwrap_or_else(|_| crate::accept::PhaseOutcome::empty_ok())
                        });
                        return (
                            slow_res,
                            ReviewRoundOutcome {
                                cancelled: true,
                                stopped: false,
                                verdict_str: String::new(),
                                raw_answer: String::new(),
                                raw_first_line: String::new(),
                                format_error: false,
                                format_retried: attempt.format_retried,
                                format_retry_failed: false,
                                env_error: None,
                                parse_error: None,
                                upgraded_by: None,
                                ro_violations: Vec::new(),
                                ro_violation_sample: Vec::new(),
                                ro_violation_file: None,
                                verdict_file,
                                diff_file: diff_file.clone(),
                                usage_dir: active_exec_dir,
                                round_started: round_started.clone(),
                                round_finished,
                                next_continuation: None,
                                conclusion: None,
                                focus_count: 0,
                            },
                            extra_res,
                        );
                    }
                    Ok((agent_outcome, text)) => {
                        if lane::stopped() {
                            stopped_flag = true;
                            (String::new(), text)
                        } else if attempt.format_retried {
                            format_error = true;
                            format_retry_failed = true;
                            parse_error = Some(clip(
                                &format!("补问 1 次仍无结论（outcome: {agent_outcome}）"),
                                MAX_PARSE_ERROR_CHARS,
                            ));
                            upgraded_by = Some("formatError".into());
                            ("NEEDS_CONTROLLER".to_string(), text)
                        } else {
                            parse_error = Some(clip(
                                &format!("reviewer ended with outcome: {agent_outcome}"),
                                MAX_PARSE_ERROR_CHARS,
                            ));
                            upgraded_by = Some("reviewerError".into());
                            ("NEEDS_CONTROLLER".to_string(), text)
                        }
                    }
                    Err(err) => {
                        if let Some(reason) = err.strip_prefix(REVIEW_ENV_ERROR_PREFIX) {
                            let msg = clip(reason.trim(), MAX_PARSE_ERROR_CHARS);
                            env_error = Some(msg.clone());
                            parse_error = Some(msg);
                            (REVIEW_VERDICT_ENV_ERROR.to_string(), String::new())
                        } else if attempt.format_retried {
                            format_error = true;
                            format_retry_failed = true;
                            parse_error = Some(clip(
                                &format!("补问 1 次仍无结论（{err}）"),
                                MAX_PARSE_ERROR_CHARS,
                            ));
                            upgraded_by = Some("formatError".into());
                            ("NEEDS_CONTROLLER".to_string(), String::new())
                        } else {
                            parse_error = Some(clip(
                                &format!("reviewer execution error: {err}"),
                                MAX_PARSE_ERROR_CHARS,
                            ));
                            upgraded_by = Some("reviewerError".into());
                            ("NEEDS_CONTROLLER".to_string(), String::new())
                        }
                    }
                };

                // If the read-only snapshot vanished during review, fail closed as `ENV_ERROR`
                // instead of letting the reviewer's verdict become `NEEDS_CONTROLLER`.
                if !stopped_flag && env_error.is_none() && use_ro_workspace && !ro_workspace.is_dir()
                {
                    let msg = clip(
                        &format!("只读审查快照缺失（{}）", ro_workspace.display()),
                        MAX_PARSE_ERROR_CHARS,
                    );
                    env_error = Some(msg.clone());
                    parse_error = Some(msg);
                    upgraded_by = None;
                    format_error = false;
                    verdict_str = REVIEW_VERDICT_ENV_ERROR.to_string();
                }

                let mut ro_violation_file: Option<PathBuf> = None;
                let ro_violation_sample: Vec<String> = ro_violations
                    .iter()
                    .take(RO_VIOLATION_SAMPLE_MAX)
                    .cloned()
                    .collect();
                if !ro_violations.is_empty() && !stopped_flag && env_error.is_none() {
                    let vfile = review_root.join(format!("round-{round}-ro-violations.txt"));
                    let mut body = ro_violations.join("\n");
                    body.push('\n');
                    let _ = write(&vfile, &body);
                    let extra = ro_violations.len().saturating_sub(ro_violation_sample.len());
                    let summary_msg = if extra > 0 {
                        format!(
                            "reviewer violated read-only boundary ({} files: {}, +{extra} more; see {})",
                            ro_violations.len(),
                            ro_violation_sample.join(", "),
                            vfile.display()
                        )
                    } else {
                        format!(
                            "reviewer violated read-only boundary ({} files: {})",
                            ro_violations.len(),
                            ro_violation_sample.join(", ")
                        )
                    };
                    parse_error = Some(clip(&summary_msg, MAX_PARSE_ERROR_CHARS));
                    upgraded_by = Some("readOnlyViolation".into());
                    verdict_str = "NEEDS_CONTROLLER".to_string();
                    ro_violation_file = Some(vfile);
                }

                // If no extra reviewer is running, cancel slow accept immediately on REWORK/error.
                if !extra_running
                    && parallel_with_slow
                    && !stopped_flag
                    && (verdict_str == "REWORK" || upgraded_by.as_deref() == Some("reviewerError"))
                {
                    slow_cancel.store(true, Ordering::SeqCst);
                    let spid = slow_holder.load(Ordering::SeqCst);
                    if spid > 0 {
                        crate::cleanup::record(run, spid);
                        kill_group(spid, 1.0);
                    }
                }

                let conclusion = crate::card::extract_reviewer_conclusion(&raw_answer);
                let focus_count = crate::card::parse_reviewer_focus(&raw_answer).len();

                ReviewRoundOutcome {
                    cancelled: rev_cancel.load(Ordering::SeqCst),
                    stopped: stopped_flag,
                    verdict_str,
                    raw_answer,
                    raw_first_line,
                    format_error,
                    format_retried: attempt.format_retried,
                    format_retry_failed,
                    env_error,
                    parse_error,
                    upgraded_by,
                    ro_violations,
                    ro_violation_sample,
                    ro_violation_file,
                    verdict_file,
                    diff_file: diff_file.clone(),
                    usage_dir: active_exec_dir,
                    round_started: round_started.clone(),
                    round_finished,
                    next_continuation,
                    conclusion,
                    focus_count,
                }
            };

            let extra_res = extra_handle.and_then(|h| h.join().ok());
            // When extra reviewer ran alongside main reviewer, cancel slow accept on REWORK only
            // when extra reviewer did NOT disagree (disagreement escalates to NEEDS_CONTROLLER).
            if extra_running
                && parallel_with_slow
                && !rev_out.stopped
                && !rev_out.cancelled
                && (rev_out.verdict_str == "REWORK"
                    || rev_out.upgraded_by.as_deref() == Some("reviewerError"))
            {
                let disagrees = extra_res.as_ref().is_some_and(|ex| {
                    extra_verdict_disagrees(&rev_out.verdict_str, ex.verdict.as_deref())
                });
                if !disagrees {
                    slow_cancel.store(true, Ordering::SeqCst);
                    let spid = slow_holder.load(Ordering::SeqCst);
                    if spid > 0 {
                        crate::cleanup::record(run, spid);
                        kill_group(spid, 1.0);
                    }
                }
            }
            let slow_res = slow_handle.map(|h| {
                h.join()
                    .unwrap_or_else(|_| crate::accept::PhaseOutcome::empty_ok())
            });
            (slow_res, rev_out, extra_res)
        });

        supervise::unregister_stop_holder(&slow_holder);
        supervise::unregister_stop_holder(&rev_holder);
        supervise::unregister_stop_holder(&extra_holder);

        if let Some(so) = &slow_outcome {
            for (k, v) in &so.passed_layers {
                let mut entry = v.clone();
                entry["passedRound"] = json!(round);
                passed_layers.insert(k.clone(), entry);
            }
        }
        let (_, accept_after_snap) = crate::accept::snapshot_for_accept(meta, run);
        let mut accept_doc = crate::accept::build_accept_summary(
            meta,
            run,
            &plan,
            fast_outcome.as_ref(),
            slow_outcome.as_ref(),
            parallel_with_slow,
            excluded,
            accept_before_snap.as_ref(),
            accept_after_snap.as_ref(),
        );
        let accept_ok = b(&accept_doc, "ok");
        let accept_cancelled = b(&accept_doc, "cancelled");
        if b(&accept_doc, "infrastructureFailure") {
            sum["infrastructureFailure"] = json!(true);
            sum["error"] =
                json!("remote infrastructure failure; retry does not spend a rework round");
        }
        if !accept_ok && !accept_cancelled && !lane::stopped() && !rev_outcome.stopped {
            apply_accept_failure_state(run, meta, &mut accept_doc, state, sum);
        }
        emit_accept_event(run, meta, &accept_doc);
        sum["accept"] = accept_doc;

        // 1. Supervisor stopped externally.
        if lane::stopped() || rev_outcome.stopped {
            *state = "stopped".into();
            sum["state"] = json!("stopped");
            let now = iso();
            let mut doc = json!({
                "status": "done",
                "verdict": Value::Null,
                "round": round,
                "reworkRounds": rework_rounds,
                "rubric": rubric_source,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": now,
                "finishedAt": now,
            });
            attach_review_model_fields(&mut doc, &active_review_cfg, active_fallback.as_ref());
            write_json(run.join("review.json"), &doc)?;
            sum["review"] = compact_review_summary(&doc);
            break;
        }

        // 2. Slow acceptance failed (not cancelled by reviewer): NEVER enter delivered,
        //    and a cancelled reviewer MUST NEVER be recorded as PASS.
        if !accept_ok && !accept_cancelled {
            if state != "blocked_mainline" {
                *state = "rejected".into();
                sum["state"] = json!("rejected");
            }
            let now = iso();
            if rev_outcome.cancelled {
                let mut round_entry = json!({
                    "round": round,
                    "verdict": Value::Null,
                    "cancelled": true,
                    "cancelledBy": "acceptFailed",
                    "diffFile": rev_outcome.diff_file.to_string_lossy(),
                    "incremental": incremental,
                    "startedAt": rev_outcome.round_started,
                    "finishedAt": rev_outcome.round_finished,
                });
                crate::usage::attach_reviewer_round_usage(&rev_outcome.usage_dir, &mut round_entry);
                rounds_history.push(round_entry);
                let mut final_doc = json!({
                    "status": "cancelled",
                    "verdict": Value::Null,
                    "cancelled": true,
                    "cancelledBy": "acceptFailed",
                    "round": round,
                    "reworkRounds": rework_rounds,
                    "rubric": rubric_source,
                    "needsAttention": false,
                    "rounds": rounds_history,
                    "startedAt": review_started_at,
                    "updatedAt": now,
                    "finishedAt": now,
                });
                attach_review_model_fields(
                    &mut final_doc,
                    &active_review_cfg,
                    active_fallback.as_ref(),
                );
                crate::usage::attach_review_totals(&rounds_history, &mut final_doc);
                write_json(run.join("review.json"), &final_doc)?;
                sum["review"] = compact_review_summary(&final_doc);
                events::emit(
                    &run_id,
                    &repo,
                    "review_verdict",
                    json!({
                        "name": run_name,
                        "round": round,
                        "verdict": Value::Null,
                        "cancelled": true,
                        "cancelledBy": "acceptFailed",
                    }),
                );
            } else {
                // Reviewer happened to finish before slow accept failed; persist raw verdict
                // but keep run state `rejected` (both must pass to enter `delivered`).
                write(&rev_outcome.verdict_file, &rev_outcome.raw_answer)?;
                let _ = write(run.join("review.md"), &rev_outcome.raw_answer);
                let mut round_entry = json!({
                    "round": round,
                    "verdict": rev_outcome.verdict_str,
                    "rawFirstLine": rev_outcome.raw_first_line,
                    "verdictFile": rev_outcome.verdict_file.to_string_lossy(),
                    "diffFile": rev_outcome.diff_file.to_string_lossy(),
                    "incremental": incremental,
                    "acceptFailed": true,
                    "startedAt": rev_outcome.round_started,
                    "finishedAt": rev_outcome.round_finished,
                });
                crate::usage::attach_reviewer_round_usage(&rev_outcome.usage_dir, &mut round_entry);
                rounds_history.push(round_entry);
                let mut final_doc = json!({
                    "status": "done",
                    "verdict": rev_outcome.verdict_str,
                    "acceptFailed": true,
                    "round": round,
                    "reworkRounds": rework_rounds,
                    "rubric": rubric_source,
                    "verdictFile": rev_outcome.verdict_file.to_string_lossy(),
                    "needsAttention": false,
                    "rounds": rounds_history,
                    "startedAt": review_started_at,
                    "updatedAt": now,
                    "finishedAt": now,
                });
                attach_review_model_fields(
                    &mut final_doc,
                    &active_review_cfg,
                    active_fallback.as_ref(),
                );
                if let Some(concl) = &rev_outcome.conclusion {
                    final_doc["conclusion"] = json!(concl);
                }
                if rev_outcome.focus_count > 0 {
                    final_doc["focusCount"] = json!(rev_outcome.focus_count);
                }
                crate::usage::attach_review_totals(&rounds_history, &mut final_doc);
                write_json(run.join("review.json"), &final_doc)?;
                sum["review"] = compact_review_summary(&final_doc);
            }
            break;
        }

        if let Some(cont) = rev_outcome.next_continuation {
            prev_review_continuation = Some(cont);
            prev_continuation_is_agy = is_agy_reviewer;
        }

        // Write exact raw reviewer verdict file (never modified or paraphrased).
        let round_raw_answer = rev_outcome.raw_answer;
        write(&rev_outcome.verdict_file, &round_raw_answer)?;
        let _ = write(run.join("review.md"), &round_raw_answer);

        let mut verdict_str = rev_outcome.verdict_str;
        let mut upgraded_by = rev_outcome.upgraded_by;
        let env_error = rev_outcome.env_error;
        let parse_error = rev_outcome.parse_error;
        let format_error = rev_outcome.format_error;
        let format_retried = rev_outcome.format_retried;
        let format_retry_failed = rev_outcome.format_retry_failed;
        let raw_first_line = rev_outcome.raw_first_line;
        let verdict_file = rev_outcome.verdict_file;
        let ro_violations = rev_outcome.ro_violations;
        let ro_violation_sample = rev_outcome.ro_violation_sample;
        let ro_violation_file = rev_outcome.ro_violation_file;
        let round_finished = rev_outcome.round_finished;
        let conclusion = rev_outcome.conclusion;
        let focus_count = rev_outcome.focus_count;
        let mut original_verdict: Option<String> = None;

        if verdict_str == REVIEW_VERDICT_ENV_ERROR {
            let err_msg = env_error
                .clone()
                .or_else(|| parse_error.clone())
                .unwrap_or_else(|| "审查环境错误：只读审查快照缺失".to_string());
            let err_short = clip(&err_msg, MAX_PARSE_ERROR_CHARS);
            let retry_cmd = review_env_error_retry_command(&run_id);
            let mut round_entry = json!({
                "round": round,
                "verdict": REVIEW_VERDICT_ENV_ERROR,
                "scope": review_scope,
                "scopeFiles": scope_files,
                "verdictFile": verdict_file.to_string_lossy(),
                "diffFile": rev_outcome.diff_file.to_string_lossy(),
                "incremental": incremental,
                "envError": true,
                "error": err_short,
                "retryCommand": retry_cmd,
                "startedAt": rev_outcome.round_started,
                "finishedAt": round_finished,
            });
            crate::usage::attach_reviewer_round_usage(&rev_outcome.usage_dir, &mut round_entry);
            rounds_history.push(round_entry);
            let mut final_doc = json!({
                "status": "env_error",
                "verdict": REVIEW_VERDICT_ENV_ERROR,
                "envError": true,
                "error": err_short,
                "parseError": err_short,
                "retryCommand": retry_cmd,
                "scope": review_scope,
                "scopeFiles": scope_files,
                "round": round,
                "reworkRounds": rework_rounds,
                "rubric": rubric_source,
                "verdictFile": verdict_file.to_string_lossy(),
                "needsAttention": true,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": round_finished,
                "finishedAt": round_finished,
            });
            if let Some(tree) = &last_valid_reviewed_tree {
                final_doc["reviewedTree"] = json!(tree);
            }
            if let Some(base) = &prev_reviewed_base {
                final_doc["reviewedBaseTree"] = json!(base);
            }
            attach_review_model_fields(
                &mut final_doc,
                &active_review_cfg,
                active_fallback.as_ref(),
            );
            crate::usage::attach_review_totals(&rounds_history, &mut final_doc);
            write_json(run.join("review.json"), &final_doc)?;
            sum["review"] = compact_review_summary(&final_doc);
            events::emit(
                &run_id,
                &repo,
                "review_verdict",
                json!({
                    "name": run_name,
                    "round": round,
                    "verdict": REVIEW_VERDICT_ENV_ERROR,
                    "envError": true,
                    "error": err_short,
                    "retryCommand": retry_cmd,
                }),
            );
            events::emit(
                &run_id,
                &repo,
                "needs_attention",
                json!({
                    "name": run_name,
                    "round": round,
                    "reworkRounds": rework_rounds,
                    "reason": "reviewEnvError",
                    "verdict": REVIEW_VERDICT_ENV_ERROR,
                    "detail": err_short,
                }),
            );
            *state = if accept_ok { "delivered" } else { "rejected" }.into();
            sum["state"] = json!(*state);
            break;
        }

        // Reconcile optional supplementary reviewer (`review.extra`): second opinion only;
        // when both verdicts are valid and disagree, mark `意见不一致` and escalate to NEEDS_CONTROLLER.
        let mut extra_json: Option<Value> = None;
        match extra_pre {
            ExtraPreDecision::None => {}
            ExtraPreDecision::Skipped {
                model,
                model_name,
                triggers,
                five_hour_pct,
                weekly_pct,
                reason,
            } => {
                let mut ex = json!({
                    "status": "skipped",
                    "model": model,
                    "modelName": model_name,
                    "triggers": triggers,
                    "skipReason": reason,
                    "disagreed": false,
                });
                if let Some(v) = five_hour_pct {
                    ex["fiveHourPct"] = json!(v);
                }
                if let Some(v) = weekly_pct {
                    ex["weeklyPct"] = json!(v);
                }
                extra_json = Some(ex);
            }
            ExtraPreDecision::Error {
                model,
                model_name,
                triggers,
                error,
            } => {
                extra_json = Some(json!({
                    "status": "error",
                    "model": model,
                    "modelName": model_name,
                    "triggers": triggers,
                    "error": error,
                    "disagreed": false,
                }));
            }
            ExtraPreDecision::Run {
                model,
                model_name,
                triggers,
                verdict_file: extra_vf,
                ..
            } => {
                if let Some(ex_out) = extra_outcome {
                    if !ex_out.cancelled {
                        if !ex_out.raw_answer.is_empty() {
                            let _ = write(&extra_vf, &ex_out.raw_answer);
                        }
                        if let Some(ex_v) = ex_out.verdict {
                            let disagreed = extra_verdict_disagrees(&verdict_str, Some(&ex_v));
                            let mut ex = json!({
                                "status": "done",
                                "model": model,
                                "modelName": model_name,
                                "triggers": triggers,
                                "verdict": ex_v,
                                "verdictFile": extra_vf.to_string_lossy(),
                                "disagreed": disagreed,
                            });
                            if let Some(c) = ex_out.conclusion {
                                ex["conclusion"] = json!(c);
                            }
                            extra_json = Some(ex);
                            if disagreed {
                                if verdict_str != "NEEDS_CONTROLLER" {
                                    original_verdict = Some(verdict_str.clone());
                                    verdict_str = "NEEDS_CONTROLLER".to_string();
                                } else if upgraded_by.is_none() {
                                    original_verdict = Some("NEEDS_CONTROLLER".to_string());
                                }
                                if upgraded_by.is_none() {
                                    upgraded_by = Some("extraReviewDisagreement".into());
                                }
                            }
                        } else if let Some(err) = ex_out.error {
                            extra_json = Some(json!({
                                "status": "error",
                                "model": model,
                                "modelName": model_name,
                                "triggers": triggers,
                                "error": err,
                                "disagreed": false,
                            }));
                        }
                    }
                }
            }
        }

        // Check `controllerReviewPaths` ONLY when NOT entering rework (upgrade PASS / NEEDS_CONTROLLER,
        // never hijack REWORK and skip automatic rework).
        let mut controller_review_json = Value::Null;
        let mut controller_paths: Vec<String> = Vec::new();
        let mut sign_hash = String::new();
        if verdict_str != "REWORK" {
            if let Some(hit) = evaluate_controller_paths(run, meta, false)? {
                controller_paths = hit.paths.clone();
                sign_hash = hit.hash.clone();
                controller_review_json = build_controller_review_json(run, &hit)?;
                if verdict_str != "NEEDS_CONTROLLER" {
                    original_verdict = Some(verdict_str.clone());
                    verdict_str = "NEEDS_CONTROLLER".to_string();
                } else if original_verdict.is_none() && upgraded_by.is_none() {
                    original_verdict = Some("NEEDS_CONTROLLER".to_string());
                }
                upgraded_by = Some("controllerReviewPaths".into());
            }
        }

        if verdict_str == "REWORK" && incremental && !current_total_base.is_empty() && !after_tree.is_empty() {
            let total_diff_path = round_dir.join("total.diff");
            if !total_diff_path.is_file() {
                if let Ok(raw) = git(
                    &top,
                    &[
                        "diff",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--color=never",
                        "--binary",
                        &current_total_base,
                        &after_tree,
                    ],
                ) {
                    let _ = write(&total_diff_path, String::from_utf8_lossy(&raw).as_ref());
                }
            }
        }

        let mut round_entry = json!({
            "round": round,
            "verdict": verdict_str,
            "scope": review_scope,
            "scopeFiles": scope_files,
            "rawFirstLine": raw_first_line,
            "verdictFile": verdict_file.to_string_lossy(),
            "diffFile": rev_outcome.diff_file.to_string_lossy(),
            "incremental": incremental,
            "startedAt": rev_outcome.round_started,
            "finishedAt": round_finished,
        });
        if !after_tree.is_empty() {
            round_entry["reviewedTree"] = json!(after_tree);
        }
        if !current_total_base.is_empty() {
            round_entry["reviewedBaseTree"] = json!(current_total_base);
        }
        if format_error {
            round_entry["formatError"] = json!(true);
        }
        if format_retried {
            round_entry["formatRetried"] = json!(true);
        }
        if format_retry_failed {
            round_entry["formatRetryFailed"] = json!(true);
        }
        if let Some(err) = &parse_error {
            round_entry["parseError"] = json!(err);
        }
        if let Some(orig) = &original_verdict {
            round_entry["originalVerdict"] = json!(orig);
        }
        if let Some(up) = &upgraded_by {
            round_entry["upgradedBy"] = json!(up);
        }
        if let Some(ref ex) = extra_json {
            round_entry["extra"] = ex.clone();
        }
        if !ro_violations.is_empty() {
            round_entry["roViolationCount"] = json!(ro_violations.len());
            round_entry["roViolationSample"] = json!(ro_violation_sample);
            if let Some(ref vf) = ro_violation_file {
                round_entry["roViolationFile"] = json!(vf.to_string_lossy());
            }
        }
        crate::usage::attach_reviewer_round_usage(&rev_outcome.usage_dir, &mut round_entry);
        rounds_history.push(round_entry);

        let mut verdict_ev = json!({
            "name": run_name,
            "round": round,
            "verdict": verdict_str,
            "reworkRounds": rework_rounds,
            "verdictFile": verdict_file.to_string_lossy(),
            "model": active_review_cfg["model"],
            "modelName": active_review_cfg["modelName"],
        });
        if format_error {
            verdict_ev["formatError"] = json!(true);
        }
        if let Some(orig) = &original_verdict {
            verdict_ev["originalVerdict"] = json!(orig);
        }
        if let Some(up) = &upgraded_by {
            verdict_ev["upgradedBy"] = json!(up);
        }
        if let Some(err) = &parse_error {
            verdict_ev["error"] = json!(clip(err, 200));
        }
        if !controller_paths.is_empty() {
            verdict_ev["controllerReviewPaths"] = json!(controller_paths);
            verdict_ev["signHash"] = json!(sign_hash);
        }
        events::emit(&run_id, &repo, "review_verdict", verdict_ev);

        if verdict_str == "PASS" {
            let mut final_doc = json!({
                "status": "done",
                "verdict": "PASS",
                "scope": review_scope,
                "scopeFiles": scope_files,
                "round": round,
                "reworkRounds": rework_rounds,
                "rubric": rubric_source,
                "verdictFile": verdict_file.to_string_lossy(),
                "reviewedTree": after_tree,
                "reviewedBaseTree": current_total_base,
                "needsAttention": rework_rounds >= NEEDS_ATTENTION_REWORK_ROUND,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": round_finished,
                "finishedAt": round_finished,
            });
            if let Some(cont) = &prev_review_continuation {
                final_doc["reviewContinuation"] = json!(cont);
                final_doc["reviewContinuationIsAgy"] = json!(prev_continuation_is_agy);
            }
            final_doc["reviewWorkdir"] = json!(reviewer_workdir.to_string_lossy());
            attach_review_model_fields(&mut final_doc, &active_review_cfg, active_fallback.as_ref());
            if format_retried {
                final_doc["formatRetried"] = json!(true);
            }
            if let Some(ref ex) = extra_json {
                final_doc["extra"] = ex.clone();
            }
            if let Some(concl) = &conclusion {
                final_doc["conclusion"] = json!(concl);
            }
            if focus_count > 0 {
                final_doc["focusCount"] = json!(focus_count);
            }
            crate::usage::attach_review_totals(&rounds_history, &mut final_doc);
            write_json(run.join("review.json"), &final_doc)?;
            sum["review"] = compact_review_summary(&final_doc);
            *state = "delivered".into();
            sum["state"] = json!("delivered");
            break;
        }

        if verdict_str == "NEEDS_CONTROLLER" {
            let reason = upgraded_by.as_deref().unwrap_or("needs_controller");
            let detail = if reason == "controllerReviewPaths" {
                format!(
                    "改动命中 controllerReviewPaths（{}），已升级为 NEEDS_CONTROLLER",
                    controller_paths.join("、")
                )
            } else if reason == "extraReviewDisagreement" {
                format!(
                    "主审查与补充审查意见不一致（主审查 {} / 补充审查 {}），已升级为 NEEDS_CONTROLLER",
                    original_verdict.as_deref().unwrap_or("NEEDS_CONTROLLER"),
                    extra_json.as_ref().map(|e| s(e, "verdict")).unwrap_or("")
                )
            } else if let Some(err) = &parse_error {
                err.clone()
            } else {
                format!("第 {round} 轮审查判定 NEEDS_CONTROLLER，请主控审阅")
            };
            let mut att_ev = json!({
                "name": run_name,
                "round": round,
                "reworkRounds": rework_rounds,
                "reason": reason,
                "verdict": "NEEDS_CONTROLLER",
                "detail": clip(&detail, 240),
                "verdictFile": verdict_file.to_string_lossy(),
            });
            if !controller_paths.is_empty() {
                att_ev["controllerReviewPaths"] = json!(controller_paths);
                att_ev["signHash"] = json!(sign_hash);
            }
            events::emit(&run_id, &repo, "needs_attention", att_ev);

            let round_review_valid = !format_error
                && !matches!(
                    upgraded_by.as_deref(),
                    Some("reviewerError" | "formatError" | "rubricError" | "readOnlyViolation")
                );
            let mut final_doc = json!({
                "status": "done",
                "verdict": "NEEDS_CONTROLLER",
                "scope": review_scope,
                "scopeFiles": scope_files,
                "round": round,
                "reworkRounds": rework_rounds,
                "rubric": rubric_source,
                "verdictFile": verdict_file.to_string_lossy(),
                "needsAttention": true,
                "formatError": format_error,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": round_finished,
                "finishedAt": round_finished,
            });
            if round_review_valid {
                if !after_tree.is_empty() {
                    final_doc["reviewedTree"] = json!(after_tree);
                }
                if !current_total_base.is_empty() {
                    final_doc["reviewedBaseTree"] = json!(current_total_base);
                }
            } else {
                if let Some(tree) = &last_valid_reviewed_tree {
                    final_doc["reviewedTree"] = json!(tree);
                }
                if let Some(base) = &prev_reviewed_base {
                    final_doc["reviewedBaseTree"] = json!(base);
                }
            }
            if let Some(cont) = &prev_review_continuation {
                final_doc["reviewContinuation"] = json!(cont);
                final_doc["reviewContinuationIsAgy"] = json!(prev_continuation_is_agy);
            }
            final_doc["reviewWorkdir"] = json!(reviewer_workdir.to_string_lossy());
            attach_review_model_fields(&mut final_doc, &active_review_cfg, active_fallback.as_ref());
            if format_retried {
                final_doc["formatRetried"] = json!(true);
            }
            if format_retry_failed {
                final_doc["formatRetryFailed"] = json!(true);
            }
            if let Some(ref ex) = extra_json {
                final_doc["extra"] = ex.clone();
            }
            if let Some(concl) = &conclusion {
                final_doc["conclusion"] = json!(concl);
            }
            if focus_count > 0 {
                final_doc["focusCount"] = json!(focus_count);
            }
            if let Some(err) = &parse_error {
                final_doc["parseError"] = json!(err);
            }
            if let Some(orig) = &original_verdict {
                final_doc["originalVerdict"] = json!(orig);
            }
            if let Some(up) = &upgraded_by {
                final_doc["upgradedBy"] = json!(up);
            }
            if !ro_violations.is_empty() {
                final_doc["roViolationCount"] = json!(ro_violations.len());
                final_doc["roViolationSample"] = json!(ro_violation_sample);
                if let Some(ref vf) = ro_violation_file {
                    final_doc["roViolationFile"] = json!(vf.to_string_lossy());
                }
            }
            if controller_review_json.is_object() {
                final_doc["controllerReview"] = controller_review_json;
            }
            crate::usage::attach_review_totals(&rounds_history, &mut final_doc);
            write_json(run.join("review.json"), &final_doc)?;
            sum["review"] = compact_review_summary(&final_doc);
            *state = if accept_ok { "delivered" } else { "rejected" }.into();
            sum["state"] = json!(*state);
            break;
        }

        // Verdict is `REWORK`: send raw verdict file verbatim back to the author.
        rework_rounds += 1;
        let attention_now = rework_rounds >= NEEDS_ATTENTION_REWORK_ROUND;
        let mut reworking_doc = json!({
            "status": "reworking",
            "verdict": "REWORK",
            "scope": review_scope,
            "scopeFiles": scope_files,
            "round": round,
            "reworkRounds": rework_rounds,
            "rubric": rubric_source,
            "verdictFile": verdict_file.to_string_lossy(),
            "reviewedTree": after_tree,
            "reviewedBaseTree": current_total_base,
            "needsAttention": attention_now,
            "rounds": rounds_history,
            "startedAt": review_started_at,
            "updatedAt": iso(),
        });
        if let Some(cont) = &prev_review_continuation {
            reworking_doc["reviewContinuation"] = json!(cont);
            reworking_doc["reviewContinuationIsAgy"] = json!(prev_continuation_is_agy);
        }
        reworking_doc["reviewWorkdir"] = json!(reviewer_workdir.to_string_lossy());
        prev_review_workdir = Some(reviewer_workdir.clone());
        attach_review_model_fields(&mut reworking_doc, &active_review_cfg, active_fallback.as_ref());
        write_json(run.join("review.json"), &reworking_doc)?;

        events::emit(
            &run_id,
            &repo,
            "rework_sent",
            json!({
                "name": run_name,
                "round": round,
                "reworkRounds": rework_rounds,
                "verdictFile": verdict_file.to_string_lossy(),
            }),
        );

        if attention_now {
            events::emit(
                &run_id,
                &repo,
                "needs_attention",
                json!({
                    "name": run_name,
                    "round": round,
                    "reworkRounds": rework_rounds,
                    "reason": "rework_rounds",
                    "verdict": "REWORK",
                    "verdictFile": verdict_file.to_string_lossy(),
                    "detail": format!("审-改返工已达第 {rework_rounds} 轮，照常继续运行，请主控关注"),
                }),
            );
        }

        let prev_tree_for_delta = (!after_tree.is_empty()).then_some(after_tree.clone());
        last_valid_reviewed_tree = prev_tree_for_delta.clone();
        last_valid_verdict_file = Some(verdict_file.to_string_lossy().into_owned());
        prev_reviewed_tree = if review_full {
            None
        } else {
            prev_tree_for_delta.clone()
        };
        prev_reviewed_base = (!current_total_base.is_empty()).then_some(current_total_base.clone());
        if !round_raw_answer.trim().is_empty() {
            prev_verdict_text = Some(round_raw_answer);
        }

        let Some(mut author_next) = supervise::continuation_with_file(meta, run, &verdict_file) else {
            let err = "author has no saved session to continue for rework";
            let now = iso();
            let mut final_doc = json!({
                "status": "done",
                "verdict": "NEEDS_CONTROLLER",
                "originalVerdict": "REWORK",
                "upgradedBy": "missingAuthorSession",
                "parseError": err,
                "round": round,
                "reworkRounds": rework_rounds,
                "rubric": rubric_source,
                "verdictFile": verdict_file.to_string_lossy(),
                "needsAttention": true,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": now,
                "finishedAt": now,
            });
            if let Some(tree) = &last_valid_reviewed_tree {
                final_doc["reviewedTree"] = json!(tree);
            }
            if let Some(base) = &prev_reviewed_base {
                final_doc["reviewedBaseTree"] = json!(base);
            }
            if let Some(cont) = &prev_review_continuation {
                final_doc["reviewContinuation"] = json!(cont);
                final_doc["reviewContinuationIsAgy"] = json!(prev_continuation_is_agy);
            }
            final_doc["reviewWorkdir"] = json!(reviewer_workdir.to_string_lossy());
            attach_review_model_fields(&mut final_doc, &active_review_cfg, active_fallback.as_ref());
            write_json(run.join("review.json"), &final_doc)?;
            sum["review"] = compact_review_summary(&final_doc);
            *state = if accept_ok { "delivered" } else { "rejected" }.into();
            sum["state"] = json!(*state);
            events::emit(
                &run_id,
                &repo,
                "needs_attention",
                json!({
                    "name": run_name,
                    "round": round,
                    "reworkRounds": rework_rounds,
                    "reason": "missingAuthorSession",
                    "verdict": "NEEDS_CONTROLLER",
                    "detail": err,
                }),
            );
            break;
        };

        // Preserve previous round's result before rework overwrites `result.md` (compression as index).
        let prev_max = crate::runs::max_round_result_num(run);
        let prev_round_num = if prev_max > 0 { prev_max } else { round as usize };
        let prev_round_result = run.join(format!("round-{prev_round_num}-result.md"));
        if !prev_round_result.is_file() {
            let prev_text = if !answer.trim().is_empty() {
                format!("{}\n", answer.trim_end_matches('\n'))
            } else {
                read(run.join("result.md"))
            };
            if !prev_text.trim().is_empty() {
                write(&prev_round_result, prev_text)?;
            }
        }

        let mut rework_ev = json!({
            "e": "review_rework",
            "round": round,
            "reworkRounds": rework_rounds,
            "verdictFile": verdict_file.to_string_lossy(),
        });
        if prev_round_result.is_file() {
            rework_ev["prevResultFile"] = json!(prev_round_result.to_string_lossy());
        }
        supervise::log_event(run, rework_ev);

        author_next["authorRound"] = json!(round + 1);
        author_next["isRework"] = json!(true);
        let (mut author_verdict, mut author_answer, mut next_attempts) =
            supervise::attempts_from(&author_next, run, *attempts + 1, holder.clone(), grace.clone());
        let resumed = supervise::resume_transient(
            &author_next,
            run,
            (author_verdict, author_answer, next_attempts),
            holder.clone(),
            grace.clone(),
        );
        (author_verdict, author_answer, next_attempts) =
            (resumed.verdict, resumed.answer, resumed.attempts);
        (author_verdict, author_answer, next_attempts) = supervise::try_agy_exhaustion_handoff(
            meta,
            run,
            (author_verdict, author_answer, next_attempts),
            Some(&verdict_file),
            holder.clone(),
            grace.clone(),
        );
        (author_verdict, author_answer, next_attempts) = supervise::resume_stalled(
            &author_next,
            run,
            (author_verdict, author_answer, next_attempts),
            holder.clone(),
            grace.clone(),
        );
        let recovered = supervise::recover_policy_refusal(
            &author_next,
            run,
            (author_verdict, author_answer, next_attempts),
            holder.clone(),
            grace.clone(),
        );
        (author_verdict, author_answer, next_attempts) =
            (recovered.verdict, recovered.answer, recovered.attempts);
        *policy_refused = recovered.refused;
        *policy_rewrite_attempted |= recovered.rewrite_attempted;
        *attempts = next_attempts;
        if !*policy_refused && !author_answer.trim().is_empty() {
            *answer = author_answer;
            let formatted = format!("{}\n", answer.trim_end_matches('\n'));
            let next_round_num = crate::runs::max_round_result_num(run) + 1;
            write(run.join(format!("round-{next_round_num}-result.md")), &formatted)?;
            write(run.join("result.md"), &formatted)?;
        }
        let pre = supervise::settle_pre_accept(
            meta,
            run,
            &author_verdict,
            &None,
            holder.clone(),
            answer,
        );
        *state = pre.state;
        *rec = pre.rec;
        *changed = pre.changed;
        let mut new_sum = pre.sum;
        if !pre.can_accept {
            let now = iso();
            let mut final_doc = json!({
                "status": "done",
                "verdict": "REWORK",
                "round": round,
                "reworkRounds": rework_rounds,
                "rubric": rubric_source,
                "verdictFile": verdict_file.to_string_lossy(),
                "needsAttention": attention_now,
                "stoppedDuringRework": true,
                "reworkOutcome": state,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": now,
                "finishedAt": now,
            });
            if let Some(tree) = &last_valid_reviewed_tree {
                final_doc["reviewedTree"] = json!(tree);
            }
            if let Some(base) = &prev_reviewed_base {
                final_doc["reviewedBaseTree"] = json!(base);
            }
            if let Some(cont) = &prev_review_continuation {
                final_doc["reviewContinuation"] = json!(cont);
                final_doc["reviewContinuationIsAgy"] = json!(prev_continuation_is_agy);
            }
            final_doc["reviewWorkdir"] = json!(reviewer_workdir.to_string_lossy());
            attach_review_model_fields(&mut final_doc, &active_review_cfg, active_fallback.as_ref());
            write_json(run.join("review.json"), &final_doc)?;
            new_sum["review"] = compact_review_summary(&final_doc);
            *sum = new_sum;
            break;
        }

        // Compute which files changed in this rework turn so the next round ONLY re-runs affected layers.
        let new_changes_doc = json(run.join("changes.json"));
        let new_after_tree = s(&new_changes_doc, "after").to_string();
        rework_delta_files = prev_tree_for_delta
            .as_deref()
            .and_then(|prev_t| diff_tree_paths(&top, prev_t, &new_after_tree));
        *sum = new_sum;
        round += 1;
    }

    Ok(())
}

/// Print controller-review diff + sign hash (or `NEEDS_CONTROLLER` verdict summary) in human delivery output.
pub fn print_delivery_review(run: &Path) {
    let review = {
        let f = json(run.join("review.json"));
        if f.is_object() {
            f
        } else {
            json(run.join("summary.json"))["review"].clone()
        }
    };
    if !review.is_object() {
        return;
    }
    let id = run.file_name().unwrap_or_default().to_string_lossy();
    let cr = &review["controllerReview"];
    if cr.is_object() && !s(cr, "hash").is_empty() {
        let paths = strings(&cr["paths"]);
        let hash = s(cr, "hash");
        let diff_text = if !s(cr, "diff").is_empty() {
            s(cr, "diff").to_string()
        } else {
            read(s(cr, "diffFile"))
        };
        println!("\n===== controller-review (NEEDS_CONTROLLER): {id} =====");
        println!("命中路径：{}", paths.join("、"));
        println!("签字哈希：{hash}");
        println!("签字命令：{} sign {id}", script().display());
        if !diff_text.trim().is_empty() {
            println!("{}", diff_text.trim_end_matches('\n'));
        }
        println!("===== end controller-review: {id} =====");
    } else if s(&review, "verdict") == REVIEW_VERDICT_ENV_ERROR || b(&review, "envError") {
        let err = {
            let e = s(&review, "error");
            if !e.is_empty() {
                e
            } else {
                s(&review, "parseError")
            }
        };
        let retry = {
            let r = s(&review, "retryCommand");
            if !r.is_empty() {
                r.to_string()
            } else {
                review_env_error_retry_command(&id)
            }
        };
        println!("\n===== review (审查环境错误 / ENV_ERROR): {id} =====");
        if !err.is_empty() {
            println!("原因：{err}");
        }
        println!("重试命令：{retry}");
        println!("===== end review: {id} =====");
    } else if s(&review, "verdict") == "NEEDS_CONTROLLER" {
        let vf = s(&review, "verdictFile");
        println!("\n===== review (NEEDS_CONTROLLER): {id} =====");
        if !s(&review, "upgradedBy").is_empty() {
            println!("升级原因：{}", s(&review, "upgradedBy"));
        }
        if !s(&review, "parseError").is_empty() {
            println!("详情：{}", s(&review, "parseError"));
        }
        if !s(&review, "roViolationFile").is_empty() {
            println!(
                "只读违规清单（{} 项）：{}",
                n(&review, "roViolationCount"),
                s(&review, "roViolationFile")
            );
        }
        if !vf.is_empty() {
            println!("审查结论文件：{vf}");
            let body = read(vf);
            if !body.trim().is_empty() {
                println!("{}", body.trim_end_matches('\n'));
            }
        }
        println!("===== end review: {id} =====");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_nist_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn glob_match_handles_stars_double_stars_and_dirs() {
        assert!(glob_match("skills/*/SKILL.md", "skills/delegate/SKILL.md"));
        assert!(!glob_match("skills/*/SKILL.md", "skills/a/b/SKILL.md"));
        assert!(glob_match("skills/**/SKILL.md", "skills/SKILL.md"));
        assert!(glob_match("skills/**/SKILL.md", "skills/a/b/SKILL.md"));
        assert!(glob_match("prompts/**", "prompts/system.md"));
        assert!(glob_match("prompts/**", "prompts/nested/tool.md"));
        assert!(!glob_match("prompts/**", "other/prompts/a.md"));
        assert!(glob_match("**/SKILL.md", "SKILL.md"));
        assert!(glob_match("**/SKILL.md", "skills/delegate/SKILL.md"));
        assert!(glob_match("prompts/", "prompts/a.md"));
        assert!(glob_match("prompts/", "prompts/sub/b.md"));
        assert!(!glob_match("prompts/", "prompts2/a.md"));
        assert!(glob_match("a?.txt", "ab.txt"));
        assert!(!glob_match("a?.txt", "a/.txt"));
    }

    #[test]
    fn parse_verdict_reads_natural_openings_and_conclusion_sections_safely() {
        // Classic exact tokens still work.
        assert_eq!(parse_verdict("PASS\nLooks good"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("PASS\r\nDetails"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("REWORK\n1. Fix foo"), ParsedVerdict::Rework);
        assert_eq!(
            parse_verdict("NEEDS_CONTROLLER\nAmbiguous spec"),
            ParsedVerdict::NeedsController
        );

        // Relaxed Markdown/prose openings in first 5 non-empty lines.
        assert_eq!(parse_verdict("**PASS**\n无阻塞问题"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("PASS: ok\n"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("\n# 代码审查\n**审查结论**：通过，无需返工。\n"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("审查通过，可合入，无阻塞问题。"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("结论：需返工 — 边界条件漏了空数组。"), ParsedVerdict::Rework);
        assert_eq!(parse_verdict("【建议返工】需要修改 parser。"), ParsedVerdict::Rework);
        assert_eq!(parse_verdict("Verdict: NEEDS_CONTROLLER - schema trade-off"), ParsedVerdict::NeedsController);
        assert_eq!(parse_verdict("## 审查报告\n\n需主控决定：是否保留旧开关。"), ParsedVerdict::NeedsController);

        // Conclusion section beyond first 5 lines.
        let late_section = "# 核查记录\n1. 文件 A\n2. 文件 B\n3. 文件 C\n4. 文件 D\n5. 文件 E\n\n## 结论\n审查通过，可合入。\n";
        assert_eq!(parse_verdict(late_section), ParsedVerdict::Pass);

        // Negations & conflicts must NEVER falsely match Pass!
        assert_eq!(parse_verdict("不通过，核心边界未处理"), ParsedVerdict::Rework);
        assert_eq!(parse_verdict("未通过审查，需修改测试"), ParsedVerdict::Rework);
        assert_eq!(parse_verdict("Verdict: not pass"), ParsedVerdict::Rework);
        assert_eq!(parse_verdict("核心路径通过，但错误处理需返工"), ParsedVerdict::Rework);
        assert_eq!(parse_verdict("通过，但边界测试仍未通过"), ParsedVerdict::Rework);

        // Re-review report headings and pass sentences mentioning prior rework/failures must stay Pass.
        assert_eq!(
            parse_verdict("# 第 1 轮返工续审\n\n通过，上轮问题已修复。"),
            ParsedVerdict::Pass
        );
        assert_eq!(parse_verdict("# Rework Review\n\nPASS"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("# PASS\n无阻塞问题"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("## 结论：通过"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("通过，本轮返工已修复全部问题。"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("PASS: rework items verified."), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("通过，未发现需要修改的问题。"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("通过：已修复此前测试未通过的问题。"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("通过，未发现需要主控拍板的事项。"), ParsedVerdict::Pass);

        // Prepositional / conditional / unclear prose must NOT default to Pass.
        assert!(matches!(parse_verdict(""), ParsedVerdict::Invalid(_)));
        assert!(matches!(parse_verdict("等 clippy 跑完再给结论。"), ParsedVerdict::Invalid(_)));
        assert!(matches!(parse_verdict("等测试通过后再给结论。"), ParsedVerdict::Invalid(_)));
        assert!(matches!(parse_verdict("通过读取源码，我注意到以下细节。"), ParsedVerdict::Invalid(_)));
        assert!(matches!(parse_verdict("LGTM\n"), ParsedVerdict::Invalid(_)));
    }

    #[test]
    fn validate_review_and_controller_paths_config() {
        assert!(validate_review_config(None).is_ok());
        assert!(validate_review_config(Some(&json!(false))).is_ok());
        assert!(validate_review_config(Some(&json!(true))).is_ok());
        assert!(validate_review_config(Some(&json!({"tier": "review", "rubric": "docs/rubric.md"}))).is_ok());
        assert!(validate_review_config(Some(&json!({"enabled": false}))).is_ok());
        assert!(validate_review_config(Some(&json!({"tier": "write"}))).is_err());
        assert!(validate_review_config(Some(&json!({"unknown": 1}))).is_err());
        assert!(validate_review_config(Some(&json!("review"))).is_err());

        assert!(validate_controller_review_paths(None).is_ok());
        assert!(validate_controller_review_paths(Some(&json!(["skills/*/SKILL.md"]))).is_ok());
        assert!(validate_controller_review_paths(Some(&json!([""]))).is_err());
        assert!(validate_controller_review_paths(Some(&json!("skills/*"))).is_err());
    }

    #[test]
    fn ro_guard_excludes_dep_build_and_gitignore_but_flags_tracked_and_unignored() {
        let base = std::env::temp_dir().join(format!(
            "delegate-review-ro-{}-{}",
            std::process::id(),
            now_ns()
        ));
        let repo = base.join("repo");
        let ro_ws = base.join("ro-ws");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("dist")).unwrap();
        fs::write(repo.join(".gitignore"), "*.log\ncustom-cache/\n").unwrap();
        fs::write(repo.join("src/app.ts"), "export const v = 1;\n").unwrap();
        fs::write(repo.join("dist/tracked.js"), "console.log('tracked');\n").unwrap();

        let run_git = |args: &[&str]| {
            let out = git_command()
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        };
        run_git(&["init", "-q"]);
        run_git(&["add", "-f", ".gitignore", "src/app.ts", "dist/tracked.js"]);
        run_git(&["commit", "-qm", "init"]);

        let head_tree = git_text(&repo, &["rev-parse", "HEAD^{tree}"]).unwrap();
        prepare_ro_workspace(&repo, &head_tree, &ro_ws, &base).unwrap();
        let before = dir_manifest_for_repo(&ro_ws, &repo).unwrap();

        // Simulate reviewer running pnpm/cargo/pytest creating dep/build/ignored outputs
        fs::create_dir_all(ro_ws.join("apps/web/node_modules/pkg")).unwrap();
        fs::write(ro_ws.join("apps/web/node_modules/pkg/index.js"), "module.exports = {};").unwrap();
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(
                ro_ws.join("apps/web/node_modules/pkg"),
                ro_ws.join("apps/web/node_modules/pkg-link"),
            );
        }
        fs::create_dir_all(ro_ws.join("target/debug")).unwrap();
        fs::write(ro_ws.join("target/debug/bin"), "elf").unwrap();
        fs::create_dir_all(ro_ws.join(".venv/lib")).unwrap();
        fs::write(ro_ws.join(".venv/lib/site.py"), "").unwrap();
        fs::create_dir_all(ro_ws.join("pkg/__pycache__")).unwrap();
        fs::write(ro_ws.join("pkg/__pycache__/a.pyc"), "pyc").unwrap();
        fs::write(ro_ws.join("dist/bundle.js"), "untracked dist").unwrap();
        fs::create_dir_all(ro_ws.join("test-results")).unwrap();
        fs::write(ro_ws.join("test-results/shot.png"), "png").unwrap();
        fs::write(ro_ws.join("run.log"), "ignored by gitignore").unwrap();
        fs::create_dir_all(ro_ws.join("custom-cache")).unwrap();
        fs::write(ro_ws.join("custom-cache/item.bin"), "ignored dir").unwrap();

        let after_clean = dir_manifest_for_repo(&ro_ws, &repo).unwrap();
        let clean_violations = filter_ro_violations(&repo, diff_manifests(&before, &after_clean));
        assert!(
            clean_violations.is_empty(),
            "dep/build/ignored files must not be violations: {clean_violations:?}"
        );

        // Now modify a tracked file (even under dist/), modify src/app.ts, and add an unignored new file
        fs::write(ro_ws.join("src/app.ts"), "export const v = 2;\n").unwrap();
        fs::write(ro_ws.join("dist/tracked.js"), "console.log('tampered');\n").unwrap();
        fs::write(ro_ws.join("src/new_unignored.ts"), "export const x = 1;\n").unwrap();

        let after_dirty = dir_manifest_for_repo(&ro_ws, &repo).unwrap();
        let violations = filter_ro_violations(&repo, diff_manifests(&before, &after_dirty));
        assert_eq!(
            violations,
            vec![
                "dist/tracked.js".to_string(),
                "src/app.ts".to_string(),
                "src/new_unignored.ts".to_string(),
            ]
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ro_workspace_outside_repo_is_searchable_by_rg_despite_local_gitignore() {
        let base = std::env::temp_dir().join(format!(
            "delegate-review-rg-{}-{}",
            std::process::id(),
            now_ns()
        ));
        let repo = base.join("repo");
        let scratch = base.join("scratch");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(&scratch).unwrap();
        fs::write(repo.join(".gitignore"), ".local/\n").unwrap();
        fs::write(repo.join("src/needle.rs"), "pub fn unique_ro_needle() -> u32 { 42 }\n").unwrap();

        let run_git = |args: &[&str]| {
            let out = git_command()
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        };
        run_git(&["init", "-q"]);
        run_git(&["add", ".gitignore", "src/needle.rs"]);
        run_git(&["commit", "-qm", "init"]);

        let head_tree = git_text(&repo, &["rev-parse", "HEAD^{tree}"]).unwrap();
        // ro_workspace_dir lives under `<cache>/delegate/review-ro/<run>`, outside any repo `.local/`.
        let run_id = format!("test-rg-{}-{}", std::process::id(), now_ns());
        let ro_ws = ro_workspace_dir(&run_id);
        let _guard = CleanupDir(ro_ws.clone());
        assert!(!ro_ws.starts_with(&repo));
        assert!(ro_ws.to_string_lossy().contains("review-ro"));

        prepare_ro_workspace(&repo, &head_tree, &ro_ws, &scratch).unwrap();
        assert!(ro_ws.join("src/needle.rs").is_file());

        // Verify `rg` finds `unique_ro_needle` inside `ro_ws` (whereas an in-repo `.local/...` checkout would be ignored).
        if let Ok(out) = std::process::Command::new("rg")
            .args(["-l", "unique_ro_needle"])
            .current_dir(&ro_ws)
            .output()
        {
            assert!(out.status.success(), "rg failed: {}", String::from_utf8_lossy(&out.stderr));
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(stdout.contains("src/needle.rs"), "rg returned: {stdout:?}");
        }

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn compact_review_summary_strips_bloat_and_reads_legacy_records() {
        let huge_list: Vec<String> = (0..500).map(|i| format!("node_modules/pkg-{i}/index.js")).collect();
        let huge_diff = format!("diff --git a/p.md b/p.md\n+{}", "x".repeat(50_000));
        let legacy = json!({
            "status": "done",
            "verdict": "NEEDS_CONTROLLER",
            "originalVerdict": "PASS",
            "round": 1,
            "reworkRounds": 0,
            "needsAttention": true,
            "upgradedBy": "readOnlyViolation",
            "parseError": format!("reviewer violated read-only boundary: {}", huge_list.join(", ")),
            "readOnlyViolation": huge_list,
            "verdictFile": "/tmp/run/review/round-1-verdict.md",
            "controllerReview": {
                "paths": ["prompts/a.md"],
                "hash": "sha256:abc",
                "diffFile": "/tmp/run/controller-review.diff",
                "diff": huge_diff,
            },
            "rounds": [{"round": 1, "readOnlyViolation": huge_list}],
        });

        let compact = compact_review_summary(&legacy);
        let serialized = serde_json::to_string(&compact).unwrap();
        assert!(serialized.len() < 2048, "compact summary too large: {} bytes", serialized.len());
        assert!(compact.get("rounds").is_none());
        assert!(compact.get("readOnlyViolation").is_none());
        assert_eq!(compact["roViolationCount"], json!(500));
        assert_eq!(compact["roViolationSample"].as_array().unwrap().len(), 5);
        assert!(s(&compact, "parseError").chars().count() <= MAX_PARSE_ERROR_CHARS + 3);
        assert!(compact["controllerReview"].get("diff").is_none());
        assert_eq!(compact["controllerReview"]["diffFile"], json!("/tmp/run/controller-review.diff"));
        assert_eq!(compact["controllerReview"]["diffBytes"], json!(huge_diff.len()));
    }

    #[test]
    fn sign_status_non_live_reads_json_without_git() {
        let dir = std::env::temp_dir().join(format!(
            "delegate-sign-nonlive-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&dir).unwrap();
        let meta = json!({"mode": "write", "worktree": {"source": "/nonexistent-repo-xyz", "path": "/nonexistent-wt"}});
        let review = json!({
            "verdict": "NEEDS_CONTROLLER",
            "controllerReview": {
                "paths": ["prompts/a.md"],
                "hash": "sha256:1111",
                "diffFile": dir.join("controller-review.diff").to_string_lossy(),
                "diffBytes": 12,
            }
        });
        let unsigned = sign_status(&dir, &meta, &review, false).unwrap();
        assert_eq!(unsigned["required"], json!(true));
        assert_eq!(unsigned["signed"], json!(false));
        assert_eq!(unsigned["valid"], json!(false));

        write_json(dir.join("sign.json"), &json!({"hash": "sha256:1111", "signedAt": "2026-01-01T00:00:00Z"})).unwrap();
        let valid = sign_status(&dir, &meta, &review, false).unwrap();
        assert_eq!(valid["signed"], json!(true));
        assert_eq!(valid["valid"], json!(true));
        assert_eq!(valid["invalidated"], json!(false));

        write_json(dir.join("sign.json"), &json!({"hash": "sha256:2222"})).unwrap();
        let inv = sign_status(&dir, &meta, &review, false).unwrap();
        assert_eq!(inv["valid"], json!(false));
        assert_eq!(inv["invalidated"], json!(true));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn original_task_brief_concatenates_full_reply_chain_including_archived_ancestors() {
        let root_tmp = std::env::temp_dir().join(format!(
            "delegate-brief-chain-{}-{}",
            std::process::id(),
            now_ns()
        ));
        let runs_dir = root_tmp.join("runs");
        let r1 = runs_dir.join("20261123-100000-root");
        let r2 = runs_dir.join("20261123-110000-mid");
        let r3 = runs_dir.join("20261123-121617-c9cb");
        fs::create_dir_all(&r1).unwrap();
        fs::create_dir_all(&r2).unwrap();
        fs::create_dir_all(&r3).unwrap();

        write_json(r1.join("meta.json"), &json!({"run": "20261123-100000-root"})).unwrap();
        write(r1.join("prompt.md"), "旧方向：按方案 A 实现").unwrap();
        write_json(
            r2.join("meta.json"),
            &json!({"run": "20261123-110000-mid", "parent": "20261123-100000-root"}),
        )
        .unwrap();
        write(r2.join("prompt.md"), "主控拍板：改为方案 B").unwrap();
        let m3 = json!({"run": "20261123-121617-c9cb", "parent": "20261123-110000-mid"});
        write_json(r3.join("meta.json"), &m3).unwrap();
        write(r3.join("prompt.md"), "按方案 B 修完剩余 3 处缺陷").unwrap();

        // Single run returns its brief directly without headings.
        let single = original_task_brief(&r1, &json!({"run": "20261123-100000-root"}));
        assert_eq!(single, "旧方向：按方案 A 实现");

        // Reply chain preserves root + every reply in chronological order with latest marked.
        let chain_brief = original_task_brief(&r3, &m3);
        let p1 = chain_brief.find("旧方向：按方案 A 实现").expect("root brief missing");
        let p2 = chain_brief.find("主控拍板：改为方案 B").expect("mid reply missing");
        let p3 = chain_brief
            .find("按方案 B 修完剩余 3 处缺陷")
            .expect("latest reply missing");
        assert!(p1 < p2 && p2 < p3, "chronological order broken: {chain_brief}");
        assert!(chain_brief.contains("初始说明（20261123-100000-root）"));
        assert!(chain_brief.contains("续做说明 #1（20261123-110000-mid）"));
        assert!(chain_brief.contains("续做说明 #2（20261123-121617-c9cb，最新"));
        assert!(chain_brief.contains("后来的决定覆盖原说明"));
        assert!(chain_brief.contains("原说明与追加/续做冲突时，在审查结论里明说依据哪条"));

        let _ = fs::remove_dir_all(&root_tmp);
    }

    #[test]
    fn original_task_brief_includes_delivered_in_run_and_steer_appends_and_excludes_fallback() {
        let root_tmp = std::env::temp_dir().join(format!(
            "delegate-brief-appends-{}-{}",
            std::process::id(),
            now_ns()
        ));
        let runs_dir = root_tmp.join("runs");
        let r1 = runs_dir.join("20261127-144457-root");
        let r2 = runs_dir.join("20261127-150000-reply");
        fs::create_dir_all(&r1).unwrap();
        fs::create_dir_all(&r2).unwrap();

        let m1 = json!({"run": "20261127-144457-root"});
        write_json(r1.join("meta.json"), &m1).unwrap();
        write(r1.join("prompt.md"), "原说明 A：句首名字不算直呼").unwrap();
        write_json(
            r1.join("appends.json"),
            &json!({
                "closed": true,
                "nextSeq": 6,
                "items": [
                    {
                        "seq": 2,
                        "message": "追加决定 B2：同时补单测",
                        "delivery": "steer",
                        "deliveredAt": "2026-11-27T14:46:00Z",
                        "cancelledAt": null
                    },
                    {
                        "seq": 1,
                        "message": "追加决定 B1：句首名字一律算直呼",
                        "delivery": "in_run",
                        "deliveredAt": "2026-11-27T14:45:30Z",
                        "cancelledAt": null
                    },
                    {
                        "seq": 3,
                        "message": "已取消追加：不应出现",
                        "delivery": "in_run",
                        "deliveredAt": "2026-11-27T14:46:30Z",
                        "cancelledAt": "2026-11-27T14:46:40Z"
                    },
                    {
                        "seq": 4,
                        "message": "未投递追加：不应出现",
                        "delivery": "in_run",
                        "deliveredAt": null,
                        "cancelledAt": null
                    },
                    {
                        "seq": 5,
                        "message": "续做说明 C：顺便清理注释",
                        "delivery": "fallback",
                        "deliveredAt": "2026-11-27T15:00:00Z",
                        "cancelledAt": null
                    }
                ]
            }),
        )
        .unwrap();

        // Single run with delivered in_run + steer appends: sorted by seq, includes precedence rule,
        // excludes cancelled/undelivered/fallback items.
        let single_with_appends = original_task_brief(&r1, &m1);
        assert!(single_with_appends.contains("后来的决定覆盖原说明"));
        assert!(single_with_appends.contains("原说明与追加/续做冲突时，在审查结论里明说依据哪条"));
        let pa = single_with_appends
            .find("原说明 A：句首名字不算直呼")
            .expect("root prompt missing");
        let pb1 = single_with_appends
            .find("追加决定 B1：句首名字一律算直呼")
            .expect("in_run append missing");
        let pb2 = single_with_appends
            .find("追加决定 B2：同时补单测")
            .expect("steer append missing");
        assert!(pa < pb1 && pb1 < pb2, "order broken: {single_with_appends}");
        assert!(single_with_appends.contains("初始说明（20261127-144457-root）"));
        assert!(single_with_appends.contains("主控追加 #1（20261127-144457-root）"));
        assert!(single_with_appends.contains("主控追加 #2（20261127-144457-root，最新"));
        assert!(!single_with_appends.contains("已取消追加"));
        assert!(!single_with_appends.contains("未投递追加"));
        assert!(!single_with_appends.contains("续做说明 C"));

        // Multi-run chain where r1's fallback became r2's prompt.md, plus r2 has its own append.
        let m2 = json!({"run": "20261127-150000-reply", "parent": "20261127-144457-root"});
        write_json(r2.join("meta.json"), &m2).unwrap();
        write(r2.join("prompt.md"), "续做说明 C：顺便清理注释").unwrap();
        write_json(
            r2.join("appends.json"),
            &json!({
                "closed": true,
                "nextSeq": 2,
                "items": [
                    {
                        "seq": 1,
                        "message": "追加决定 D：保留英文注释",
                        "delivery": "in_run",
                        "deliveredAt": "2026-11-27T15:01:00Z",
                        "cancelledAt": null
                    }
                ]
            }),
        )
        .unwrap();

        let chain_brief = original_task_brief(&r2, &m2);
        assert_eq!(
            chain_brief.matches("续做说明 C：顺便清理注释").count(),
            1,
            "fallback item must not be duplicated: {chain_brief}"
        );
        let p1 = chain_brief.find("原说明 A").unwrap();
        let p2 = chain_brief.find("追加决定 B1").unwrap();
        let p3 = chain_brief.find("追加决定 B2").unwrap();
        let p4 = chain_brief.find("续做说明 C").unwrap();
        let p5 = chain_brief.find("追加决定 D").unwrap();
        assert!(
            p1 < p2 && p2 < p3 && p3 < p4 && p4 < p5,
            "chain chronological order broken: {chain_brief}"
        );
        assert!(chain_brief.contains("续做说明 #1（20261127-150000-reply）"));
        assert!(chain_brief.contains("主控追加 #1（20261127-150000-reply，最新"));

        let _ = fs::remove_dir_all(&root_tmp);
    }

    #[test]
    fn review_model_four_layer_precedence() {
        let base_models = json!({
            "tier-rev": "agy/tier-rev",
            "user-rev": "agy/user-rev",
            "repo-rev": "agy/gemini-4-hydrogen",
            "cli-rev": "agy/cli-rev",
        });
        let base_tiers = json!({"scout": "tier-rev", "review": "tier-rev", "write": "tier-rev"});

        // Layer 4: repo `"review": true`, no user/repo/CLI model -> review tier.
        let (rv4, src4) = crate::config::merge_review(None, Some(&json!(true)));
        let cfg4 = json!({"models": base_models, "tiers": base_tiers, "review": rv4});
        let sel4 = resolve_review_selection(&cfg4, None, src4, &[]).unwrap().unwrap();
        assert_eq!(sel4.model_name, "tier-rev");
        assert_eq!(sel4.model, "agy/tier-rev");
        assert_eq!(sel4.model_source, "tier");

        // Layer 3: user config sets `review.model`, repo sets `"review": true` -> user model wins over tier.
        let user_rev = json!({"enabled": true, "model": "user-rev", "thinking": "high"});
        let (rv3, src3) = crate::config::merge_review(Some(&user_rev), Some(&json!(true)));
        let cfg3 = json!({"models": base_models, "tiers": base_tiers, "review": rv3});
        let sel3 = resolve_review_selection(&cfg3, None, src3, &[]).unwrap().unwrap();
        assert_eq!(sel3.model_name, "user-rev");
        assert_eq!(sel3.model, "agy/user-rev");
        assert_eq!(sel3.thinking.as_deref(), Some("high"));
        assert_eq!(sel3.model_source, "user");
        assert_eq!(sel3.fallback_name.as_deref(), Some("tier-rev"));

        // Layer 2: repo config sets `review.model` -> repo wins over user & tier.
        let repo_rev = json!({"enabled": true, "model": "repo-rev", "thinking": "high"});
        let (rv2, src2) = crate::config::merge_review(Some(&user_rev), Some(&repo_rev));
        let cfg2 = json!({"models": base_models, "tiers": base_tiers, "review": rv2});
        let sel2 = resolve_review_selection(&cfg2, None, src2, &[]).unwrap().unwrap();
        assert_eq!(sel2.model_name, "repo-rev");
        assert_eq!(sel2.model, "agy/gemini-4-hydrogen");
        assert_eq!(sel2.model_source, "repo");
        assert_eq!(sel2.fallback_name.as_deref(), Some("tier-rev"));

        // Layer 1: CLI `--review-model` -> CLI wins over repo, user, and tier.
        let sel1 = resolve_review_selection(&cfg2, Some("cli-rev"), src2, &[])
            .unwrap()
            .unwrap();
        assert_eq!(sel1.model_name, "cli-rev");
        assert_eq!(sel1.model, "agy/cli-rev");
        assert_eq!(sel1.model_source, "cli");
        assert_eq!(sel1.fallback_name.as_deref(), Some("tier-rev"));
    }

    #[test]
    fn review_model_unavailable_classification_and_launch_fallback() {
        // Launch-time fallback when configured review.model is not registered.
        let cfg = json!({
            "models": {"rev-tier": "agy/rev-tier"},
            "tiers": {"scout": "rev-tier", "review": "rev-tier", "write": "rev-tier"},
            "review": {"enabled": true, "model": "missing-model", "thinking": "high"},
        });
        let sel = resolve_review_selection(&cfg, None, Some("repo"), &[])
            .unwrap()
            .unwrap();
        assert_eq!(sel.model_name, "rev-tier");
        assert_eq!(sel.launch_fallback_from.as_deref(), Some("missing-model"));
        assert!(sel.launch_fallback_reason.as_deref().unwrap_or("").contains("missing-model"));

        // Runtime classification: startup failure, quota exhaustion, upstream refusal.
        let tmp = std::env::temp_dir().join(format!(
            "delegate-rev-unavail-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&tmp).unwrap();

        let startup = classify_unavailable_reason(&Err("No such file or directory".into()), &tmp);
        assert!(startup.as_deref().unwrap_or("").starts_with("启动失败"));

        let policy_txt = "The prompt could not be submitted. The prompt contains sensitive words that violate Google's Generative AI Prohibited Use policy.";
        assert_eq!(
            classify_unavailable_reason(&Ok(("ok".into(), policy_txt.into())), &tmp).as_deref(),
            Some("上游拒绝")
        );

        write(tmp.join("stderr.log"), "429 RESOURCE_EXHAUSTED: quota exceeded\n").unwrap();
        assert_eq!(
            classify_unavailable_reason(&Ok(("failed".into(), String::new())), &tmp).as_deref(),
            Some("额度用尽")
        );

        write(tmp.join("stderr.log"), "HTTP 403 Forbidden: upstream rejected\n").unwrap();
        assert_eq!(
            classify_unavailable_reason(&Ok(("failed".into(), String::new())), &tmp).as_deref(),
            Some("上游拒绝")
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn verdict_missing_note_mentions_review_md() {
        assert!(VERDICT_MISSING_NOTE.contains("review.md"));
        assert!(VERDICT_MISSING_NOTE.contains("未从审查开头识别出明确的通过/返工结论"));
    }

    #[test]
    fn extra_review_validation_triggers_quota_guard_and_disagreement() {
        // Validation
        assert!(validate_review_config(Some(&json!({
            "enabled": true,
            "model": "hydrogen",
            "thinking": "high",
            "extra": {
                "model": "sonnet",
                "on": ["controllerReviewPaths", "risk:high"],
                "minQuota": {"fiveHour": 50, "weekly": 30}
            }
        })))
        .is_ok());
        assert!(validate_review_config(Some(&json!({"extra": false}))).is_ok());
        assert!(validate_review_config(Some(&json!({"extra": {"model": "sonnet", "on": []}}))).is_err());
        assert!(validate_review_config(Some(&json!({"extra": {"model": "sonnet", "on": ["unknown"]}}))).is_err());
        assert!(validate_review_config(Some(&json!({"extra": {"model": "sonnet", "on": ["risk:high"], "minQuota": {"fiveHour": 150}}}))).is_err());

        // Triggers: risk:high hits on SQL migration diff, does not hit on ordinary text diff.
        let extra_cfg = json!({
            "enabled": true,
            "model": "agy/claude-sonnet-4",
            "modelName": "sonnet",
            "on": ["controllerReviewPaths", "risk:high"],
            "minQuota": {"fiveHour": 50, "weekly": 30}
        });
        let meta = json!({});
        let tmp_run = std::env::temp_dir().join(format!(
            "delegate-extra-trig-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&tmp_run).unwrap();
        let plain_diff = "diff --git a/notes.txt b/notes.txt\n--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n-old\n+new\n";
        assert!(evaluate_extra_triggers(&extra_cfg, &tmp_run, &meta, plain_diff).is_empty());
        let high_diff = "diff --git a/migrations/001.sql b/migrations/001.sql\nnew file mode 100644\n--- /dev/null\n+++ b/migrations/001.sql\n@@ -0,0 +1 @@\n+ALTER TABLE users ADD COLUMN role TEXT;\n";
        assert_eq!(
            evaluate_extra_triggers(&extra_cfg, &tmp_run, &meta, high_diff),
            vec!["risk:high".to_string()]
        );
        let _ = fs::remove_dir_all(&tmp_run);

        // Quota guard: fiveHour < 50 -> skip, weekly < 30 -> skip, both >= threshold -> allow.
        let mk_cache = |fh: f64, wk: f64| {
            json!({
                "accounts": {
                    "acct1": {
                        "ok": true,
                        "entries": [
                            {"pool": "Gemini Models", "window": "weekly", "remainingPct": 98.0},
                            {"pool": "Claude and GPT models", "window": "fiveHour", "remainingPct": fh},
                            {"pool": "Claude and GPT models", "window": "weekly", "remainingPct": wk},
                        ]
                    }
                }
            })
        };
        assert_eq!(
            check_extra_quota(&extra_cfg, &mk_cache(60.0, 40.0), Some("acct1")),
            ExtraQuotaDecision::Allow
        );
        match check_extra_quota(&extra_cfg, &mk_cache(45.0, 60.0), Some("acct1")) {
            ExtraQuotaDecision::Skip { reason, .. } => {
                assert_eq!(reason, "补充审查跳过：Claude 池剩余 5 小时 45% / 本周 60%");
            }
            other => panic!("expected Skip, got {other:?}"),
        }
        match check_extra_quota(&extra_cfg, &mk_cache(80.0, 20.0), Some("acct1")) {
            ExtraQuotaDecision::Skip { reason, .. } => {
                assert_eq!(reason, "补充审查跳过：Claude 池剩余 5 小时 80% / 本周 20%");
            }
            other => panic!("expected Skip, got {other:?}"),
        }

        // Disagreement check.
        assert!(!extra_verdict_disagrees("PASS", Some("PASS")));
        assert!(extra_verdict_disagrees("PASS", Some("REWORK")));
        assert!(extra_verdict_disagrees("PASS", Some("NEEDS_CONTROLLER")));
        assert!(extra_verdict_disagrees("REWORK", Some("PASS")));
        assert!(!extra_verdict_disagrees("PASS", None));

        // resetAt + 60s buffer & staleness in check_extra_quota_at:
        let reset_str = "2026-11-27T12:00:00Z";
        let reset_ep = iso_epoch(reset_str).unwrap();
        let cache_with_reset = json!({
            "accounts": {
                "acct1": {
                    "ok": true,
                    "fetchedEpoch": reset_ep - 600,
                    "entries": [
                        {"pool": "Claude and GPT models", "window": "fiveHour", "remainingPct": 10.0, "resetAt": reset_str},
                        {"pool": "Claude and GPT models", "window": "weekly", "remainingPct": 80.0, "resetAt": "2026-12-04T00:00:00Z"}
                    ]
                }
            }
        });
        // Within 60s buffer -> still skips!
        assert!(matches!(
            check_extra_quota_at(&extra_cfg, &cache_with_reset, Some("acct1"), reset_ep + 30),
            ExtraQuotaDecision::Skip { .. }
        ));
        // Past resetAt + 60s -> 5h reset (unknown), weekly 80% >= 30% -> allows!
        assert_eq!(
            check_extra_quota_at(&extra_cfg, &cache_with_reset, Some("acct1"), reset_ep + 60),
            ExtraQuotaDecision::Allow
        );
    }

    #[test]
    fn dir_copy_reviewer_diff_shows_only_modified_files() {
        let tmp = std::env::temp_dir().join(format!(
            "delegate-dir-copy-diff-{}-{}",
            std::process::id(),
            now_ns()
        ));
        let _guard = CleanupDir(tmp.clone());
        fs::create_dir_all(tmp.join("v1")).unwrap();
        git(&tmp, &["init", "-q"]).unwrap();
        git(&tmp, &["config", "user.name", "t"]).unwrap();
        git(&tmp, &["config", "user.email", "t@t"]).unwrap();
        for i in 0..4 {
            write(
                tmp.join(format!("v1/f{i}.txt")),
                format!("line1\nline2\nline3\nline4\ncontent {i}\n"),
            )
            .unwrap();
        }
        git(&tmp, &["add", "."]).unwrap();
        git(&tmp, &["commit", "-q", "-m", "init"]).unwrap();
        let t1 = git_text(&tmp, &["rev-parse", "HEAD^{tree}"]).unwrap();

        fs::create_dir_all(tmp.join("v2")).unwrap();
        for i in 0..4 {
            let mut body = format!("line1\nline2\nline3\nline4\ncontent {i}\n");
            if i == 2 {
                body.push_str("only-changed-line\n");
            }
            write(tmp.join(format!("v2/f{i}.txt")), body).unwrap();
        }
        git(&tmp, &["add", "."]).unwrap();
        let t2 = git_text(&tmp, &["write-tree"]).unwrap();

        let (diff, count) = generate_reviewer_diff(&tmp, &t1, &t2, None);
        assert_eq!(count, 1, "expected only 1 modified file in diff:\n{diff}");
        assert!(diff.contains("v2/f2.txt"), "{diff}");
        assert!(diff.contains("+only-changed-line"), "{diff}");
        assert!(!diff.contains("v2/f0.txt"), "identical copy should be omitted:\n{diff}");
        assert!(!diff.contains("v2/f1.txt"), "identical copy should be omitted:\n{diff}");
        assert!(!diff.contains("v2/f3.txt"), "identical copy should be omitted:\n{diff}");
    }

    #[test]
    fn range_diff_is_empty_distinguishes_pure_sync_from_extra_edits() {
        let tmp = std::env::temp_dir().join(format!(
            "delegate-range-diff-{}-{}",
            std::process::id(),
            now_ns()
        ));
        let _guard = CleanupDir(tmp.clone());
        fs::create_dir_all(&tmp).unwrap();
        git(&tmp, &["init", "-q"]).unwrap();
        git(&tmp, &["config", "user.name", "t"]).unwrap();
        git(&tmp, &["config", "user.email", "t@t"]).unwrap();
        write(tmp.join("a.txt"), "a1\n").unwrap();
        write(tmp.join("b.txt"), "b1\n").unwrap();
        git(&tmp, &["add", "."]).unwrap();
        git(&tmp, &["commit", "-q", "-m", "base1"]).unwrap();
        let base1 = git_text(&tmp, &["rev-parse", "HEAD^{tree}"]).unwrap();

        write(tmp.join("a.txt"), "a1\na2\n").unwrap();
        git(&tmp, &["add", "."]).unwrap();
        let head1 = git_text(&tmp, &["write-tree"]).unwrap();

        write(tmp.join("a.txt"), "a1\n").unwrap();
        write(tmp.join("b.txt"), "b1\nb2\n").unwrap();
        git(&tmp, &["add", "."]).unwrap();
        let base2 = git_text(&tmp, &["write-tree"]).unwrap();

        write(tmp.join("a.txt"), "a1\na2\n").unwrap();
        write(tmp.join("b.txt"), "b1\nb2\n").unwrap();
        git(&tmp, &["add", "."]).unwrap();
        let head2_clean = git_text(&tmp, &["write-tree"]).unwrap();

        write(tmp.join("a.txt"), "a1\na2\na3\n").unwrap();
        git(&tmp, &["add", "."]).unwrap();
        let head2_extra = git_text(&tmp, &["write-tree"]).unwrap();

        assert!(range_diff_is_empty(&tmp, &base1, &head1, &base2, &head2_clean));
        assert!(!range_diff_is_empty(&tmp, &base1, &head1, &base2, &head2_extra));
    }

    #[test]
    fn conflict_resolution_detects_non_conflict_line_edits() {
        let before = b"header 1\nheader 2\n<<<<<<< worktree\nours\n=======\ntheirs\n>>>>>>> caller\nfooter 1\nfooter 2\n";
        let resolved_only = b"header 1\nheader 2\nours + theirs\nfooter 1\nfooter 2\n";
        let resolved_with_extra_footer = b"header 1\nheader 2\nours\nfooter 1\nfooter 2 tweaked\n";
        let resolved_with_extra_header = b"header 0\nheader 1\nheader 2\nours\nfooter 1\nfooter 2\n";
        let unresolved = b"header 1\nheader 2\n<<<<<<< worktree\nours\n=======\ntheirs\n>>>>>>> caller\nfooter 1\nfooter 2\n";

        assert!(conflict_resolution_only_touched_conflicts(before, resolved_only));
        assert!(!conflict_resolution_only_touched_conflicts(before, resolved_with_extra_footer));
        assert!(!conflict_resolution_only_touched_conflicts(before, resolved_with_extra_header));
        assert!(!conflict_resolution_only_touched_conflicts(before, unresolved));
    }

    #[test]
    fn load_inherited_review_skips_unreviewed_ancestors_and_preserves_stopped_rework_tree() {
        let tmp = std::env::temp_dir().join(format!(
            "delegate-inherit-review-{}-{}",
            std::process::id(),
            now_ns()
        ));
        let _guard = CleanupDir(tmp.clone());
        let runs_dir = tmp.join("runs");
        fs::create_dir_all(&runs_dir).unwrap();

        let mk_run = |id: &str, parent: Option<&str>, after: &str, rev: Option<Value>| {
            let d = runs_dir.join(id);
            fs::create_dir_all(&d).unwrap();
            let mut m = json!({"run": id, "dir": d, "chainBase": "base0"});
            if let Some(p) = parent {
                m["parent"] = json!(p);
            }
            write_json(d.join("meta.json"), &m).unwrap();
            write_json(d.join("changes.json"), &json!({"after": after})).unwrap();
            if let Some(r) = rev {
                write_json(d.join("review.json"), &r).unwrap();
            }
            (d, m)
        };

        // 1. Unreviewed rubricError / reviewerError / review-off controllerReviewPaths -> returns None.
        let (_, _) = mk_run(
            "r-rubric",
            None,
            "tree-unreviewed-1",
            Some(json!({
                "verdict": "NEEDS_CONTROLLER",
                "upgradedBy": "rubricError",
                "round": 1,
                "rounds": []
            })),
        );
        let (_, _) = mk_run(
            "r-off-cr",
            Some("r-rubric"),
            "tree-unreviewed-2",
            Some(json!({
                "verdict": "NEEDS_CONTROLLER",
                "upgradedBy": "controllerReviewPaths",
                "round": 0,
                "rounds": []
            })),
        );
        let (_, _) = mk_run(
            "r-err",
            Some("r-off-cr"),
            "tree-unreviewed-3",
            Some(json!({
                "verdict": "NEEDS_CONTROLLER",
                "upgradedBy": "reviewerError",
                "round": 1,
                "rounds": [{"round": 1, "verdict": "NEEDS_CONTROLLER"}]
            })),
        );
        let (child1, child1_meta) = mk_run("r-child1", Some("r-err"), "tree-child1", None);
        assert!(
            load_inherited_review(&child1, &child1_meta).is_none(),
            "unreviewed ancestors must not be inherited"
        );

        // 2. Valid PASS ancestor followed by reviewerError ancestor -> skips failed ancestor, inherits PASS tree!
        let (_, _) = mk_run(
            "r-pass",
            None,
            "tree-pass",
            Some(json!({
                "verdict": "PASS",
                "round": 1,
                "reviewedTree": "tree-pass",
                "reviewedBaseTree": "base0",
                "rounds": [{"round": 1, "verdict": "PASS"}]
            })),
        );
        let (_, _) = mk_run(
            "r-err-after-pass",
            Some("r-pass"),
            "tree-unreviewed-after-pass",
            Some(json!({
                "verdict": "NEEDS_CONTROLLER",
                "upgradedBy": "reviewerError",
                "round": 1,
                "rounds": [{"round": 1, "verdict": "NEEDS_CONTROLLER"}]
            })),
        );
        let (child2, child2_meta) = mk_run("r-child2", Some("r-err-after-pass"), "tree-child2", None);
        let inh2 = load_inherited_review(&child2, &child2_meta).expect("should inherit r-pass");
        assert_eq!(inh2.reviewed_tree, "tree-pass");

        // 3. stoppedDuringRework with explicit reviewedTree T1 (changes.after is unreviewed T2) -> inherits T1!
        let (_, _) = mk_run(
            "r-stopped-rework",
            None,
            "tree-T2-unreviewed",
            Some(json!({
                "verdict": "REWORK",
                "round": 1,
                "stoppedDuringRework": true,
                "reviewedTree": "tree-T1-reviewed",
                "reviewedBaseTree": "base0",
                "rounds": [{"round": 1, "verdict": "REWORK"}]
            })),
        );
        let (child3, child3_meta) = mk_run("r-child3", Some("r-stopped-rework"), "tree-T3", None);
        let inh3 = load_inherited_review(&child3, &child3_meta).expect("should inherit T1");
        assert_eq!(inh3.reviewed_tree, "tree-T1-reviewed");

        // 4. Legacy stoppedDuringRework without reviewedTree -> must NOT fall back to unreviewed changes.after!
        let (_, _) = mk_run(
            "r-legacy-stopped",
            None,
            "tree-T2-unreviewed",
            Some(json!({
                "verdict": "REWORK",
                "round": 1,
                "stoppedDuringRework": true,
                "rounds": [{"round": 1, "verdict": "REWORK"}]
            })),
        );
        let (child4, child4_meta) = mk_run("r-child4", Some("r-legacy-stopped"), "tree-T3", None);
        assert!(
            load_inherited_review(&child4, &child4_meta).is_none(),
            "legacy stoppedDuringRework without reviewedTree must not fall back to changes.after"
        );
    }

    #[test]
    fn can_continue_review_session_blocks_agy_across_changed_ro_workspace() {
        let ro1 = Path::new("/tmp/review-ro/run-1");
        let ro2 = Path::new("/tmp/review-ro/run-2");
        assert!(can_continue_review_session(true, true, Some(ro1), ro1));
        assert!(!can_continue_review_session(true, true, Some(ro1), ro2));
        assert!(!can_continue_review_session(true, true, None, ro2));
        assert!(can_continue_review_session(false, false, Some(ro1), ro2));
        assert!(!can_continue_review_session(true, false, Some(ro1), ro1));
        assert!(!can_continue_review_session(false, true, Some(ro1), ro1));

        let note_switched = format_review_workdir_note(true, ro2, Some(ro1), true);
        assert!(note_switched.contains("/tmp/review-ro/run-2"), "{note_switched}");
        assert!(note_switched.contains("新路径"), "{note_switched}");

        let note_fresh = format_review_workdir_note(true, ro2, Some(ro1), false);
        assert!(note_fresh.contains("/tmp/review-ro/run-2"), "{note_fresh}");
    }
}
