use crate::changes;
use crate::common::*;
use crate::lane;
use crate::review::glob_match;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
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
            });
            continue;
        }
        // On rework rounds: if this layer already passed earlier AND none of the rework delta
        // files match this layer, reuse the prior pass without re-running it.
        if let Some(delta) = rework_changed_files {
            let touched_by_rework = delta.iter().any(|p| matches_rule_paths(p, &r.paths));
            if !touched_by_rework && prev_passed.contains_key(&r.name) {
                skipped.push(SkippedLayer {
                    name: r.name.clone(),
                    tier: r.tier,
                    command: r.command.clone(),
                    reason: "返工未改动命中路径，沿用上轮通过结果".into(),
                    reused: true,
                });
                continue;
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
        all_layers.push(json!({
            "name": sk.name,
            "tier": sk.tier.as_str(),
            "command": sk.command,
            "status": status,
            "ok": true,
            "reused": sk.reused,
            "reason": sk.reason,
        }));
        skipped_layers.push(json!({
            "name": sk.name,
            "tier": sk.tier.as_str(),
            "reused": sk.reused,
            "reason": sk.reason,
        }));
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

    let queued = fast_outcome.map_or(0.0, |f| f.queued_seconds)
        + slow_outcome.map_or(0.0, |s| s.queued_seconds);
    if queued >= 1.0 {
        result["queuedSeconds"] = json!(queued as i64);
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
    }

    result
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
    } else {
        "验收失败".to_string()
    };

    if b(accept, "tiered") && !b(accept, "cancelled") && !b(accept, "notExecuted") {
        if b(accept, "failClosed") {
            label.push_str("（全量兜底）");
        } else {
            let ran = strings(&accept["ranLayers"]);
            let skipped: Vec<String> = accept["skippedLayers"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| s(x, "name").to_string())
                .filter(|x| !x.is_empty())
                .collect();
            let mut detail = Vec::new();
            if !ran.is_empty() {
                if ran.len() <= 3 {
                    detail.push(format!("跑 {}", ran.join("、")));
                } else {
                    detail.push(format!("跑 {} 层", ran.len()));
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
}
