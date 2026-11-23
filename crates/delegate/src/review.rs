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
use std::sync::{atomic::AtomicI32, Arc, Mutex};

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

/// Compute live signature validity for `status`, `sign --check`, and 6.30 merger.
pub fn sign_status(run: &Path, meta: &Value, review: &Value) -> Option<Value> {
    let sign_doc = json(run.join("sign.json"));
    let cr = &review["controllerReview"];
    let has_cr = cr.is_object() && !s(cr, "hash").is_empty();
    let has_sign = sign_doc.is_object() && !s(&sign_doc, "hash").is_empty();
    if !has_cr && !has_sign {
        return None;
    }

    let live = evaluate_controller_paths(run, meta, true).ok().flatten();
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
        if let Some(status) = sign_status(&run, &meta, &review_doc) {
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
            Some(ControllerReviewHit {
                paths: strings(&cr["paths"]),
                diff: s(cr, "diff").to_string(),
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

/// Extract the original task brief for this run (or root of its reply chain).
fn original_task_brief(run: &Path, meta: &Value) -> String {
    let mut cur = run.to_path_buf();
    let mut cur_meta = meta.clone();
    let mut visited = BTreeSet::new();
    loop {
        let parent = s(&cur_meta, "parent").to_string();
        if parent.is_empty() || !visited.insert(parent.clone()) {
            break;
        }
        let Some(pdir) = cur.parent().map(|p| p.join(&parent)) else {
            break;
        };
        if !pdir.join("meta.json").is_file() {
            break;
        }
        cur_meta = json(pdir.join("meta.json"));
        cur = pdir;
    }
    let extracted = crate::history::extract_user_prompt(&cur, &cur_meta);
    if !extracted.trim().is_empty() {
        extracted
    } else {
        read(cur.join("prompt.md"))
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

pub(crate) fn dir_manifest(root: &Path) -> Res<BTreeMap<String, String>> {
    fn visit(root: &Path, path: &Path, out: &mut BTreeMap<String, String>) -> Res<()> {
        let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if meta.is_dir() {
            for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
                visit(root, &entry.map_err(|e| e.to_string())?.path(), out)?;
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
        visit(root, root, &mut out)?;
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

fn build_controller_review_json(run: &Path, hit: &ControllerReviewHit) -> Res<Value> {
    let diff_file = run.join("controller-review.diff");
    write(&diff_file, &hit.diff)?;
    Ok(json!({
        "paths": hit.paths,
        "hash": hit.hash,
        "diffFile": diff_file.to_string_lossy(),
        "diff": hit.diff,
    }))
}

/// Entry point called from `supervise::inner` after initial `settle()` (and escalation)
/// and before `finish_run()`.
#[allow(clippy::too_many_arguments)]
pub fn run_if_applicable(
    meta: &mut Value,
    run: &Path,
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

    // Case B: Auto-review is OFF, but controllerReviewPaths is configured and matched.
    if !review_enabled {
        if matches!(state.as_str(), "delivered" | "answered") {
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
                sum["review"] = review_doc;
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
        }
        return Ok(());
    }

    // Case A: Auto-review is ON -> only runs after acceptance passes (`state == "delivered"`).
    if state.as_str() != "delivered" {
        return Ok(());
    }

    let run_id = events::run_id_for(run, meta);
    let repo = events::repo_for_meta(meta);
    let run_name = s(meta, "name").to_string();
    let review_root = run.join("review");
    fs::create_dir_all(&review_root).map_err(|e| e.to_string())?;
    let ro_workspace = review_root.join("ro-workspace");

    let (rubric_text, rubric_source) = match load_rubric(&review_cfg) {
        Ok(pair) => pair,
        Err(err) => {
            let now = iso();
            let review_doc = json!({
                "status": "done",
                "verdict": "NEEDS_CONTROLLER",
                "round": 1,
                "reworkRounds": 0,
                "model": review_cfg["model"],
                "tier": review_cfg["tier"],
                "needsAttention": true,
                "upgradedBy": "rubricError",
                "parseError": err,
                "rounds": [],
                "startedAt": now,
                "updatedAt": now,
                "finishedAt": now,
            });
            write_json(run.join("review.json"), &review_doc)?;
            sum["review"] = review_doc;
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
            sum["review"] = doc;
            break;
        }

        let round_dir = review_root.join(format!("round-{round}"));
        fs::create_dir_all(&round_dir).map_err(|e| e.to_string())?;
        let round_started = iso();
        let incremental = round > 1 && prev_reviewed_tree.is_some();

        // Update review.json so `status` and TUI immediately show "审查中".
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
            }),
        );

        // Compute the diff for this review round.
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

        // Prepare reviewer prompt & fork target.
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
                "作者已完成第 {} 轮返工并通过验收。下面是仅限本轮返工的增量 diff，请基于此前审查结论续审：\n\
                 - 核实上轮指出的问题是否全部真正修复，且未引入新回归或范围外改动。\n\
                 - 第一行必须且只能是 `PASS`、`REWORK` 或 `NEEDS_CONTROLLER`（无前缀、无 Markdown 修饰）。\n\n\
                 ## 本轮增量 diff（第 {round} 轮审查）\n\n\
                 {diff_section}\n",
                round - 1
            )
        } else if incremental {
            // Fallback when previous review session wasn't saved: provide rubric + brief + incremental diff.
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

        // Prepare read-only execution environment & write-detection snapshot.
        let excludes = strings(&meta["snapshotExclude"]);
        let (reviewer_workdir, before_pi_snap, before_agy_manifest) = if is_agy_reviewer
            && top.is_dir()
            && !after_tree.is_empty()
        {
            prepare_ro_workspace(&top, &after_tree, &ro_workspace, &round_dir)?;
            let manifest = dir_manifest(&ro_workspace)?;
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

        let review_res = agents::run_agent(&review_meta, &round_dir, 1, holder.clone(), grace.clone());
        // Clean up any leftover reviewer subprocesses.
        let rev_pgid = read(round_dir.join("agent.pid")).trim().parse().unwrap_or(0);
        crate::cleanup::record(&round_dir, rev_pgid);
        if rev_pgid > 0 && group_members(rev_pgid) {
            end_group(rev_pgid, 1.0);
        }

        // Verify reviewer did not modify any files.
        let ro_violations: Vec<String> = if let Some(before_m) = before_agy_manifest {
            let after_m = dir_manifest(&ro_workspace).unwrap_or_default();
            diff_manifests(&before_m, &after_m)
        } else if let Some(before_s) = before_pi_snap {
            if let Some(after_s) = changes::snapshot(&top, &round_dir, &excludes) {
                if s(&before_s, "tree") != s(&after_s, "tree") {
                    changes::tree_changes(&top, &before_s, &after_s, meta["worktree"].is_object())
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

        if let Some(cont) = find_session_continuation(&round_dir, &review_meta) {
            prev_review_continuation = Some(cont);
        }

        let round_finished = iso();
        let verdict_file = review_root.join(format!("round-{round}-verdict.md"));
        let mut format_error = false;
        let mut parse_error: Option<String> = None;
        let mut upgraded_by: Option<String> = None;
        let mut raw_first_line = String::new();

        let (mut verdict_str, raw_answer) = match review_res {
            Ok((agent_outcome, text)) if agent_outcome == "ok" => {
                let parsed = parse_verdict(&text);
                match parsed {
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
                        raw_first_line = first.clone();
                        format_error = true;
                        parse_error = Some(format!(
                            "invalid reviewer verdict first line: {first:?} (expected PASS, REWORK, or NEEDS_CONTROLLER)"
                        ));
                        upgraded_by = Some("formatError".into());
                        ("NEEDS_CONTROLLER".to_string(), text)
                    }
                }
            }
            Ok((agent_outcome, text)) => {
                if lane::stopped() {
                    *state = "stopped".into();
                    sum["state"] = json!("stopped");
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
                        "updatedAt": round_finished,
                        "finishedAt": round_finished,
                    });
                    write_json(run.join("review.json"), &doc)?;
                    sum["review"] = doc;
                    break;
                }
                parse_error = Some(format!("reviewer ended with outcome: {agent_outcome}"));
                upgraded_by = Some("reviewerError".into());
                ("NEEDS_CONTROLLER".to_string(), text)
            }
            Err(err) => {
                parse_error = Some(format!("reviewer execution error: {err}"));
                upgraded_by = Some("reviewerError".into());
                ("NEEDS_CONTROLLER".to_string(), String::new())
            }
        };

        if !ro_violations.is_empty() {
            parse_error = Some(format!(
                "reviewer violated read-only boundary and modified files: {}",
                ro_violations.join(", ")
            ));
            upgraded_by = Some("readOnlyViolation".into());
            verdict_str = "NEEDS_CONTROLLER".to_string();
        }

        // Write exact raw reviewer verdict file (never modified or paraphrased).
        write(&verdict_file, &raw_answer)?;
        let _ = write(run.join("review.md"), &raw_answer);

        // Check `controllerReviewPaths` on the chain's net changed files:
        // touching any matched path ALWAYS upgrades the verdict to `NEEDS_CONTROLLER`.
        let mut original_verdict: Option<String> = None;
        let mut controller_review_json = Value::Null;
        let mut controller_paths: Vec<String> = Vec::new();
        let mut sign_hash = String::new();
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

        let mut round_entry = json!({
            "round": round,
            "verdict": verdict_str,
            "rawFirstLine": raw_first_line,
            "verdictFile": verdict_file.to_string_lossy(),
            "diffFile": diff_file.to_string_lossy(),
            "incremental": incremental,
            "startedAt": round_started,
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
            round_entry["readOnlyViolation"] = json!(ro_violations);
        }
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
            let final_doc = json!({
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
            write_json(run.join("review.json"), &final_doc)?;
            sum["review"] = final_doc;
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
                final_doc["readOnlyViolation"] = json!(ro_violations);
            }
            if controller_review_json.is_object() {
                final_doc["controllerReview"] = controller_review_json;
            }
            write_json(run.join("review.json"), &final_doc)?;
            sum["review"] = final_doc;
            break;
        }

        // Verdict is `REWORK`: send the raw verdict file verbatim back to the author.
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

        // Remember this round's delivered tree so the next review round diffs ONLY the incremental change.
        prev_reviewed_tree = (!after_tree.is_empty()).then_some(after_tree);

        // Continue author's session with the EXACT review verdict file (zero LLM paraphrasing).
        let Some(author_next) = supervise::continuation_with_file(meta, run, &verdict_file) else {
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
            sum["review"] = final_doc;
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

        supervise::log_event(
            run,
            json!({
                "e": "review_rework",
                "round": round,
                "reworkRounds": rework_rounds,
                "verdictFile": verdict_file.to_string_lossy(),
            }),
        );

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
            write(
                run.join("result.md"),
                format!("{}\n", answer.trim_end_matches('\n')),
            )?;
        }
        let (new_state, mut new_sum, new_rec, new_changed) = supervise::settle(
            meta,
            run,
            &author_verdict,
            &None,
            holder.clone(),
            author_verdict == "ok" && agents::is_question(answer),
        );
        *state = new_state;
        *rec = new_rec;
        *changed = new_changed;
        if state.as_str() != "delivered" {
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
            new_sum["review"] = final_doc;
            *sum = new_sum;
            break;
        }
        *sum = new_sum;
        round += 1;
    }

    if ro_workspace.exists() {
        let _ = fs::remove_dir_all(&ro_workspace);
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
}
