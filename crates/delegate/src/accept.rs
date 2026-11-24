use crate::changes;
use crate::common::*;
use crate::lane;
use crate::review::glob_match;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Instant;

/// Build configuration and lockfile patterns that force fail-closed full acceptance.
pub const BUILD_CONFIG_PATTERNS: &[&str] = &[
    "Cargo.toml",
    "**/Cargo.toml",
    "Cargo.lock",
    "**/Cargo.lock",
    ".cargo/**",
    "**/.cargo/**",
    "rust-toolchain",
    "rust-toolchain.toml",
    "build.rs",
    "**/build.rs",
    "xtask/**",
    "package.json",
    "**/package.json",
    "pnpm-lock.yaml",
    "**/pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "package-lock.json",
    "**/package-lock.json",
    "yarn.lock",
    "**/yarn.lock",
    "bun.lock",
    "bun.lockb",
    "tsconfig*.json",
    "**/tsconfig*.json",
    "Makefile",
    "GNUmakefile",
    "Justfile",
    "justfile",
    "flake.nix",
    "flake.lock",
    "pyproject.toml",
    "poetry.lock",
    "uv.lock",
    "go.mod",
    "go.sum",
    "CMakeLists.txt",
    ".delegate.json",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptTier {
    Fast,
    Slow,
}

impl AcceptTier {
    pub fn as_str(self) -> &'static str {
        match self {
            AcceptTier::Fast => "fast",
            AcceptTier::Slow => "slow",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AcceptRule {
    pub name: String,
    pub tier: AcceptTier,
    pub paths: Vec<String>,
    pub command: String,
    pub lane: bool,
}

#[derive(Debug, Clone)]
pub struct AcceptLayerSpec {
    pub name: String,
    pub tier: AcceptTier,
    pub command: String,
    pub lane: bool,
    pub matched_files: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct SkippedLayer {
    pub name: String,
    pub tier: AcceptTier,
    pub command: String,
    pub reason: String,
    pub reused: bool,
    pub reused_from_round: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct AcceptPlan {
    pub tiered: bool,
    pub fail_closed: bool,
    pub fail_closed_reason: Option<String>,
    pub fast: Vec<AcceptLayerSpec>,
    pub slow: Vec<AcceptLayerSpec>,
    pub skipped: Vec<SkippedLayer>,
}

pub fn validate_accept_rules(value: Option<&Value>) -> Res<()> {
    let Some(v) = value else { return Ok(()) };
    if v.is_null() {
        return Ok(());
    }
    let arr = v
        .as_array()
        .ok_or("acceptRules must be a non-empty array of rule objects")?;
    if arr.is_empty() {
        return Err("acceptRules must not be empty".into());
    }
    let mut names = BTreeSet::new();
    for (idx, item) in arr.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("acceptRules[{idx}] must be an object"))?;
        for key in obj.keys() {
            if !["name", "paths", "command", "tier", "lane"].contains(&key.as_str()) {
                return Err(format!(
                    "acceptRules[{idx}] has unknown field {key:?} (allowed: name, paths, command, tier, lane)"
                ));
            }
        }
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty() && !s.contains(['\0', '\n', '\r']))
            .ok_or_else(|| format!("acceptRules[{idx}].name must be a non-empty string"))?;
        if !names.insert(name.to_string()) {
            return Err(format!("acceptRules[{idx}].name duplicates {name:?}"));
        }
        let paths = obj
            .get("paths")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| format!("acceptRules[{idx}].paths must be a non-empty list of globs"))?;
        let mut has_positive = false;
        for pat in paths {
            let Some(p) = pat.as_str().filter(|s| !s.trim().is_empty() && !s.contains('\0')) else {
                return Err(format!(
                    "acceptRules[{idx}].paths entries must be non-empty strings without NUL"
                ));
            };
            if let Some(neg) = p.strip_prefix('!') {
                if neg.trim().is_empty() {
                    return Err(format!("acceptRules[{idx}].paths has empty negation pattern"));
                }
            } else {
                has_positive = true;
            }
        }
        if !has_positive {
            return Err(format!(
                "acceptRules[{idx}].paths must contain at least one positive glob"
            ));
        }
        if !obj
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty() && !s.contains('\0'))
        {
            return Err(format!(
                "acceptRules[{idx}].command must be a non-empty command string"
            ));
        }
        if let Some(tier) = obj.get("tier") {
            let t = tier
                .as_str()
                .ok_or_else(|| format!("acceptRules[{idx}].tier must be \"fast\" or \"slow\""))?;
            if !["fast", "slow"].contains(&t) {
                return Err(format!(
                    "acceptRules[{idx}].tier must be \"fast\" or \"slow\", got {t:?}"
                ));
            }
        }
        if let Some(lane_val) = obj.get("lane") {
            if !lane_val.is_boolean() {
                return Err(format!("acceptRules[{idx}].lane must be a boolean"));
            }
        }
    }
    Ok(())
}

pub fn parse_rules(meta: &Value) -> Vec<AcceptRule> {
    meta["acceptRules"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let name = s(item, "name").trim().to_string();
            let command = s(item, "command").trim().to_string();
            let paths = strings(&item["paths"]);
            if name.is_empty() || command.is_empty() || paths.is_empty() {
                return None;
            }
            let tier = match s(item, "tier") {
                "fast" => AcceptTier::Fast,
                _ => AcceptTier::Slow,
            };
            let lane = item
                .get("lane")
                .and_then(Value::as_bool)
                .unwrap_or(tier == AcceptTier::Slow);
            Some(AcceptRule {
                name,
                tier,
                paths,
                command,
                lane,
            })
        })
        .collect()
}

pub fn has_acceptance(meta: &Value) -> bool {
    !s(meta, "accept").trim().is_empty() || !parse_rules(meta).is_empty()
}

pub fn is_build_config_path(path: &str) -> bool {
    let clean = path.trim_start_matches("./");
    BUILD_CONFIG_PATTERNS
        .iter()
        .any(|pat| glob_match(pat, clean))
}

pub fn matches_rule_paths(path: &str, patterns: &[String]) -> bool {
    let clean = path.trim_start_matches("./");
    let mut matched = false;
    for pat in patterns {
        if let Some(neg) = pat.strip_prefix('!') {
            if glob_match(neg, clean) {
                return false;
            }
        } else if glob_match(pat, clean) {
            matched = true;
        }
    }
    matched
}

fn format_path_globs(patterns: &[String]) -> String {
    let shown = patterns.iter().take(4).cloned().collect::<Vec<_>>().join("、");
    if patterns.len() > 4 {
        format!("{shown} 等 {} 条", patterns.len())
    } else {
        shown
    }
}

/// Build the tiered acceptance execution plan for a run (initial round or rework round).
pub fn plan(
    meta: &Value,
    changed_files: &[String],
    changes_known: bool,
    rework_changed_files: Option<&[String]>,
    prev_passed: &BTreeMap<String, Value>,
) -> AcceptPlan {
    let rules = parse_rules(meta);
    let legacy_cmd = s(meta, "accept").trim().to_string();
    let accept_also = strings(&meta["acceptAlso"]);

    if rules.is_empty() {
        if legacy_cmd.is_empty() {
            return AcceptPlan {
                tiered: false,
                fail_closed: false,
                fail_closed_reason: None,
                fast: Vec::new(),
                slow: Vec::new(),
                skipped: Vec::new(),
            };
        }
        return AcceptPlan {
            tiered: false,
            fail_closed: false,
            fail_closed_reason: None,
            fast: Vec::new(),
            slow: vec![AcceptLayerSpec {
                name: "accept".into(),
                tier: AcceptTier::Slow,
                command: legacy_cmd,
                lane: true,
                matched_files: changed_files.to_vec(),
                reason: "默认单条 accept".into(),
            }],
            skipped: Vec::new(),
        };
    }

    // Determine whether fail-closed full fallback is required.
    let mut fail_closed_reason: Option<String> = None;
    if !changes_known {
        fail_closed_reason = Some("无法确定改动文件列表，按 fail-closed 跑全量".into());
    } else if changed_files.is_empty() {
        fail_closed_reason = Some("无改动文件命中任何 acceptRules，按 fail-closed 跑全量".into());
    } else {
        let build_hits: Vec<&String> = changed_files
            .iter()
            .filter(|p| is_build_config_path(p))
            .collect();
        if !build_hits.is_empty() {
            let sample = build_hits
                .iter()
                .take(3)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("、");
            fail_closed_reason = Some(format!("命中构建配置（{sample}），按 fail-closed 跑全量"));
        } else {
            let uncovered: Vec<&String> = changed_files
                .iter()
                .filter(|p| !rules.iter().any(|r| matches_rule_paths(p, &r.paths)))
                .collect();
            if !uncovered.is_empty() {
                let sample = uncovered
                    .iter()
                    .take(3)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("、");
                fail_closed_reason = Some(format!(
                    "改动未命中任何 acceptRules（{sample}），按 fail-closed 跑全量"
                ));
            }
        }
    }

    // Also check if rework delta itself hit a build config or uncovered path.
    if fail_closed_reason.is_none() {
        if let Some(delta) = rework_changed_files {
            if let Some(hit) = delta.iter().find(|p| is_build_config_path(p)) {
                fail_closed_reason = Some(format!("返工命中构建配置（{hit}），按 fail-closed 跑全量"));
            } else if let Some(unc) = delta
                .iter()
                .find(|p| !rules.iter().any(|r| matches_rule_paths(p, &r.paths)))
            {
                fail_closed_reason = Some(format!(
                    "返工改动未命中任何 acceptRules（{unc}），按 fail-closed 跑全量"
                ));
            }
        }
    }

    if let Some(reason) = fail_closed_reason {
        let mut fast = Vec::new();
        let mut slow = Vec::new();
        if !legacy_cmd.is_empty() {
            slow.push(AcceptLayerSpec {
                name: "full".into(),
                tier: AcceptTier::Slow,
                command: legacy_cmd,
                lane: true,
                matched_files: changed_files.to_vec(),
                reason: reason.clone(),
            });
        } else {
            for r in &rules {
                let spec = AcceptLayerSpec {
                    name: r.name.clone(),
                    tier: r.tier,
                    command: r.command.clone(),
                    lane: r.lane,
                    matched_files: changed_files.to_vec(),
                    reason: reason.clone(),
                };
                match r.tier {
                    AcceptTier::Fast => fast.push(spec),
                    AcceptTier::Slow => slow.push(spec),
                }
            }
            for (idx, extra_cmd) in accept_also.iter().enumerate() {
                slow.push(AcceptLayerSpec {
                    name: if accept_also.len() == 1 {
                        "accept-also".into()
                    } else {
                        format!("accept-also-{}", idx + 1)
                    },
                    tier: AcceptTier::Slow,
                    command: extra_cmd.clone(),
                    lane: true,
                    matched_files: changed_files.to_vec(),
                    reason: "任务附加 --accept-also".into(),
                });
            }
        }
        return AcceptPlan {
            tiered: true,
            fail_closed: true,
            fail_closed_reason: Some(reason),
            fast,
            slow,
            skipped: Vec::new(),
        };
    }

    // Normal tiered path matching (with optional rework incremental reuse).
    let mut fast = Vec::new();
    let mut slow = Vec::new();
    let mut skipped = Vec::new();

    for r in &rules {
        let matched: Vec<String> = changed_files
            .iter()
            .filter(|p| matches_rule_paths(p, &r.paths))
            .cloned()
            .collect();
        if matched.is_empty() {
            skipped.push(SkippedLayer {
                name: r.name.clone(),
                tier: r.tier,
                command: r.command.clone(),
                reason: format!("改动未命中 {}", format_path_globs(&r.paths)),
                reused: false,
                reused_from_round: None,
            });
            continue;
        }
        // On rework rounds: if this layer already passed earlier AND none of the rework delta
        // files match this layer, reuse the prior pass without re-running it.
        if let Some(delta) = rework_changed_files {
            let touched_by_rework = delta.iter().any(|p| matches_rule_paths(p, &r.paths));
            if !touched_by_rework {
                if let Some(prev) = prev_passed.get(&r.name) {
                    let prev_round = prev["passedRound"].as_u64().filter(|&rn| rn > 0);
                    let reason = match prev_round {
                        Some(rn) => format!("返工未改动命中路径，沿用第 {rn} 轮通过"),
                        None => "返工未改动命中路径，沿用上轮通过结果".into(),
                    };
                    skipped.push(SkippedLayer {
                        name: r.name.clone(),
                        tier: r.tier,
                        command: r.command.clone(),
                        reason,
                        reused: true,
                        reused_from_round: prev_round,
                    });
                    continue;
                }
            }
        }
        let spec = AcceptLayerSpec {
            name: r.name.clone(),
            tier: r.tier,
            command: r.command.clone(),
            lane: r.lane,
            matched_files: matched,
            reason: format!("命中 {}", format_path_globs(&r.paths)),
        };
        match r.tier {
            AcceptTier::Fast => fast.push(spec),
            AcceptTier::Slow => slow.push(spec),
        }
    }

    // Append `--accept-also` commands (always executed, never skipped by path rules).
    for (idx, extra_cmd) in accept_also.iter().enumerate() {
        slow.push(AcceptLayerSpec {
            name: if accept_also.len() == 1 {
                "accept-also".into()
            } else {
                format!("accept-also-{}", idx + 1)
            },
            tier: AcceptTier::Slow,
            command: extra_cmd.clone(),
            lane: true,
            matched_files: changed_files.to_vec(),
            reason: "任务附加 --accept-also".into(),
        });
    }

    AcceptPlan {
        tiered: true,
        fail_closed: false,
        fail_closed_reason: None,
        fast,
        slow,
        skipped,
    }
}

#[derive(Debug, Clone)]
pub struct PhaseOutcome {
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub not_executed: bool,
    pub infrastructure_failure: bool,
    pub cancelled: bool,
    pub cancelled_by: Option<String>,
    pub local_fallback: Value,
    pub queued_seconds: f64,
    pub layers: Vec<Value>,
    pub passed_layers: BTreeMap<String, Value>,
    pub timed_out: bool,
}

impl PhaseOutcome {
    pub fn empty_ok() -> Self {
        Self {
            ok: true,
            exit_code: Some(0),
            not_executed: false,
            infrastructure_failure: false,
            cancelled: false,
            cancelled_by: None,
            local_fallback: Value::Null,
            queued_seconds: 0.0,
            layers: Vec::new(),
            passed_layers: BTreeMap::new(),
            timed_out: false,
        }
    }
}

fn open_accept_log(run: &Path, append_mode: bool) -> Res<File> {
    let path = run.join("accept.log");
    if append_mode {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())
    } else {
        File::create(&path).map_err(|e| e.to_string())
    }
}

/// Execute a list of acceptance layers (fast or slow phase), supporting lane acquisition and
/// cancellation when running in parallel with the read-only reviewer.
#[allow(clippy::too_many_arguments)]
pub fn run_phase(
    meta: &Value,
    run: &Path,
    specs: &[AcceptLayerSpec],
    append_log: bool,
    holder: &AtomicI32,
    cancel: Option<&AtomicBool>,
    cancel_reason: Option<&str>,
) -> PhaseOutcome {
    if specs.is_empty() {
        return PhaseOutcome::empty_ok();
    }
    if lane::stopped() || cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
        let is_cancel = !lane::stopped() && cancel.is_some_and(|c| c.load(Ordering::SeqCst));
        let layers = specs
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "tier": spec.tier.as_str(),
                    "command": spec.command,
                    "status": if is_cancel { "cancelled" } else { "not_run" },
                    "ok": false,
                    "reason": cancel_reason.unwrap_or("stopped before execution"),
                })
            })
            .collect();
        return PhaseOutcome {
            ok: false,
            exit_code: None,
            not_executed: !is_cancel,
            infrastructure_failure: !is_cancel,
            cancelled: is_cancel,
            cancelled_by: is_cancel.then(|| cancel_reason.unwrap_or("review").to_string()),
            local_fallback: Value::Null,
            queued_seconds: 0.0,
            layers,
            passed_layers: BTreeMap::new(),
            timed_out: false,
        };
    }

    let needs_lane = specs.iter().any(|s| s.lane);
    let slot = if needs_lane {
        let label = format!(
            "accept {}",
            run.file_name().unwrap_or_default().to_string_lossy()
        );
        match lane::acquire_shell_cancellable(&label, None, Some(run), cancel, |ahead, _| {
            crate::supervise::log_event(run, json!({"e":"queue","ahead":ahead}));
        }) {
            Ok(s) => Some(s),
            Err(err) => {
                let is_cancel = !lane::stopped()
                    && cancel.is_some_and(|c| c.load(Ordering::SeqCst));
                let layers = specs
                    .iter()
                    .map(|spec| {
                        json!({
                            "name": spec.name,
                            "tier": spec.tier.as_str(),
                            "command": spec.command,
                            "status": if is_cancel { "cancelled" } else { "not_run" },
                            "ok": false,
                            "reason": if is_cancel {
                                cancel_reason.unwrap_or("cancelled while queued for lane").to_string()
                            } else {
                                err.clone()
                            },
                        })
                    })
                    .collect();
                return PhaseOutcome {
                    ok: false,
                    exit_code: None,
                    not_executed: !is_cancel,
                    infrastructure_failure: !is_cancel,
                    cancelled: is_cancel,
                    cancelled_by: is_cancel.then(|| cancel_reason.unwrap_or("review").to_string()),
                    local_fallback: Value::Null,
                    queued_seconds: 0.0,
                    layers,
                    passed_layers: BTreeMap::new(),
                    timed_out: false,
                };
            }
        }
    } else {
        None
    };

    let queued_seconds = slot.as_ref().map_or(0.0, |s| s.queued);

    let mut log = match open_accept_log(run, append_log) {
        Ok(f) => f,
        Err(e) => {
            let layers = specs
                .iter()
                .map(|spec| {
                    json!({
                        "name": spec.name,
                        "tier": spec.tier.as_str(),
                        "command": spec.command,
                        "status": "failed",
                        "ok": false,
                        "reason": e,
                    })
                })
                .collect();
            return PhaseOutcome {
                ok: false,
                exit_code: Some(1),
                not_executed: true,
                infrastructure_failure: true,
                cancelled: false,
                cancelled_by: None,
                local_fallback: Value::Null,
                queued_seconds,
                layers,
                passed_layers: BTreeMap::new(),
                timed_out: false,
            };
        }
    };

    let accept_timeout = meta["acceptTimeoutSeconds"].as_f64().filter(|t| *t > 0.0);
    let mut layers = Vec::new();
    let mut passed_layers = BTreeMap::new();
    let mut local_fallback = Value::Null;
    let mut overall_ok = true;
    let mut overall_exit = Some(0);
    let mut any_timed = false;
    let mut any_infra = false;
    let mut any_not_exec = false;
    let mut was_cancelled = false;

    for (idx, spec) in specs.iter().enumerate() {
        if lane::stopped() || cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            let is_cancel = !lane::stopped() && cancel.is_some_and(|c| c.load(Ordering::SeqCst));
            was_cancelled = is_cancel;
            overall_ok = false;
            for rem in &specs[idx..] {
                layers.push(json!({
                    "name": rem.name,
                    "tier": rem.tier.as_str(),
                    "command": rem.command,
                    "status": if is_cancel { "cancelled" } else { "not_run" },
                    "ok": false,
                    "reason": cancel_reason.unwrap_or("stopped"),
                }));
            }
            break;
        }
        let _ = writeln!(
            log,
            "=== [{}] {} ({}) ===\n$ {}",
            spec.tier.as_str(),
            spec.name,
            spec.reason,
            spec.command
        );
        let _ = log.flush();
        let started = Instant::now();
        let (mut code, mut timed) = run_shell_cancellable(
            (&spec.command, "accept"),
            Path::new(s(meta, "workdir")),
            run,
            &meta["env"],
            accept_timeout,
            &mut log,
            Some(holder),
            false,
            cancel,
        )
        .unwrap_or((1, false));

        if code == 75 && !cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            let remote_status = json(run.join("accept.remote.json"));
            if s(&remote_status, "stage") == "missing_managed" {
                let reason = s(&remote_status, "reason").to_string();
                let paths = remote_status["paths"].clone();
                let _ = writeln!(
                    log,
                    "\n[remote missing managed paths: {reason}; rerunning acceptance locally]"
                );
                let _ = log.flush();
                (code, timed) = run_shell_cancellable(
                    (&spec.command, "accept"),
                    Path::new(s(meta, "workdir")),
                    run,
                    &meta["env"],
                    accept_timeout,
                    &mut log,
                    Some(holder),
                    true,
                    cancel,
                )
                .unwrap_or((1, false));
                local_fallback = json!({"reason": reason, "paths": paths});
            }
        }
        let elapsed = (started.elapsed().as_secs_f64() * 100.0).round() / 100.0;
        let cancelled_now = !lane::stopped() && cancel.is_some_and(|c| c.load(Ordering::SeqCst));
        let _ = writeln!(
            log,
            "{}\n[exit {code}]",
            if cancelled_now {
                "\n[accept cancelled]"
            } else if timed {
                "\n[accept timed out]"
            } else {
                ""
            }
        );
        let _ = log.flush();

        if cancelled_now {
            was_cancelled = true;
            overall_ok = false;
            overall_exit = Some(code);
            layers.push(json!({
                "name": spec.name,
                "tier": spec.tier.as_str(),
                "command": spec.command,
                "status": "cancelled",
                "ok": false,
                "exitCode": code,
                "seconds": elapsed,
                "reason": cancel_reason.unwrap_or("cancelled by parallel review"),
            }));
            for rem in &specs[idx + 1..] {
                layers.push(json!({
                    "name": rem.name,
                    "tier": rem.tier.as_str(),
                    "command": rem.command,
                    "status": "cancelled",
                    "ok": false,
                    "reason": cancel_reason.unwrap_or("cancelled by parallel review"),
                }));
            }
            break;
        }

        let outcome_meta = shell_outcome(run, "accept", code, timed);
        let layer_ok = code == 0;
        let mut layer_entry = json!({
            "name": spec.name,
            "tier": spec.tier.as_str(),
            "command": spec.command,
            "status": if layer_ok { "passed" } else { "failed" },
            "ok": layer_ok,
            "exitCode": code,
            "seconds": elapsed,
            "reason": spec.reason,
        });
        if !spec.matched_files.is_empty() {
            layer_entry["matchedCount"] = json!(spec.matched_files.len());
            layer_entry["matchedSample"] = json!(spec.matched_files.iter().take(5).cloned().collect::<Vec<_>>());
        }
        if timed {
            layer_entry["timedOut"] = json!(true);
        }
        layers.push(layer_entry.clone());

        if layer_ok {
            passed_layers.insert(spec.name.clone(), layer_entry);
        } else {
            overall_ok = false;
            overall_exit = Some(code);
            any_timed |= timed;
            any_infra |= b(&outcome_meta, "infrastructureFailure");
            any_not_exec |= b(&outcome_meta, "notExecuted");
            for rem in &specs[idx + 1..] {
                layers.push(json!({
                    "name": rem.name,
                    "tier": rem.tier.as_str(),
                    "command": rem.command,
                    "status": "not_run",
                    "ok": false,
                    "reason": format!("前序层 {} 失败，未执行", spec.name),
                }));
            }
            break;
        }
    }

    drop(slot);
    PhaseOutcome {
        ok: overall_ok,
        exit_code: overall_exit,
        not_executed: any_not_exec,
        infrastructure_failure: any_infra,
        cancelled: was_cancelled,
        cancelled_by: was_cancelled.then(|| cancel_reason.unwrap_or("review").to_string()),
        local_fallback,
        queued_seconds,
        layers,
        passed_layers,
        timed_out: any_timed,
    }
}

/// Snapshot tree before/after acceptance execution (excluding managed paths).
pub fn snapshot_for_accept(meta: &Value, run: &Path) -> (Vec<String>, Option<Value>) {
    let excluded = changes::managed_paths(meta);
    let snap = changes::repository_snapshot_excluding(Path::new(s(meta, "top")), run, &excluded);
    (excluded, snap)
}

/// Assemble the backward-compatible `summary.json["accept"]` object from the plan and phase results.
#[allow(clippy::too_many_arguments)]
pub fn build_accept_summary(
    meta: &Value,
    run: &Path,
    plan: &AcceptPlan,
    fast_outcome: Option<&PhaseOutcome>,
    slow_outcome: Option<&PhaseOutcome>,
    parallel_with_review: bool,
    excluded: Vec<String>,
    before_snap: Option<&Value>,
    after_snap: Option<&Value>,
) -> Value {
    let mut all_layers: Vec<Value> = Vec::new();
    let mut ran_layers: Vec<String> = Vec::new();
    let mut skipped_layers: Vec<Value> = Vec::new();
    let mut reused_layers: Vec<String> = Vec::new();

    if let Some(fo) = fast_outcome {
        for l in &fo.layers {
            let st = s(l, "status");
            if matches!(st, "passed" | "failed" | "cancelled") {
                ran_layers.push(s(l, "name").to_string());
            }
            all_layers.push(l.clone());
        }
    }
    if let Some(so) = slow_outcome {
        for l in &so.layers {
            let st = s(l, "status");
            if matches!(st, "passed" | "failed" | "cancelled") {
                ran_layers.push(s(l, "name").to_string());
            }
            all_layers.push(l.clone());
        }
    } else if !plan.slow.is_empty() && fast_outcome.is_some_and(|f| !f.ok) {
        for spec in &plan.slow {
            all_layers.push(json!({
                "name": spec.name,
                "tier": spec.tier.as_str(),
                "command": spec.command,
                "status": "not_run",
                "ok": false,
                "reason": "快层验收未通过，未执行慢层",
            }));
        }
    }

    for sk in &plan.skipped {
        let status = if sk.reused { "reused" } else { "skipped" };
        let mut layer_obj = json!({
            "name": sk.name,
            "tier": sk.tier.as_str(),
            "command": sk.command,
            "status": status,
            "ok": true,
            "reused": sk.reused,
            "reason": sk.reason,
        });
        let mut skip_obj = json!({
            "name": sk.name,
            "tier": sk.tier.as_str(),
            "reused": sk.reused,
            "reason": sk.reason,
        });
        if let Some(rn) = sk.reused_from_round {
            layer_obj["reusedFromRound"] = json!(rn);
            skip_obj["reusedFromRound"] = json!(rn);
        }
        all_layers.push(layer_obj);
        skipped_layers.push(skip_obj);
        if sk.reused {
            reused_layers.push(sk.name.clone());
        }
    }

    // Write skipped/reused summary lines into accept.log for full auditability.
    if !plan.skipped.is_empty() {
        if let Ok(mut log) = open_accept_log(run, true) {
            for sk in &plan.skipped {
                let tag = if sk.reused { "reused" } else { "skipped" };
                let _ = writeln!(log, "[{tag}] {}: {}", sk.name, sk.reason);
            }
        }
    }

    let fast_ok = fast_outcome.is_none_or(|f| f.ok);
    let slow_ok = if fast_ok {
        slow_outcome.is_none_or(|s| s.ok)
    } else {
        false
    };
    let ok = fast_ok && slow_ok;

    let failed_phase = fast_outcome
        .filter(|f| !f.ok)
        .or_else(|| slow_outcome.filter(|s| !s.ok));
    let exit_code = failed_phase
        .and_then(|p| p.exit_code)
        .or(Some(if ok { 0 } else { 1 }));
    let not_executed = failed_phase.is_some_and(|p| p.not_executed);
    let infrastructure_failure = failed_phase.is_some_and(|p| p.infrastructure_failure);
    let timed_out = failed_phase.is_some_and(|p| p.timed_out);
    let cancelled = failed_phase.is_some_and(|p| p.cancelled);
    let cancelled_by = failed_phase.and_then(|p| p.cancelled_by.clone());

    let command_summary = {
        let ran_cmds: Vec<String> = all_layers
            .iter()
            .filter(|l| matches!(s(l, "status"), "passed" | "failed" | "cancelled"))
            .map(|l| s(l, "command").to_string())
            .filter(|c| !c.is_empty())
            .collect();
        if !ran_cmds.is_empty() {
            ran_cmds.join(" && ")
        } else if !s(meta, "accept").is_empty() {
            s(meta, "accept").to_string()
        } else {
            plan.skipped
                .iter()
                .map(|sk| format!("[{}] {}", if sk.reused { "reused" } else { "skipped" }, sk.name))
                .collect::<Vec<_>>()
                .join(", ")
        }
    };

    let mut result = json!({
        "command": command_summary,
        "ok": ok,
        "exitCode": exit_code,
        "notExecuted": not_executed,
        "infrastructureFailure": infrastructure_failure,
        "timedOut": timed_out,
    });

    if plan.tiered {
        result["tiered"] = json!(true);
        result["failClosed"] = json!(plan.fail_closed);
        if let Some(reason) = &plan.fail_closed_reason {
            result["failClosedReason"] = json!(reason);
        }
        result["layers"] = json!(all_layers);
        result["ranLayers"] = json!(ran_layers);
        result["skippedLayers"] = json!(skipped_layers);
        if !reused_layers.is_empty() {
            result["reusedLayers"] = json!(reused_layers);
        }
    }
    if parallel_with_review {
        result["parallelWithReview"] = json!(true);
    }
    if cancelled {
        result["cancelled"] = json!(true);
        if let Some(by) = cancelled_by {
            result["cancelledBy"] = json!(by);
        }
    }

    let mut elapsed_total = 0.0_f64;
    let mut layer_seconds = serde_json::Map::new();
    for l in &all_layers {
        if let Some(secs) = l["seconds"].as_f64() {
            elapsed_total += secs;
            let lname = s(l, "name");
            if !lname.is_empty() {
                layer_seconds.insert(
                    lname.to_string(),
                    json!((secs * 100.0).round() / 100.0),
                );
            }
        }
    }
    result["elapsedSeconds"] = json!((elapsed_total * 100.0).round() / 100.0);
    if !layer_seconds.is_empty() {
        result["layerSeconds"] = Value::Object(layer_seconds);
    }

    let queued = fast_outcome.map_or(0.0, |f| f.queued_seconds)
        + slow_outcome.map_or(0.0, |s| s.queued_seconds);
    if queued >= 0.01 {
        result["queuedSeconds"] = json!((queued * 100.0).round() / 100.0);
    }

    let fallback = slow_outcome
        .filter(|s| s.local_fallback.is_object())
        .map(|s| s.local_fallback.clone())
        .or_else(|| {
            fast_outcome
                .filter(|f| f.local_fallback.is_object())
                .map(|f| f.local_fallback.clone())
        });
    if let Some(fb) = fallback {
        result["localFallback"] = fb;
    }

    if !excluded.is_empty() {
        result["excluded"] = json!(excluded);
    }
    if let (Some(before), Some(after)) = (before_snap, after_snap) {
        let mut exc_set: BTreeSet<String> = excluded.into_iter().collect();
        for ev in [before, after] {
            exc_set.extend(strings(&ev["excluded"]));
        }
        if !exc_set.is_empty() {
            result["excluded"] = json!(exc_set);
        }
        result["tree"] = before["tree"].clone();
        result["treeAfter"] = after["tree"].clone();
        result["snapshotComplete"] = json!(b(before, "complete") && b(after, "complete"));
        if !b(&result, "snapshotComplete") {
            result["snapshotReason"] = before
                .get("incompleteReason")
                .or_else(|| after.get("incompleteReason"))
                .cloned()
                .unwrap_or(json!("incomplete repository snapshot"));
        }
    } else if !s(meta, "top").is_empty() {
        result["snapshotReason"] = json!("acceptance repository snapshot failed");
    }

    if !ok {
        let text = read(run.join("accept.log"));
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            let tail: String = trimmed
                .chars()
                .rev()
                .take(1500)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            result["tail"] = json!(tail);
        } else if not_executed {
            result["tail"] = json!("stopped while queued for the heavy lane");
        }
        if !cancelled && !not_executed && !infrastructure_failure {
            let changed = changed_paths_from_run(run);
            if let Some(attr) = analyze_failure_attribution(run, meta, &changed) {
                result["failureAttribution"] = attr;
            }
        }
    }

    result
}

const KNOWN_SOURCE_EXTS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "css", "scss", "less", "html", "vue", "svelte",
    "py", "go", "sql", "toml", "json", "yaml", "yml", "md", "sh", "bash", "c", "cc", "cpp", "h",
    "hpp", "java", "kt", "swift", "rb", "php", "proto", "graphql", "prisma", "wgsl", "glsl",
];

fn has_known_source_ext(path: &str) -> bool {
    let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) else {
        return false;
    };
    KNOWN_SOURCE_EXTS.contains(&ext.to_ascii_lowercase().as_str())
}

fn changed_paths_from_run(run: &Path) -> Vec<String> {
    json(run.join("changes.json"))["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| s(c, "path").to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn collect_repo_roots(meta: &Value) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for raw in [
        s(&meta["worktree"], "path"),
        s(&meta["worktree"], "source"),
        s(meta, "top"),
        s(meta, "workdir"),
    ] {
        if raw.is_empty() {
            continue;
        }
        let p = PathBuf::from(raw);
        if !roots.contains(&p) {
            roots.push(p.clone());
        }
        if let Ok(canon) = fs::canonicalize(&p) {
            if !roots.contains(&canon) {
                roots.push(canon);
            }
        }
    }
    roots
}

fn normalize_repo_path(
    raw: &str,
    roots: &[PathBuf],
    any_root_exists: bool,
    changed_set: &BTreeSet<String>,
) -> Option<String> {
    let mut tok = raw
        .trim()
        .trim_matches(|c: char| matches!(c, '\'' | '"' | '`' | '(' | ')' | '[' | ']' | ',' | ';' | ':'));
    if tok.is_empty()
        || tok.contains("://")
        || tok.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let rel = if tok.starts_with('/') {
        let p = Path::new(tok);
        let mut matched = None;
        for root in roots {
            if let Ok(stripped) = p.strip_prefix(root) {
                let s = stripped.to_string_lossy().into_owned();
                if !s.is_empty() {
                    matched = Some(s);
                    break;
                }
            }
        }
        matched?
    } else {
        while let Some(rest) = tok.strip_prefix("./") {
            tok = rest;
        }
        tok.to_string()
    };
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return None;
    }
    if rel.starts_with(".local/")
        || rel.starts_with(".git/")
        || rel.starts_with("node_modules/")
        || rel.starts_with("target/")
        || rel.contains("/node_modules/")
        || rel.contains("/target/")
    {
        return None;
    }
    if any_root_exists {
        if changed_set.contains(&rel) || roots.iter().any(|r| r.join(&rel).is_file()) {
            return Some(rel);
        }
        return None;
    }
    if changed_set.contains(&rel) || (rel.contains('/') && has_known_source_ext(&rel)) {
        return Some(rel);
    }
    None
}

fn extract_path_before_line_col(tok: &str) -> Option<&str> {
    // Handles `path:42`, `path:42:10`, `path:42:10:`, and `path(42,10)`.
    let cleaned = tok.trim_matches(|c: char| matches!(c, '\'' | '"' | '`' | '[' | ']' | ',' | ';'));
    if let Some(open) = cleaned.rfind('(') {
        if let Some(inside) = cleaned[open + 1..].strip_suffix(')') {
            let mut parts = inside.split(',');
            if let (Some(l), Some(c), None) = (parts.next(), parts.next(), parts.next()) {
                if !l.is_empty()
                    && l.bytes().all(|b| b.is_ascii_digit())
                    && !c.is_empty()
                    && c.bytes().all(|b| b.is_ascii_digit())
                {
                    return Some(&cleaned[..open]);
                }
            }
        }
    }
    let trimmed = cleaned.trim_end_matches(':');
    let mut parts = trimmed.rsplitn(3, ':');
    let last = parts.next()?;
    if last.is_empty() || !last.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let prev = parts.next()?;
    if prev.bytes().all(|b| b.is_ascii_digit()) && !prev.is_empty() {
        parts.next()
    } else {
        Some(&trimmed[..trimmed.len() - last.len() - 1])
    }
}

fn is_failure_marker_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    line.contains("FAIL")
        || line.contains("ERROR")
        || lower.contains("error:")
        || lower.contains("error[")
        || lower.contains("panicked at")
        || lower.contains("exceeds")
        || lower.contains("size-audit")
        || lower.contains("violation")
        || lower.contains("assertionerror")
        || lower.contains("syntaxerror")
        || lower.contains("typeerror")
        || line.contains('✗')
        || line.contains('×')
}

fn is_warning_header(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("warning:") || lower.starts_with("warning[")
}

fn is_error_or_section_header(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("error:")
        || lower.starts_with("error[")
        || trimmed.starts_with("===")
        || trimmed.starts_with("---")
        || trimmed.starts_with("FAIL")
        || trimmed.starts_with("FAILED")
        || lower.starts_with("thread '")
}

/// Cross-reference error file paths extracted from `accept.log` against this run's changed files
/// to flag failures whose reported files are completely outside this run's changes (suspected
/// pre-existing mainline failure).
pub fn analyze_failure_attribution(
    run: &Path,
    meta: &Value,
    changed_paths: &[String],
) -> Option<Value> {
    let log_path = run.join("accept.log");
    if !log_path.is_file() {
        return None;
    }
    let log_text = read(&log_path);
    if log_text.trim().is_empty() {
        return None;
    }

    let roots = collect_repo_roots(meta);
    let any_root_exists = roots.iter().any(|r| r.is_dir());
    let changed_set: BTreeSet<String> = changed_paths
        .iter()
        .map(|p| p.trim_start_matches("./").to_string())
        .filter(|p| !p.is_empty())
        .collect();

    let lines: Vec<&str> = log_text.lines().collect();
    let mut seen: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut in_warning_block = false;

    let mut record_path = |path: String, line_no: usize, raw_line: &str| {
        seen.entry(path.clone()).or_insert_with(|| {
            order.push(path);
            (line_no, clip(raw_line.trim(), 160))
        });
    };

    for (idx, &raw_line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("$ ")
            || trimmed.starts_with("[exit ")
            || trimmed.starts_with("[skipped]")
            || trimmed.starts_with("[reused]")
        {
            continue;
        }
        if is_warning_header(trimmed) {
            in_warning_block = true;
            continue;
        }
        if is_error_or_section_header(trimmed) {
            in_warning_block = false;
        }
        if in_warning_block {
            continue;
        }

        // 1) Rust/Cargo/Clippy arrow diagnostic (`--> path:line:col` / `::: path:line:col`).
        for prefix in ["--> ", "::: "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let first = rest.split_whitespace().next().unwrap_or("");
                let cand = extract_path_before_line_col(first).unwrap_or(first);
                if let Some(p) = normalize_repo_path(cand, &roots, any_root_exists, &changed_set) {
                    record_path(p, line_no, raw_line);
                }
            }
        }

        // 2) Python traceback (`File "path", line N`).
        if let Some(pos) = trimmed.find("File \"") {
            let after = &trimmed[pos + 6..];
            if let Some(end) = after.find('"') {
                if let Some(p) =
                    normalize_repo_path(&after[..end], &roots, any_root_exists, &changed_set)
                {
                    record_path(p, line_no, raw_line);
                }
            }
        }

        // 3) ESLint / Stylelint standalone file header followed within 5 lines by an `error` diagnostic.
        if !trimmed.contains(' ') {
            if let Some(p) = normalize_repo_path(trimmed, &roots, any_root_exists, &changed_set) {
                let has_following_error = lines[idx + 1..(idx + 6).min(lines.len())].iter().any(|next| {
                    let nt = next.trim();
                    let nl = nt.to_ascii_lowercase();
                    !nt.is_empty()
                        && (nl.contains(" error ") || nl.ends_with(" error") || nl.starts_with("error"))
                });
                if has_following_error {
                    record_path(p, line_no, raw_line);
                }
            }
        }

        // 4) Generic `path:line(:col)` or `path(line,col)` tokens on non-warning lines.
        let lower_line = trimmed.to_ascii_lowercase();
        let line_is_warning_only =
            lower_line.contains(" warning ") && !lower_line.contains("error") && !trimmed.contains("FAIL");
        if !line_is_warning_only {
            for tok in trimmed.split_whitespace() {
                if let Some(cand) = extract_path_before_line_col(tok) {
                    if let Some(p) =
                        normalize_repo_path(cand, &roots, any_root_exists, &changed_set)
                    {
                        record_path(p, line_no, raw_line);
                    }
                }
            }
        }

        // 5) Explicit failure marker lines (`FAIL:`, `ERROR`, `size-audit`, `panicked at`, etc.).
        if is_failure_marker_line(trimmed) {
            for tok in trimmed.split(|c: char| {
                c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | ',' | ';' | '\'' | '"')
            }) {
                if tok.is_empty() {
                    continue;
                }
                let cand = extract_path_before_line_col(tok).unwrap_or(tok);
                if let Some(p) = normalize_repo_path(cand, &roots, any_root_exists, &changed_set) {
                    record_path(p, line_no, raw_line);
                }
            }
        }
    }

    if order.is_empty() {
        return None;
    }

    let mut in_run = Vec::new();
    let mut outside_run = Vec::new();
    for path in order {
        let (log_line, excerpt) = &seen[&path];
        let item = json!({
            "path": path,
            "logLine": log_line,
            "excerpt": excerpt,
        });
        if changed_set.contains(&path) {
            in_run.push(item);
        } else {
            outside_run.push(item);
        }
    }

    let suspected_mainline = !outside_run.is_empty() && in_run.is_empty();
    Some(json!({
        "suspectedMainline": suspected_mainline,
        "logFile": log_path.to_string_lossy(),
        "inRunFiles": in_run,
        "outsideRunFiles": outside_run,
        "changedFileCount": changed_set.len(),
    }))
}

/// True when an acceptance failure is attributed purely to files outside this run's changes
/// (`inRunFiles` empty and `outsideRunFiles` non-empty) and is not a cancellation or infra error.
pub fn is_mainline_blocked(accept_doc: &Value) -> bool {
    !b(accept_doc, "ok")
        && !b(accept_doc, "cancelled")
        && !b(accept_doc, "notExecuted")
        && !b(accept_doc, "infrastructureFailure")
        && accept_doc["failureAttribution"].is_object()
        && b(&accept_doc["failureAttribution"], "suspectedMainline")
        && accept_doc["failureAttribution"]["inRunFiles"]
            .as_array()
            .is_some_and(Vec::is_empty)
        && accept_doc["failureAttribution"]["outsideRunFiles"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
}

/// Compute exact git blob SHA-1 for a file under `root` (`"absent"` if missing).
pub fn git_blob_hash_file(root: &Path, rel: &str) -> String {
    let path = root.join(rel);
    let Ok(meta) = fs::symlink_metadata(&path) else {
        return "absent".into();
    };
    let bytes = if meta.file_type().is_symlink() {
        use std::os::unix::ffi::OsStrExt;
        match fs::read_link(&path) {
            Ok(target) => target.as_os_str().as_bytes().to_vec(),
            Err(_) => return "absent".into(),
        }
    } else if meta.is_file() {
        match fs::read(&path) {
            Ok(data) => data,
            Err(_) => return "absent".into(),
        }
    } else {
        return "absent".into();
    };
    let header = format!("blob {}\0", bytes.len());
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(&bytes);
    hasher.digest().to_string()
}

/// Build the `mainlineBlock` record persisted in `summary.json` when acceptance enters
/// `blocked_mainline`. Captures blocking files, their git blob hashes in the worktree at failure
/// time, the worktree's base HEAD, and which acceptance layers failed / were not run.
pub fn build_mainline_block(run: &Path, meta: &Value, accept_doc: &Value) -> Value {
    let attr = &accept_doc["failureAttribution"];
    let blocking_files: Vec<String> = attr["outsideRunFiles"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| s(item, "path").to_string())
        .filter(|p| !p.is_empty())
        .collect();

    let wt_root = {
        let wp = s(&meta["worktree"], "path");
        if !wp.is_empty() && Path::new(wp).is_dir() {
            PathBuf::from(wp)
        } else {
            let src = s(&meta["worktree"], "source");
            if !src.is_empty() && Path::new(src).is_dir() {
                PathBuf::from(src)
            } else {
                PathBuf::from(s(meta, "top"))
            }
        }
    };
    let mut blocking_blobs = serde_json::Map::new();
    for p in &blocking_files {
        let blob = if wt_root.is_dir() {
            git_blob_hash_file(&wt_root, p)
        } else {
            "absent".into()
        };
        blocking_blobs.insert(p.clone(), json!(blob));
    }

    let blocked_base_head = {
        let nb = s(meta, "netBase");
        if !nb.is_empty() {
            nb.to_string()
        } else {
            let wb = read(run.join("worktree-base-commit")).trim().to_string();
            if !wb.is_empty() {
                wb
            } else {
                let src = Path::new(s(&meta["worktree"], "source"));
                read_git_head(src).unwrap_or_default()
            }
        }
    };

    let mut failed_layers = Vec::new();
    let mut not_run_layers = Vec::new();
    if let Some(layers) = accept_doc["layers"].as_array() {
        for layer in layers {
            let lname = s(layer, "name").to_string();
            if lname.is_empty() {
                continue;
            }
            match s(layer, "status") {
                "failed" => failed_layers.push(lname),
                "not_run" => not_run_layers.push(lname),
                _ => {}
            }
        }
    }

    json!({
        "blocked": true,
        "blockingFiles": blocking_files,
        "blockingBlobs": blocking_blobs,
        "blockedBaseHead": blocked_base_head,
        "failedLayers": failed_layers,
        "notRunLayers": not_run_layers,
        "logFile": s(attr, "logFile"),
        "blockedAt": iso(),
        "rerunCount": 0,
    })
}

/// Build an `AcceptPlan` that re-runs ONLY the previously failed (and downstream `not_run`)
/// acceptance layers, preserving previously passed layers as `reused` and previously skipped
/// layers as `skipped`. Used by zero-LLM mainline-unblock auto-rerun.
pub fn plan_failed_layers_only(
    meta: &Value,
    old_accept: &Value,
    failed_layers: &[String],
    not_run_layers: &[String],
) -> AcceptPlan {
    let rules = parse_rules(meta);
    let legacy_cmd = s(meta, "accept").trim().to_string();
    let accept_also = strings(&meta["acceptAlso"]);

    if !b(old_accept, "tiered") || rules.is_empty() {
        let mut slow = Vec::new();
        if !legacy_cmd.is_empty() {
            slow.push(AcceptLayerSpec {
                name: "accept".into(),
                tier: AcceptTier::Slow,
                command: legacy_cmd,
                lane: true,
                matched_files: Vec::new(),
                reason: "主干解除阻塞自动重验".into(),
            });
        }
        for (idx, extra_cmd) in accept_also.iter().enumerate() {
            slow.push(AcceptLayerSpec {
                name: if accept_also.len() == 1 {
                    "accept-also".into()
                } else {
                    format!("accept-also-{}", idx + 1)
                },
                tier: AcceptTier::Slow,
                command: extra_cmd.clone(),
                lane: true,
                matched_files: Vec::new(),
                reason: "主干解除阻塞自动重验（--accept-also）".into(),
            });
        }
        return AcceptPlan {
            tiered: false,
            fail_closed: false,
            fail_closed_reason: None,
            fast: Vec::new(),
            slow,
            skipped: Vec::new(),
        };
    }

    let mut rerun_set: BTreeSet<&str> = BTreeSet::new();
    for name in failed_layers.iter().chain(not_run_layers.iter()) {
        rerun_set.insert(name.as_str());
    }
    let old_layers_by_name: BTreeMap<String, &Value> = old_accept["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let nm = s(l, "name");
            (!nm.is_empty()).then(|| (nm.to_string(), l))
        })
        .collect();

    let mut fast = Vec::new();
    let mut slow = Vec::new();
    let mut skipped = Vec::new();

    for r in &rules {
        if rerun_set.contains(r.name.as_str()) {
            let spec = AcceptLayerSpec {
                name: r.name.clone(),
                tier: r.tier,
                command: r.command.clone(),
                lane: r.lane,
                matched_files: Vec::new(),
                reason: "主干阻塞解除，仅重跑失败层".into(),
            };
            match r.tier {
                AcceptTier::Fast => fast.push(spec),
                AcceptTier::Slow => slow.push(spec),
            }
        } else if let Some(prev) = old_layers_by_name.get(&r.name) {
            let prev_st = s(prev, "status");
            let is_reused = matches!(prev_st, "passed" | "reused");
            let prev_round = prev["reusedFromRound"].as_u64().or(if prev_st == "passed" { Some(1) } else { None });
            skipped.push(SkippedLayer {
                name: r.name.clone(),
                tier: r.tier,
                command: r.command.clone(),
                reason: if is_reused {
                    match prev_round {
                        Some(rn) => format!("主干解除阻塞自动重验，沿用第 {rn} 轮通过"),
                        None => "主干解除阻塞自动重验，沿用先前通过结果".into(),
                    }
                } else {
                    s(prev, "reason").to_string()
                },
                reused: is_reused,
                reused_from_round: if is_reused { prev_round } else { None },
            });
        } else {
            skipped.push(SkippedLayer {
                name: r.name.clone(),
                tier: r.tier,
                command: r.command.clone(),
                reason: format!("改动未命中 {}", format_path_globs(&r.paths)),
                reused: false,
                reused_from_round: None,
            });
        }
    }

    // Handle legacy "full" fail-closed layer or `--accept-also` layers if they were in `rerun_set`.
    if rerun_set.contains("full") && !legacy_cmd.is_empty() {
        slow.push(AcceptLayerSpec {
            name: "full".into(),
            tier: AcceptTier::Slow,
            command: legacy_cmd,
            lane: true,
            matched_files: Vec::new(),
            reason: "主干阻塞解除，重跑全量层".into(),
        });
    }
    for (idx, extra_cmd) in accept_also.iter().enumerate() {
        let nm = if accept_also.len() == 1 {
            "accept-also".to_string()
        } else {
            format!("accept-also-{}", idx + 1)
        };
        if rerun_set.contains(nm.as_str()) {
            slow.push(AcceptLayerSpec {
                name: nm,
                tier: AcceptTier::Slow,
                command: extra_cmd.clone(),
                lane: true,
                matched_files: Vec::new(),
                reason: "主干阻塞解除，仅重跑失败层".into(),
            });
        }
    }

    AcceptPlan {
        tiered: true,
        fail_closed: b(old_accept, "failClosed"),
        fail_closed_reason: (!s(old_accept, "failClosedReason").is_empty())
            .then(|| s(old_accept, "failClosedReason").to_string()),
        fast,
        slow,
        skipped,
    }
}

/// Concise human-readable acceptance label for L0 status/wait/delivery lines.
pub fn human_accept_label(accept: &Value, infra_failure: bool) -> String {
    let mut label = if b(accept, "cancelled") {
        let by = s(accept, "cancelledBy");
        if by.is_empty() {
            "验收已取消".to_string()
        } else {
            format!("验收已取消（{by}）")
        }
    } else if b(accept, "notExecuted") {
        "验收未执行（排队/基础设施）".to_string()
    } else if infra_failure || b(accept, "infrastructureFailure") {
        "验收基础设施失败".to_string()
    } else if b(accept, "ok") {
        "验收通过".to_string()
    } else if b(accept, "mainlineBlocked") {
        "主干阻塞".to_string()
    } else {
        "验收失败".to_string()
    };

    if b(accept, "tiered") && !b(accept, "cancelled") && !b(accept, "notExecuted") {
        if b(accept, "failClosed") {
            label.push_str("（全量兜底）");
        } else {
            let ran = strings(&accept["ranLayers"]);
            let mut reused = Vec::new();
            let mut skipped = Vec::new();
            for x in accept["skippedLayers"].as_array().into_iter().flatten() {
                let nm = s(x, "name").to_string();
                if nm.is_empty() {
                    continue;
                }
                if b(x, "reused") {
                    reused.push(nm);
                } else {
                    skipped.push(nm);
                }
            }
            let mut detail = Vec::new();
            if !ran.is_empty() {
                if ran.len() <= 3 {
                    detail.push(format!("跑 {}", ran.join("、")));
                } else {
                    detail.push(format!("跑 {} 层", ran.len()));
                }
            }
            if !reused.is_empty() {
                if reused.len() <= 2 {
                    detail.push(format!("沿用 {}", reused.join("、")));
                } else {
                    detail.push(format!("沿用 {} 层", reused.len()));
                }
            }
            if !skipped.is_empty() {
                if skipped.len() <= 2 {
                    detail.push(format!("跳过 {}", skipped.join("、")));
                } else {
                    detail.push(format!("跳过 {} 层", skipped.len()));
                }
            }
            if !detail.is_empty() {
                label.push_str(&format!("（{}）", detail.join("；")));
            }
        }
    }

    if accept["localFallback"].is_object() {
        let reason = s(&accept["localFallback"], "reason");
        label.push_str(&format!("（本地回退：{}）", clip(reason, 80)));
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta() -> Value {
        json!({
            "accept": "cargo clippy --workspace && pnpm check",
            "acceptRules": [
                {
                    "name": "rust-fast",
                    "tier": "fast",
                    "paths": ["crates/**/*.rs"],
                    "command": "cargo fmt --check"
                },
                {
                    "name": "rust-slow",
                    "tier": "slow",
                    "paths": ["crates/**/*.rs"],
                    "command": "cargo clippy --workspace"
                },
                {
                    "name": "web",
                    "tier": "fast",
                    "paths": ["apps/web/**", "packages/**"],
                    "command": "pnpm check:web"
                },
                {
                    "name": "docs",
                    "tier": "fast",
                    "paths": ["docs/**", "*.md", "!docs/decision/**"],
                    "command": "true"
                }
            ]
        })
    }

    #[test]
    fn validate_accept_rules_checks_schema() {
        assert!(validate_accept_rules(None).is_ok());
        assert!(validate_accept_rules(Some(&sample_meta()["acceptRules"])).is_ok());
        for bad in [
            json!([]),
            json!("cargo test"),
            json!([{"name": "", "paths": ["a"], "command": "true"}]),
            json!([{"name": "a", "paths": [], "command": "true"}]),
            json!([{"name": "a", "paths": ["!docs/**"], "command": "true"}]),
            json!([{"name": "a", "paths": ["a"], "command": "true", "tier": "medium"}]),
            json!([{"name": "a", "paths": ["a"], "command": "true", "unknown": 1}]),
            json!([
                {"name": "dup", "paths": ["a"], "command": "true"},
                {"name": "dup", "paths": ["b"], "command": "true"}
            ]),
        ] {
            assert!(validate_accept_rules(Some(&bad)).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn plan_selects_matched_layers_and_skips_unrelated() {
        let meta = sample_meta();
        let p = plan(
            &meta,
            &["apps/web/src/App.tsx".to_string()],
            true,
            None,
            &BTreeMap::new(),
        );
        assert!(p.tiered);
        assert!(!p.fail_closed);
        assert_eq!(p.fast.len(), 1);
        assert_eq!(p.fast[0].name, "web");
        assert!(p.slow.is_empty());
        assert_eq!(p.skipped.len(), 3);
        assert!(p.skipped.iter().any(|s| s.name == "rust-slow" && !s.reused));
    }

    #[test]
    fn plan_fails_closed_on_build_config_and_uncovered_paths() {
        let meta = sample_meta();
        for build_file in [
            "Cargo.toml",
            "crates/core/Cargo.toml",
            "xtask/src/main.rs",
            "package.json",
            "pnpm-lock.yaml",
        ] {
            let p = plan(
                &meta,
                &[build_file.to_string()],
                true,
                None,
                &BTreeMap::new(),
            );
            assert!(p.fail_closed, "expected fail-closed for {build_file}");
            assert_eq!(p.slow.len(), 1);
            assert_eq!(p.slow[0].name, "full");
        }
        // Uncovered path (also excluded by !docs/decision/**) -> fail-closed!
        let p_unc = plan(
            &meta,
            &["docs/decision/001.md".to_string()],
            true,
            None,
            &BTreeMap::new(),
        );
        assert!(p_unc.fail_closed);
        assert_eq!(p_unc.slow[0].name, "full");

        // Mixed covered + uncovered -> fail-closed!
        let p_mix = plan(
            &meta,
            &["apps/web/src/App.tsx".to_string(), "scripts/deploy.sh".to_string()],
            true,
            None,
            &BTreeMap::new(),
        );
        assert!(p_mix.fail_closed);
    }

    #[test]
    fn plan_reuses_unaffected_passed_layers_on_rework() {
        let meta = sample_meta();
        let all_changed = vec![
            "apps/web/src/App.tsx".to_string(),
            "crates/core/src/lib.rs".to_string(),
        ];
        let mut prev_passed = BTreeMap::new();
        prev_passed.insert("web".to_string(), json!({"ok": true}));
        prev_passed.insert("rust-fast".to_string(), json!({"ok": true}));
        // Suppose rust-slow was cancelled before passing; rework only touched crates/core/src/lib.rs.
        let delta = vec!["crates/core/src/lib.rs".to_string()];
        let p = plan(&meta, &all_changed, true, Some(&delta), &prev_passed);
        assert!(!p.fail_closed);
        // `web` was NOT touched by rework and already passed -> reused!
        assert!(p.skipped.iter().any(|s| s.name == "web" && s.reused));
        // `rust-fast` WAS touched -> re-runs!
        assert_eq!(p.fast.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["rust-fast"]);
        // `rust-slow` WAS touched (and hadn't passed) -> runs!
        assert_eq!(p.slow.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["rust-slow"]);
    }

    #[test]
    fn failure_attribution_flags_errors_outside_run_changes_and_ignores_warnings() {
        let base = std::env::temp_dir().join(format!("delegate-attr-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let repo = base.join("repo");
        let run = base.join("run");
        fs::create_dir_all(repo.join("apps/web/src/styles")).unwrap();
        fs::create_dir_all(repo.join("apps/web/src/scene")).unwrap();
        fs::create_dir_all(repo.join("crates/runtime/src")).unwrap();
        fs::create_dir_all(&run).unwrap();
        fs::write(repo.join("apps/web/src/styles/agent-birth.css"), ".a {}\n").unwrap();
        fs::write(repo.join("apps/web/src/scene/camera.ts"), "export const x = 1;\n").unwrap();
        fs::write(repo.join("crates/runtime/src/inbound.rs"), "fn main() {}\n").unwrap();
        fs::write(repo.join("crates/runtime/src/retry.rs"), "fn retry() {}\n").unwrap();

        let meta = json!({"worktree": {"path": repo, "source": repo}});

        // Case 1: Error in unrelated CSS file while run only edited camera.ts -> suspectedMainline = true.
        fs::write(
            run.join("accept.log"),
            "=== [accept:fast] web ===\n$ pnpm check:web\napps/web/src/styles/agent-birth.css:14:3: Unknown word\n[exit 1]\n",
        )
        .unwrap();
        let attr1 = analyze_failure_attribution(
            &run,
            &meta,
            &["apps/web/src/scene/camera.ts".to_string()],
        )
        .unwrap();
        assert_eq!(attr1["suspectedMainline"], json!(true), "{attr1}");
        assert_eq!(attr1["inRunFiles"].as_array().unwrap().len(), 0);
        assert_eq!(attr1["outsideRunFiles"][0]["path"], "apps/web/src/styles/agent-birth.css");
        assert_eq!(attr1["outsideRunFiles"][0]["logLine"], 3);

        // Case 2: Warning in run's own file + size-audit FAIL in unrelated inbound.rs -> suspectedMainline = true.
        fs::write(
            run.join("accept.log"),
            "warning: unused variable\n  --> crates/runtime/src/retry.rs:5:9\nFAIL: crates/runtime/src/inbound.rs (1250 lines > 1000)\n",
        )
        .unwrap();
        let attr2 = analyze_failure_attribution(
            &run,
            &meta,
            &["crates/runtime/src/retry.rs".to_string()],
        )
        .unwrap();
        assert_eq!(attr2["suspectedMainline"], json!(true), "{attr2}");
        assert_eq!(attr2["outsideRunFiles"][0]["path"], "crates/runtime/src/inbound.rs");
        assert_eq!(attr2["outsideRunFiles"][0]["logLine"], 3);

        // Case 3: Real compiler error in run's own file -> suspectedMainline = false.
        fs::write(
            run.join("accept.log"),
            "error[E0308]: mismatched types\n  --> crates/runtime/src/retry.rs:42:9\n",
        )
        .unwrap();
        let attr3 = analyze_failure_attribution(
            &run,
            &meta,
            &["crates/runtime/src/retry.rs".to_string()],
        )
        .unwrap();
        assert_eq!(attr3["suspectedMainline"], json!(false), "{attr3}");
        assert_eq!(attr3["inRunFiles"][0]["path"], "crates/runtime/src/retry.rs");

        let _ = fs::remove_dir_all(&base);
    }
}
