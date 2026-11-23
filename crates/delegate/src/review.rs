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
                    other => {
                        return Err(format!(
                            "review has unknown field {other:?} (allowed: enabled, tier, model, thinking, rubric)"
                        ));
                    }
                }
            }
            Ok(())
        }
        _ => Err(
            "review must be a boolean or {enabled?, tier?, model?, thinking?, rubric?}".into(),
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

/// Resolve reviewer model/agent/rubric at launch time and snapshot into `meta["reviewConfig"]`.
pub fn resolve_launch_config(config_val: &Value, repo: Option<&Path>) -> Res<Option<Value>> {
    let review_val = &config_val["review"];
    if !is_enabled(review_val) {
        return Ok(None);
    }
    let tier = review_val["tier"]
        .as_str()
        .filter(|t| !t.is_empty())
        .unwrap_or("review");
    let explicit_model = review_val["model"].as_str().filter(|m| !m.is_empty());
    let explicit_thinking = review_val["thinking"].as_str().filter(|t| !t.is_empty());
    let listed = agents::pi_models();
    let (model, model_name, thinking) = if let Some(name) = explicit_model {
        let resolved = models::resolve_model(name, config_val, &listed)?;
        let think = models::thinking(explicit_thinking, &json!(name), config_val)?;
        (resolved, name.to_string(), think)
    } else {
        let pick = models::pick(
            config_val,
            tier,
            explicit_thinking,
            &listed,
            agents::local_clock(),
            &|_| 0,
        )?;
        (pick.model, pick.name, pick.thinking)
    };
    let colleague = crate::agy::colleague(&model);
    let (agent_bin, agent_version) = agents::agent_identity(colleague)?;
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
    Ok(Some(json!({
        "enabled": true,
        "tier": tier,
        "model": model,
        "modelName": model_name,
        "thinking": thinking,
        "agent": colleague,
        "agentBin": agent_bin,
        "agentVersion": agent_version,
        "rubric": rubric.map(|p| p.to_string_lossy().into_owned()),
    })))
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

/// Strict controlled verdict parser: the very first line must be exactly
/// `PASS`, `REWORK`, or `NEEDS_CONTROLLER`. Anything else fails closed.
pub fn parse_verdict(answer: &str) -> ParsedVerdict {
    let first = answer.lines().next().unwrap_or("").trim();
    match first {
        "PASS" => ParsedVerdict::Pass,
        "REWORK" => ParsedVerdict::Rework,
        "NEEDS_CONTROLLER" => ParsedVerdict::NeedsController,
        other => ParsedVerdict::Invalid(clip(other, 120)),
    }
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
            x if x.starts_with('-') => return Err(format!("unrecognized arguments: {x}")),
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

/// Extract the complete chronological task brief across the `reply` chain (root -> each reply ->
/// current run, including archived ancestors). Single runs return their brief directly; multi-round
/// reply chains concatenate every round in order and mark the latest round so reviewers judge by
/// the latest decisions and direction changes rather than stale root instructions.
fn original_task_brief(run: &Path, meta: &Value) -> String {
    let mut chain = crate::runs::chain_ancestors_to_run(run);
    if chain.is_empty() {
        chain.push(run.to_path_buf());
    }
    let mut briefs: Vec<(String, String)> = Vec::new();
    for dir in &chain {
        let node_meta = if dir == run && meta.is_object() {
            meta.clone()
        } else {
            json(dir.join("meta.json"))
        };
        let extracted = crate::history::extract_user_prompt(dir, &node_meta);
        let text = if !extracted.trim().is_empty() {
            extracted.trim().to_string()
        } else {
            read(dir.join("prompt.md")).trim().to_string()
        };
        if !text.is_empty() {
            let rid = crate::notes::run_id(dir, &node_meta);
            briefs.push((rid, text));
        }
    }
    if briefs.is_empty() {
        return String::new();
    }
    if briefs.len() == 1 {
        return briefs.remove(0).1;
    }
    let last_idx = briefs.len() - 1;
    let mut sections = Vec::with_capacity(briefs.len());
    for (idx, (rid, text)) in briefs.into_iter().enumerate() {
        let heading = if idx == 0 {
            format!("### 初始说明（{rid}）")
        } else if idx == last_idx {
            format!("### 续做说明 #{idx}（{rid}，最新——以本轮拍板/方向为准）")
        } else {
            format!("### 续做说明 #{idx}（{rid}）")
        };
        sections.push(format!("{heading}\n\n{text}"));
    }
    sections.join("\n\n")
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
        let stat = git_text(top, &["diff", "--stat", "--no-renames", from_tree, to_tree])
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

fn build_reviewer_meta(
    author_meta: &Value,
    review_cfg: &Value,
    review_dir: &Path,
    workdir: &Path,
    prompt_file: &Path,
    fork: Option<&str>,
) -> Value {
    let mut m = json!({
        "run": author_meta["run"],
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
        "timeoutSeconds": author_meta["timeoutSeconds"],
        "top": author_meta["top"],
        "worktree": author_meta["worktree"],
        "agentDeny": [],
    });
    if s(&m, "agent").is_empty() {
        m["agent"] = json!(agents::AGENT);
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
        "round",
        "reworkRounds",
        "model",
        "tier",
        "needsAttention",
        "upgradedBy",
        "originalVerdict",
        "formatError",
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
    parse_error: Option<String>,
    upgraded_by: Option<String>,
    ro_violations: Vec<String>,
    ro_violation_sample: Vec<String>,
    ro_violation_file: Option<PathBuf>,
    verdict_file: PathBuf,
    diff_file: PathBuf,
    round_started: String,
    round_finished: String,
    next_continuation: Option<String>,
    conclusion: Option<String>,
    focus_count: usize,
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
        let accept_doc = crate::accept::build_accept_summary(
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
        emit_accept_event(run, meta, &accept_doc);
        if b(&accept_doc, "infrastructureFailure") {
            sum["infrastructureFailure"] = json!(true);
            sum["error"] = json!("remote infrastructure failure; retry does not spend a rework round");
        }
        *state = if b(&accept_doc, "ok") {
            "delivered"
        } else {
            "rejected"
        }
        .into();
        sum["accept"] = accept_doc;
        sum["state"] = json!(*state);
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
            let accept_doc = crate::accept::build_accept_summary(
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
            emit_accept_event(run, meta, &accept_doc);
            let acc_ok = b(&accept_doc, "ok");
            sum["accept"] = accept_doc;
            *state = if acc_ok { "delivered" } else { "rejected" }.into();
            sum["state"] = json!(*state);
            if !acc_ok {
                return Ok(());
            }
            let now = iso();
            let err_short = clip(&err, MAX_PARSE_ERROR_CHARS);
            let review_doc = json!({
                "status": "done",
                "verdict": "NEEDS_CONTROLLER",
                "round": 1,
                "reworkRounds": 0,
                "model": review_cfg["model"],
                "tier": review_cfg["tier"],
                "needsAttention": true,
                "upgradedBy": "rubricError",
                "parseError": err_short,
                "rounds": [],
                "startedAt": now,
                "updatedAt": now,
                "finishedAt": now,
            });
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
    let review_started_at = iso();
    let mut rounds_history: Vec<Value> = Vec::new();
    let mut round: u64 = 1;
    let mut rework_rounds: u64 = 0;
    let mut prev_reviewed_tree: Option<String> = None;
    let mut prev_review_continuation: Option<String> = None;
    let mut passed_layers: BTreeMap<String, Value> = BTreeMap::new();
    let mut rework_delta_files: Option<Vec<String>> = None;
    let is_agy_reviewer = crate::agy::is_agy(s(&review_cfg, "model"));

    loop {
        if lane::stopped() {
            *state = "stopped".into();
            sum["state"] = json!("stopped");
            let now = iso();
            let doc = json!({
                "status": "done",
                "verdict": Value::Null,
                "round": round,
                "reworkRounds": rework_rounds,
                "model": review_cfg["model"],
                "tier": review_cfg["tier"],
                "rubric": rubric_source,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": now,
                "finishedAt": now,
            });
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
                passed_layers.insert(k.clone(), v.clone());
            }
            if !fo.ok {
                let (_, accept_after_snap) = crate::accept::snapshot_for_accept(meta, run);
                let accept_doc = crate::accept::build_accept_summary(
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
                emit_accept_event(run, meta, &accept_doc);
                if b(&accept_doc, "infrastructureFailure") {
                    sum["infrastructureFailure"] = json!(true);
                    sum["error"] =
                        json!("remote infrastructure failure; retry does not spend a rework round");
                }
                sum["accept"] = accept_doc;
                *state = if lane::stopped() { "stopped" } else { "rejected" }.into();
                sum["state"] = json!(*state);
                if round > 1 {
                    let now = iso();
                    let final_doc = json!({
                        "status": "done",
                        "verdict": "REWORK",
                        "round": round - 1,
                        "reworkRounds": rework_rounds,
                        "model": review_cfg["model"],
                        "tier": review_cfg["tier"],
                        "rubric": rubric_source,
                        "needsAttention": rework_rounds >= NEEDS_ATTENTION_REWORK_ROUND,
                        "stoppedDuringRework": true,
                        "reworkOutcome": state,
                        "rounds": rounds_history,
                        "startedAt": review_started_at,
                        "updatedAt": now,
                        "finishedAt": now,
                    });
                    write_json(run.join("review.json"), &final_doc)?;
                    sum["review"] = compact_review_summary(&final_doc);
                }
                break;
            }
        }

        // Fast layers passed (or none configured). Prepare review round inputs.
        let round_dir = review_root.join(format!("round-{round}"));
        fs::create_dir_all(&round_dir).map_err(|e| e.to_string())?;
        let round_started = iso();
        let incremental = round > 1 && prev_reviewed_tree.is_some();

        let live_review = json!({
            "status": "reviewing",
            "verdict": if rework_rounds > 0 { json!("REWORK") } else { Value::Null },
            "round": round,
            "reworkRounds": rework_rounds,
            "model": review_cfg["model"],
            "tier": review_cfg["tier"],
            "rubric": rubric_source,
            "needsAttention": rework_rounds >= NEEDS_ATTENTION_REWORK_ROUND,
            "rounds": rounds_history,
            "startedAt": review_started_at,
            "updatedAt": round_started,
        });
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
                "model": review_cfg["model"],
                "tier": review_cfg["tier"],
                "parallelWithSlowAccept": !plan.slow.is_empty(),
            }),
        );

        let changes_doc = json(run.join("changes.json"));
        let top = {
            let t = PathBuf::from(s(&changes_doc, "top"));
            if t.is_dir() {
                t
            } else {
                PathBuf::from(s(meta, "top"))
            }
        };
        let after_tree = s(&changes_doc, "after").to_string();
        let (diff_from, diff_label) = if let Some(prev_tree) = prev_reviewed_tree.as_deref() {
            (prev_tree.to_string(), "incremental")
        } else {
            let (total_base, _) = worktree::effective_total_base(&top, meta);
            (total_base, "total")
        };
        let diff_text = if !top.is_dir() || diff_from.is_empty() || after_tree.is_empty() {
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
                    &diff_from,
                    &after_tree,
                ],
            )
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
        };
        let diff_file = round_dir.join(format!("{diff_label}.diff"));
        write(&diff_file, &diff_text)?;

        let fork_target = if incremental {
            match prev_review_continuation.as_deref() {
                Some(prev) => copy_pi_fork_if_needed(prev, &round_dir, is_agy_reviewer).ok(),
                None => None,
            }
        } else {
            None
        };
        let diff_section = format_diff_for_prompt(&diff_text, &diff_file, &top, &diff_from, &after_tree);
        let prompt_body = if fork_target.is_some() {
            format!(
                "作者已完成第 {} 轮返工。下面是仅限本轮返工的增量 diff，请基于此前审查结论续审：\n\
                 - 核实上轮指出的问题是否全部真正修复，且未引入新回归或范围外改动。\n\
                 - 第一行必须且只能是 `PASS`、`REWORK` 或 `NEEDS_CONTROLLER`（无前缀、无 Markdown 修饰）。\n\
                 - 结论正文后附加 `## 主控重点看` 小节（`- [高|中|低] path:line — 理由`，无则写 `无`）。\n\n\
                 ## 本轮增量 diff（第 {round} 轮审查）\n\n\
                 {diff_section}\n",
                round - 1
            )
        } else if incremental {
            format!(
                "请作为独立只读审查员进行第 {round} 轮续审（上轮审查会话不可续接，已提供完整上下文与本轮增量 diff）。\n\
                 输出第一行必须且只能是 `PASS`、`REWORK` 或 `NEEDS_CONTROLLER`。\n\n\
                 ## 审查要点（{rubric_source}）\n\n\
                 {}\n\n\
                 ## 任务说明\n\n\
                 {}\n\n\
                 ## 本轮增量 diff\n\n\
                 {diff_section}\n",
                rubric_text.trim(),
                task_brief.trim(),
            )
        } else {
            format!(
                "请作为独立只读审查员审查以下写入任务的交付结果。\n\
                 严格遵守：不修改任何文件，不委派子任务；输出第一行必须且只能是 `PASS`、`REWORK` 或 `NEEDS_CONTROLLER`（无前缀、无 Markdown 修饰）。\n\n\
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
        let review_prompt_file = round_dir.join("prompt.md");
        write(&review_prompt_file, &prompt_body)?;

        // Prepare isolated read-only workspace whenever running agy OR running in parallel with slow accept.
        let parallel_with_slow = !plan.slow.is_empty();
        let excludes = strings(&meta["snapshotExclude"]);
        let use_ro_workspace = (is_agy_reviewer || parallel_with_slow)
            && top.is_dir()
            && !after_tree.is_empty();
        let (reviewer_workdir, before_pi_snap, before_ro_manifest) = if use_ro_workspace {
            prepare_ro_workspace(&top, &after_tree, &ro_workspace, &round_dir)?;
            let manifest = dir_manifest_for_repo(&ro_workspace, &top)?;
            (ro_workspace.clone(), None, Some(manifest))
        } else {
            let snap = if top.is_dir() {
                changes::snapshot(&top, &round_dir, &excludes)
            } else {
                None
            };
            (PathBuf::from(s(meta, "workdir")), snap, None)
        };

        let review_meta = build_reviewer_meta(
            meta,
            &review_cfg,
            &round_dir,
            &reviewer_workdir,
            &review_prompt_file,
            fork_target.as_deref(),
        );
        crate::deny::prepare(&review_meta, &round_dir)?;

        // Phase L2: run slow acceptance and read-only reviewer in parallel with mutual cancellation.
        let slow_holder = Arc::new(AtomicI32::new(0));
        let rev_holder = Arc::new(AtomicI32::new(0));
        let slow_cancel = Arc::new(AtomicBool::new(false));
        let rev_cancel = Arc::new(AtomicBool::new(false));
        supervise::register_stop_holder(slow_holder.clone());
        supervise::register_stop_holder(rev_holder.clone());

        let append_slow_log = fast_outcome.is_some();
        let (slow_outcome, rev_outcome) = std::thread::scope(|scope| {
            let slow_handle = if parallel_with_slow {
                let slow_holder_t = slow_holder.clone();
                let slow_cancel_t = slow_cancel.clone();
                let rev_cancel_t = rev_cancel.clone();
                let rev_holder_t = rev_holder.clone();
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
                        // Slow acceptance failed -> cancel reviewer immediately (never PASS).
                        rev_cancel_t.store(true, Ordering::SeqCst);
                        let rpid = rev_holder_t.load(Ordering::SeqCst);
                        if rpid > 0 {
                            crate::cleanup::record(round_dir_ref, rpid);
                            kill_group(rpid, 1.0);
                        }
                    }
                    res
                }))
            } else {
                None
            };

            let review_res = agents::run_agent_cancellable(
                &review_meta,
                &round_dir,
                1,
                rev_holder.clone(),
                grace.clone(),
                Some(rev_cancel.clone()),
            );
            let rev_pgid = read(round_dir.join("agent.pid")).trim().parse().unwrap_or(0);
            crate::cleanup::record(&round_dir, rev_pgid);
            if rev_pgid > 0 && group_members(rev_pgid) {
                end_group(rev_pgid, 1.0);
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
                    parse_error: None,
                    upgraded_by: None,
                    ro_violations: Vec::new(),
                    ro_violation_sample: Vec::new(),
                    ro_violation_file: None,
                    verdict_file,
                    diff_file: diff_file.clone(),
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
                    if let Some(after_s) = changes::snapshot(&top, &round_dir, &excludes) {
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
                let next_continuation = find_session_continuation(&round_dir, &review_meta);

                let mut format_error = false;
                let mut parse_error: Option<String> = None;
                let mut upgraded_by: Option<String> = None;
                let mut raw_first_line = String::new();
                let mut stopped_flag = false;

                let (mut verdict_str, raw_answer) = match review_res {
                    Ok((agent_outcome, text)) if agent_outcome == "ok" => {
                        match parse_verdict(&text) {
                            ParsedVerdict::Pass => {
                                raw_first_line = "PASS".into();
                                ("PASS".to_string(), text)
                            }
                            ParsedVerdict::Rework => {
                                raw_first_line = "REWORK".into();
                                ("REWORK".to_string(), text)
                            }
                            ParsedVerdict::NeedsController => {
                                raw_first_line = "NEEDS_CONTROLLER".into();
                                ("NEEDS_CONTROLLER".to_string(), text)
                            }
                            ParsedVerdict::Invalid(first) => {
                                raw_first_line = clip(&first, MAX_PARSE_ERROR_CHARS);
                                format_error = true;
                                parse_error = Some(clip(
                                    &format!(
                                        "invalid reviewer verdict first line: {:?} (expected PASS, REWORK, or NEEDS_CONTROLLER)",
                                        clip(&first, 120)
                                    ),
                                    MAX_PARSE_ERROR_CHARS,
                                ));
                                upgraded_by = Some("formatError".into());
                                ("NEEDS_CONTROLLER".to_string(), text)
                            }
                        }
                    }
                    Ok((agent_outcome, _)) if agent_outcome == "cancelled" => {
                        return (
                            slow_handle.map(|h| h.join().unwrap_or_else(|_| crate::accept::PhaseOutcome::empty_ok())),
                            ReviewRoundOutcome {
                                cancelled: true,
                                stopped: false,
                                verdict_str: String::new(),
                                raw_answer: String::new(),
                                raw_first_line: String::new(),
                                format_error: false,
                                parse_error: None,
                                upgraded_by: None,
                                ro_violations: Vec::new(),
                                ro_violation_sample: Vec::new(),
                                ro_violation_file: None,
                                verdict_file,
                                diff_file: diff_file.clone(),
                                round_started: round_started.clone(),
                                round_finished,
                                next_continuation: None,
                                conclusion: None,
                                focus_count: 0,
                            },
                        );
                    }
                    Ok((agent_outcome, text)) => {
                        if lane::stopped() {
                            stopped_flag = true;
                            (String::new(), text)
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
                        parse_error = Some(clip(
                            &format!("reviewer execution error: {err}"),
                            MAX_PARSE_ERROR_CHARS,
                        ));
                        upgraded_by = Some("reviewerError".into());
                        ("NEEDS_CONTROLLER".to_string(), String::new())
                    }
                };

                let mut ro_violation_file: Option<PathBuf> = None;
                let ro_violation_sample: Vec<String> = ro_violations
                    .iter()
                    .take(RO_VIOLATION_SAMPLE_MAX)
                    .cloned()
                    .collect();
                if !ro_violations.is_empty() && !stopped_flag {
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

                // If reviewer rejected (REWORK) or failed (reviewerError), cancel slow accept immediately!
                if parallel_with_slow
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
                    parse_error,
                    upgraded_by,
                    ro_violations,
                    ro_violation_sample,
                    ro_violation_file,
                    verdict_file,
                    diff_file: diff_file.clone(),
                    round_started: round_started.clone(),
                    round_finished,
                    next_continuation,
                    conclusion,
                    focus_count,
                }
            };

            let slow_res = slow_handle.map(|h| {
                h.join()
                    .unwrap_or_else(|_| crate::accept::PhaseOutcome::empty_ok())
            });
            (slow_res, rev_out)
        });

        supervise::unregister_stop_holder(&slow_holder);
        supervise::unregister_stop_holder(&rev_holder);

        if let Some(so) = &slow_outcome {
            for (k, v) in &so.passed_layers {
                passed_layers.insert(k.clone(), v.clone());
            }
        }
        let (_, accept_after_snap) = crate::accept::snapshot_for_accept(meta, run);
        let accept_doc = crate::accept::build_accept_summary(
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
        emit_accept_event(run, meta, &accept_doc);
        let accept_ok = b(&accept_doc, "ok");
        let accept_cancelled = b(&accept_doc, "cancelled");
        if b(&accept_doc, "infrastructureFailure") {
            sum["infrastructureFailure"] = json!(true);
            sum["error"] =
                json!("remote infrastructure failure; retry does not spend a rework round");
        }
        sum["accept"] = accept_doc;

        // 1. Supervisor stopped externally.
        if lane::stopped() || rev_outcome.stopped {
            *state = "stopped".into();
            sum["state"] = json!("stopped");
            let now = iso();
            let doc = json!({
                "status": "done",
                "verdict": Value::Null,
                "round": round,
                "reworkRounds": rework_rounds,
                "model": review_cfg["model"],
                "tier": review_cfg["tier"],
                "rubric": rubric_source,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": now,
                "finishedAt": now,
            });
            write_json(run.join("review.json"), &doc)?;
            sum["review"] = compact_review_summary(&doc);
            break;
        }

        // 2. Slow acceptance failed (not cancelled by reviewer): NEVER enter delivered,
        //    and a cancelled reviewer MUST NEVER be recorded as PASS.
        if !accept_ok && !accept_cancelled {
            *state = "rejected".into();
            sum["state"] = json!("rejected");
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
                crate::usage::attach_reviewer_round_usage(&round_dir, &mut round_entry);
                rounds_history.push(round_entry);
                let mut final_doc = json!({
                    "status": "cancelled",
                    "verdict": Value::Null,
                    "cancelled": true,
                    "cancelledBy": "acceptFailed",
                    "round": round,
                    "reworkRounds": rework_rounds,
                    "model": review_cfg["model"],
                    "tier": review_cfg["tier"],
                    "rubric": rubric_source,
                    "needsAttention": false,
                    "rounds": rounds_history,
                    "startedAt": review_started_at,
                    "updatedAt": now,
                    "finishedAt": now,
                });
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
                crate::usage::attach_reviewer_round_usage(&round_dir, &mut round_entry);
                rounds_history.push(round_entry);
                let mut final_doc = json!({
                    "status": "done",
                    "verdict": rev_outcome.verdict_str,
                    "acceptFailed": true,
                    "round": round,
                    "reworkRounds": rework_rounds,
                    "model": review_cfg["model"],
                    "tier": review_cfg["tier"],
                    "rubric": rubric_source,
                    "verdictFile": rev_outcome.verdict_file.to_string_lossy(),
                    "needsAttention": false,
                    "rounds": rounds_history,
                    "startedAt": review_started_at,
                    "updatedAt": now,
                    "finishedAt": now,
                });
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
        }

        // Write exact raw reviewer verdict file (never modified or paraphrased).
        write(&rev_outcome.verdict_file, &rev_outcome.raw_answer)?;
        let _ = write(run.join("review.md"), &rev_outcome.raw_answer);

        let mut verdict_str = rev_outcome.verdict_str;
        let mut upgraded_by = rev_outcome.upgraded_by;
        let parse_error = rev_outcome.parse_error;
        let format_error = rev_outcome.format_error;
        let raw_first_line = rev_outcome.raw_first_line;
        let verdict_file = rev_outcome.verdict_file;
        let ro_violations = rev_outcome.ro_violations;
        let ro_violation_sample = rev_outcome.ro_violation_sample;
        let ro_violation_file = rev_outcome.ro_violation_file;
        let round_finished = rev_outcome.round_finished;
        let conclusion = rev_outcome.conclusion;
        let focus_count = rev_outcome.focus_count;

        // Check `controllerReviewPaths` ONLY when NOT entering rework (upgrade PASS / NEEDS_CONTROLLER,
        // never hijack REWORK and skip automatic rework).
        let mut original_verdict: Option<String> = None;
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
                } else if upgraded_by.is_none() {
                    original_verdict = Some("NEEDS_CONTROLLER".to_string());
                }
                upgraded_by = Some("controllerReviewPaths".into());
            }
        }

        let mut round_entry = json!({
            "round": round,
            "verdict": verdict_str,
            "rawFirstLine": raw_first_line,
            "verdictFile": verdict_file.to_string_lossy(),
            "diffFile": rev_outcome.diff_file.to_string_lossy(),
            "incremental": incremental,
            "startedAt": rev_outcome.round_started,
            "finishedAt": round_finished,
        });
        if format_error {
            round_entry["formatError"] = json!(true);
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
        if !ro_violations.is_empty() {
            round_entry["roViolationCount"] = json!(ro_violations.len());
            round_entry["roViolationSample"] = json!(ro_violation_sample);
            if let Some(ref vf) = ro_violation_file {
                round_entry["roViolationFile"] = json!(vf.to_string_lossy());
            }
        }
        crate::usage::attach_reviewer_round_usage(&round_dir, &mut round_entry);
        rounds_history.push(round_entry);

        let mut verdict_ev = json!({
            "name": run_name,
            "round": round,
            "verdict": verdict_str,
            "reworkRounds": rework_rounds,
            "verdictFile": verdict_file.to_string_lossy(),
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
                "round": round,
                "reworkRounds": rework_rounds,
                "model": review_cfg["model"],
                "tier": review_cfg["tier"],
                "rubric": rubric_source,
                "verdictFile": verdict_file.to_string_lossy(),
                "needsAttention": rework_rounds >= NEEDS_ATTENTION_REWORK_ROUND,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": round_finished,
                "finishedAt": round_finished,
            });
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

            let mut final_doc = json!({
                "status": "done",
                "verdict": "NEEDS_CONTROLLER",
                "round": round,
                "reworkRounds": rework_rounds,
                "model": review_cfg["model"],
                "tier": review_cfg["tier"],
                "rubric": rubric_source,
                "verdictFile": verdict_file.to_string_lossy(),
                "needsAttention": true,
                "formatError": format_error,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": round_finished,
                "finishedAt": round_finished,
            });
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
        let reworking_doc = json!({
            "status": "reworking",
            "verdict": "REWORK",
            "round": round,
            "reworkRounds": rework_rounds,
            "model": review_cfg["model"],
            "tier": review_cfg["tier"],
            "rubric": rubric_source,
            "verdictFile": verdict_file.to_string_lossy(),
            "needsAttention": attention_now,
            "rounds": rounds_history,
            "startedAt": review_started_at,
            "updatedAt": iso(),
        });
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
        prev_reviewed_tree = prev_tree_for_delta.clone();

        let Some(mut author_next) = supervise::continuation_with_file(meta, run, &verdict_file) else {
            let err = "author has no saved session to continue for rework";
            let now = iso();
            let final_doc = json!({
                "status": "done",
                "verdict": "NEEDS_CONTROLLER",
                "originalVerdict": "REWORK",
                "upgradedBy": "missingAuthorSession",
                "parseError": err,
                "round": round,
                "reworkRounds": rework_rounds,
                "model": review_cfg["model"],
                "tier": review_cfg["tier"],
                "rubric": rubric_source,
                "verdictFile": verdict_file.to_string_lossy(),
                "needsAttention": true,
                "rounds": rounds_history,
                "startedAt": review_started_at,
                "updatedAt": now,
                "finishedAt": now,
            });
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
        let prev_round_result = run.join(format!("round-{round}-result.md"));
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
        (author_verdict, author_answer, next_attempts) = supervise::resume_stalled(
            &author_next,
            run,
            (author_verdict, author_answer, next_attempts),
            holder.clone(),
            grace.clone(),
        );
        *attempts = next_attempts;
        if !author_answer.trim().is_empty() {
            *answer = author_answer;
            let formatted = format!("{}\n", answer.trim_end_matches('\n'));
            write(run.join(format!("round-{}-result.md", round + 1)), &formatted)?;
            write(run.join("result.md"), &formatted)?;
        }
        let pre = supervise::settle_pre_accept(
            meta,
            run,
            &author_verdict,
            &None,
            holder.clone(),
            author_verdict == "ok" && agents::is_question(answer),
        );
        *state = pre.state;
        *rec = pre.rec;
        *changed = pre.changed;
        let mut new_sum = pre.sum;
        if !pre.can_accept {
            let now = iso();
            let final_doc = json!({
                "status": "done",
                "verdict": "REWORK",
                "round": round,
                "reworkRounds": rework_rounds,
                "model": review_cfg["model"],
                "tier": review_cfg["tier"],
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
    fn parse_verdict_is_strict_on_first_line() {
        assert_eq!(parse_verdict("PASS\nLooks good"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("PASS\r\nDetails"), ParsedVerdict::Pass);
        assert_eq!(parse_verdict("REWORK\n1. Fix foo"), ParsedVerdict::Rework);
        assert_eq!(
            parse_verdict("NEEDS_CONTROLLER\nAmbiguous spec"),
            ParsedVerdict::NeedsController
        );
        assert!(matches!(parse_verdict("**PASS**\n"), ParsedVerdict::Invalid(_)));
        assert!(matches!(parse_verdict("PASS: ok\n"), ParsedVerdict::Invalid(_)));
        assert!(matches!(parse_verdict("\nPASS\n"), ParsedVerdict::Invalid(_)));
        assert!(matches!(parse_verdict(""), ParsedVerdict::Invalid(_)));
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

        let _ = fs::remove_dir_all(&root_tmp);
    }
}
