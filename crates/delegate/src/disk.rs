//! Global disk health management for delegate build caches:
//! - Zero-cost cleanup (L0): superseded rustc incremental sessions, stale-hashed own-crate
//!   artifacts in `deps/`, and test executables in released pool slots.
//! - Three-signal liveness probing with zero hard time guards:
//!   (A) Cargo `flock` on `.cargo-lock` / `.cargo-build-lock`,
//!   (B) `/proc/[pid]/{fd,cwd,exe}` references to the target directory,
//!   (C) active `delegate lane` tickets (`running: true`) bound to the slot/repo.
//! - Whole-disk watermark global LRU across all non-busy managed targets (`delegate disk prune`).
//! - Second-disk overflow (`overflowRoot`) for new/empty slots when primary is at high watermark
//!   with no evictable non-busy bytes, hysteresis return on release/claim at `<= returnPct`,
//!   and emergency cold-idle slot migration only at `>= migratePct` (never migrating busy slots).
//! - Strictly read-only, zero-lock health snapshot for `status`, `snapshot --json` (TUI), and
//!   Stop hooks, including >24h & >10 GiB stale-slot advisory reminders (never auto-deleting runs).

use crate::common::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_WARN_PCT: u64 = 75;
pub const DEFAULT_HIGH_PCT: u64 = 85;
pub const DEFAULT_MIN_FREE_GIB: u64 = 50;
pub const DEFAULT_TARGET_PCT: u64 = 70;
pub const DEFAULT_MIGRATE_PCT: u64 = 90;
pub const DEFAULT_RETURN_PCT: u64 = 60;
pub const DEFAULT_OVERFLOW_HIGH_PCT: u64 = 90;
pub const DEFAULT_OVERFLOW_MIN_FREE_GIB: u64 = 100;
pub const DEFAULT_OVERFLOW_TARGET_PCT: u64 = 80;
pub const DEFAULT_OVERFLOW_TARGET_FREE_GIB: u64 = 150;
pub const DEFAULT_STALE_RUN_HOURS: u64 = 24;
pub const DEFAULT_STALE_RUN_GIB: u64 = 10;

const GIB: u64 = 1024 * 1024 * 1024;
const MAX_DIAG_ENTRIES: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskConfig {
    pub primary_root: Option<PathBuf>,
    pub overflow_root: Option<PathBuf>,
    pub extra_targets: Vec<PathBuf>,
    pub extra_overflow_targets: Vec<PathBuf>,
    pub warn_pct: u64,
    pub high_pct: u64,
    pub min_free_gib: u64,
    pub target_pct: u64,
    pub migrate_pct: u64,
    pub return_pct: u64,
    pub overflow_high_pct: u64,
    pub overflow_min_free_gib: u64,
    pub overflow_target_pct: u64,
    pub overflow_target_free_gib: u64,
    pub stale_run_hours: u64,
    pub stale_run_gib: u64,
    pub configured: bool,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            primary_root: None,
            overflow_root: None,
            extra_targets: Vec::new(),
            extra_overflow_targets: Vec::new(),
            warn_pct: DEFAULT_WARN_PCT,
            high_pct: DEFAULT_HIGH_PCT,
            min_free_gib: DEFAULT_MIN_FREE_GIB,
            target_pct: DEFAULT_TARGET_PCT,
            migrate_pct: DEFAULT_MIGRATE_PCT,
            return_pct: DEFAULT_RETURN_PCT,
            overflow_high_pct: DEFAULT_OVERFLOW_HIGH_PCT,
            overflow_min_free_gib: DEFAULT_OVERFLOW_MIN_FREE_GIB,
            overflow_target_pct: DEFAULT_OVERFLOW_TARGET_PCT,
            overflow_target_free_gib: DEFAULT_OVERFLOW_TARGET_FREE_GIB,
            stale_run_hours: DEFAULT_STALE_RUN_HOURS,
            stale_run_gib: DEFAULT_STALE_RUN_GIB,
            configured: false,
        }
    }
}

impl DiskConfig {
    pub fn min_free_bytes(&self) -> u64 {
        self.min_free_gib.saturating_mul(GIB)
    }

    pub fn overflow_min_free_bytes(&self) -> u64 {
        self.overflow_min_free_gib.saturating_mul(GIB)
    }

    pub fn overflow_target_free_bytes(&self) -> u64 {
        self.overflow_target_free_gib.saturating_mul(GIB)
    }

    pub fn stale_run_secs(&self) -> i64 {
        self.stale_run_hours.saturating_mul(3600) as i64
    }

    pub fn stale_run_bytes(&self) -> u64 {
        self.stale_run_gib.saturating_mul(GIB)
    }

    pub fn primary_probe_path(&self, pool_root: &Path) -> PathBuf {
        self.primary_root
            .clone()
            .unwrap_or_else(|| pool_root.to_path_buf())
    }

    pub fn primary_is_high(&self, sp: &DiskSpace) -> bool {
        sp.total_bytes > 0
            && (sp.used_pct() >= self.high_pct as f64
                || (self.min_free_bytes() > 0 && sp.avail_bytes < self.min_free_bytes()))
    }

    pub fn primary_bytes_to_free(&self, sp: &DiskSpace) -> u64 {
        if !self.primary_is_high(sp) {
            return 0;
        }
        let target_used = sp.total_bytes.saturating_mul(self.target_pct) / 100;
        let need_pct = sp.used_bytes.saturating_sub(target_used);
        let need_free = self
            .min_free_bytes()
            .saturating_sub(sp.avail_bytes)
            .min(sp.used_bytes);
        need_pct.max(need_free)
    }

    pub fn primary_can_return(&self, sp: &DiskSpace) -> bool {
        sp.total_bytes > 0
            && sp.used_pct() <= self.return_pct as f64
            && !self.primary_is_high(sp)
    }

    pub fn overflow_is_high(&self, sp: &DiskSpace) -> bool {
        sp.total_bytes > 0
            && (sp.used_pct() >= self.overflow_high_pct as f64
                || (self.overflow_min_free_bytes() > 0
                    && sp.avail_bytes < self.overflow_min_free_bytes()))
    }

    pub fn overflow_bytes_to_free(&self, sp: &DiskSpace) -> u64 {
        if !self.overflow_is_high(sp) {
            return 0;
        }
        let target_used = sp.total_bytes.saturating_mul(self.overflow_target_pct) / 100;
        let need_pct = sp.used_bytes.saturating_sub(target_used);
        let need_free = self
            .overflow_target_free_bytes()
            .saturating_sub(sp.avail_bytes)
            .min(sp.used_bytes);
        need_pct.max(need_free)
    }

    pub fn primary_level(&self, sp: &DiskSpace) -> DiskLevel {
        if sp.total_bytes == 0 {
            return DiskLevel::Ok;
        }
        let crit_pct = self.migrate_pct.max(92) as f64;
        if sp.used_pct() >= crit_pct || sp.avail_bytes == 0 {
            DiskLevel::Critical
        } else if self.primary_is_high(sp) {
            DiskLevel::High
        } else if sp.used_pct() >= self.warn_pct as f64 {
            DiskLevel::Warn
        } else {
            DiskLevel::Ok
        }
    }

    pub fn overflow_level(&self, sp: &DiskSpace) -> DiskLevel {
        if sp.total_bytes == 0 {
            return DiskLevel::Ok;
        }
        let crit_free = self.overflow_min_free_bytes() / 2;
        if sp.used_pct() >= 95.0
            || sp.avail_bytes == 0
            || (crit_free > 0 && sp.avail_bytes < crit_free)
        {
            DiskLevel::Critical
        } else if self.overflow_is_high(sp) {
            DiskLevel::High
        } else if sp.used_pct() >= self.warn_pct as f64
            || (self.overflow_target_free_bytes() > 0
                && sp.avail_bytes < self.overflow_target_free_bytes())
        {
            DiskLevel::Warn
        } else {
            DiskLevel::Ok
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskRole {
    Primary,
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiskLevel {
    Ok,
    Warn,
    High,
    Critical,
}

impl DiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            DiskLevel::Ok => "ok",
            DiskLevel::Warn => "warn",
            DiskLevel::High => "high",
            DiskLevel::Critical => "critical",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskSpace {
    pub total_bytes: u64,
    pub avail_bytes: u64,
    pub used_bytes: u64,
}

impl DiskSpace {
    pub fn from_total_avail(total_bytes: u64, avail_bytes: u64) -> Self {
        let avail = avail_bytes.min(total_bytes);
        Self {
            total_bytes,
            avail_bytes: avail,
            used_bytes: total_bytes.saturating_sub(avail),
        }
    }

    pub fn used_pct(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            (self.used_bytes as f64 * 100.0) / (self.total_bytes as f64)
        }
    }

    pub fn after_freed(&self, freed: u64) -> Self {
        let used = self.used_bytes.saturating_sub(freed);
        let avail = self.total_bytes.saturating_sub(used);
        Self {
            total_bytes: self.total_bytes,
            avail_bytes: avail,
            used_bytes: used,
        }
    }

    pub fn after_added(&self, added: u64) -> Self {
        let used = self.used_bytes.saturating_add(added).min(self.total_bytes);
        let avail = self.total_bytes.saturating_sub(used);
        Self {
            total_bytes: self.total_bytes,
            avail_bytes: avail,
            used_bytes: used,
        }
    }
}

pub fn expand_tilde(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("~/") {
        home().join(rest)
    } else if trimmed == "~" {
        home()
    } else {
        PathBuf::from(trimmed)
    }
}

/// Validate the user-level `disk` configuration section.
pub fn validate_config(value: Option<&Value>, repo: bool) -> Res<()> {
    let Some(v) = value else {
        return Ok(());
    };
    if v.is_null() {
        return Ok(());
    }
    if repo {
        return Err("disk is a host-level user config and cannot be set in repository .delegate.json".into());
    }
    let map = v.as_object().ok_or("disk must be an object")?;
    const KNOWN: &[&str] = &[
        "primaryRoot",
        "overflowRoot",
        "extraTargets",
        "extraOverflowTargets",
        "warnPct",
        "highPct",
        "minFreeGib",
        "targetPct",
        "migratePct",
        "returnPct",
        "overflowHighPct",
        "overflowMinFreeGib",
        "overflowTargetPct",
        "overflowTargetFreeGib",
        "staleRunHours",
        "staleRunGib",
    ];
    for key in map.keys() {
        if !KNOWN.contains(&key.as_str()) {
            return Err(format!("disk has unknown field '{key}'"));
        }
    }
    for key in ["primaryRoot", "overflowRoot"] {
        if let Some(val) = map.get(key) {
            if !val.is_null()
                && !val
                    .as_str()
                    .is_some_and(|s| !s.contains('\0'))
            {
                return Err(format!("disk.{key} must be a path string or null"));
            }
        }
    }
    for key in ["extraTargets", "extraOverflowTargets"] {
        if let Some(val) = map.get(key) {
            if !val.is_null()
                && !val.as_array().is_some_and(|arr| {
                    arr.iter()
                        .all(|item| item.as_str().is_some_and(|s| !s.trim().is_empty() && !s.contains('\0')))
                })
            {
                return Err(format!("disk.{key} must be an array of non-empty path strings"));
            }
        }
    }
    let check_pct = |key: &str, default: u64| -> Res<u64> {
        match map.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(val) => match val.as_u64() {
                Some(n) if (1..=99).contains(&n) => Ok(n),
                _ => Err(format!("disk.{key} must be an integer between 1 and 99")),
            },
        }
    };
    let _warn_pct = check_pct("warnPct", DEFAULT_WARN_PCT)?;
    let high_pct = check_pct("highPct", DEFAULT_HIGH_PCT)?;
    let target_pct = check_pct("targetPct", DEFAULT_TARGET_PCT)?;
    let _migrate_pct = check_pct("migratePct", DEFAULT_MIGRATE_PCT)?;
    let return_pct = check_pct("returnPct", DEFAULT_RETURN_PCT)?;
    let overflow_high_pct = check_pct("overflowHighPct", DEFAULT_OVERFLOW_HIGH_PCT)?;
    let overflow_target_pct = check_pct("overflowTargetPct", DEFAULT_OVERFLOW_TARGET_PCT)?;

    if target_pct >= high_pct {
        return Err(format!(
            "disk.targetPct ({target_pct}) must be less than disk.highPct ({high_pct})"
        ));
    }
    if return_pct >= high_pct {
        return Err(format!(
            "disk.returnPct ({return_pct}) must be less than disk.highPct ({high_pct})"
        ));
    }
    if overflow_target_pct >= overflow_high_pct {
        return Err(format!(
            "disk.overflowTargetPct ({overflow_target_pct}) must be less than disk.overflowHighPct ({overflow_high_pct})"
        ));
    }
    for key in [
        "minFreeGib",
        "overflowMinFreeGib",
        "overflowTargetFreeGib",
        "staleRunHours",
        "staleRunGib",
    ] {
        if let Some(val) = map.get(key) {
            if !val.is_null() && val.as_u64().is_none() {
                return Err(format!("disk.{key} must be a non-negative integer"));
            }
        }
    }
    Ok(())
}

pub fn load_config_from(disk_val: Option<&Value>) -> Res<DiskConfig> {
    validate_config(disk_val, false)?;
    let mut cfg = DiskConfig::default();
    if let Some(obj) = disk_val.and_then(Value::as_object) {
        cfg.configured = true;
        if let Some(p) = obj
            .get("primaryRoot")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            cfg.primary_root = Some(expand_tilde(p));
        }
        if let Some(p) = obj
            .get("overflowRoot")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            cfg.overflow_root = Some(expand_tilde(p));
        }
        if let Some(arr) = obj.get("extraTargets").and_then(Value::as_array) {
            cfg.extra_targets = arr
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(expand_tilde)
                .collect();
        }
        if let Some(arr) = obj.get("extraOverflowTargets").and_then(Value::as_array) {
            cfg.extra_overflow_targets = arr
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(expand_tilde)
                .collect();
        }
        if let Some(n) = obj.get("warnPct").and_then(Value::as_u64) {
            cfg.warn_pct = n;
        }
        if let Some(n) = obj.get("highPct").and_then(Value::as_u64) {
            cfg.high_pct = n;
        }
        if let Some(n) = obj.get("minFreeGib").and_then(Value::as_u64) {
            cfg.min_free_gib = n;
        }
        if let Some(n) = obj.get("targetPct").and_then(Value::as_u64) {
            cfg.target_pct = n;
        }
        if let Some(n) = obj.get("migratePct").and_then(Value::as_u64) {
            cfg.migrate_pct = n;
        }
        if let Some(n) = obj.get("returnPct").and_then(Value::as_u64) {
            cfg.return_pct = n;
        }
        if let Some(n) = obj.get("overflowHighPct").and_then(Value::as_u64) {
            cfg.overflow_high_pct = n;
        }
        if let Some(n) = obj.get("overflowMinFreeGib").and_then(Value::as_u64) {
            cfg.overflow_min_free_gib = n;
        }
        if let Some(n) = obj.get("overflowTargetPct").and_then(Value::as_u64) {
            cfg.overflow_target_pct = n;
        }
        if let Some(n) = obj.get("overflowTargetFreeGib").and_then(Value::as_u64) {
            cfg.overflow_target_free_gib = n;
        }
        if let Some(n) = obj.get("staleRunHours").and_then(Value::as_u64) {
            cfg.stale_run_hours = n;
        }
        if let Some(n) = obj.get("staleRunGib").and_then(Value::as_u64) {
            cfg.stale_run_gib = n;
        }
    }
    let env_primary = setting("POOL_TARGET_ROOT", "");
    if !env_primary.trim().is_empty() {
        cfg.primary_root = Some(expand_tilde(&env_primary));
        cfg.configured = true;
    }
    let env_overflow = setting("POOL_TARGET_OVERFLOW_ROOT", "");
    if !env_overflow.trim().is_empty() {
        cfg.overflow_root = Some(expand_tilde(&env_overflow));
        cfg.configured = true;
    }
    Ok(cfg)
}

/// Load and strictly validate disk configuration using the same path as `config::load`.
/// Fails closed with file path + field diagnostics on invalid JSON, unknown/invalid fields,
/// or repository-level `disk` config.
pub fn load_config() -> Res<DiskConfig> {
    let top = std::env::current_dir()
        .ok()
        .and_then(|cwd| git_top_fast(&cwd));
    let loaded = crate::config::load(top.as_deref())?;
    load_config_from(loaded.value.get("disk"))
}

fn load_user_config_strict() -> Res<DiskConfig> {
    let path = crate::config::user_path();
    let user = crate::config::read_file(&path)?;
    if let Some(ref u) = user {
        validate_config(u.get("disk"), false).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    load_config_from(user.as_ref().and_then(|u| u.get("disk")))
}

fn parse_fake_space(raw: &str) -> Option<DiskSpace> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (total_s, avail_s) = trimmed.split_once(':')?;
    let total = total_s.trim().parse::<u64>().ok()?;
    let avail = avail_s.trim().parse::<u64>().ok()?;
    Some(DiskSpace::from_total_avail(total, avail))
}

pub fn statvfs_space(path: &Path) -> Option<DiskSpace> {
    let mut probe = path;
    while !probe.exists() {
        probe = probe.parent()?;
    }
    let cpath = CString::new(probe.as_os_str().as_bytes()).ok()?;
    let mut st = unsafe { std::mem::zeroed::<libc::statvfs>() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = if st.f_frsize > 0 {
        st.f_frsize as u64
    } else {
        st.f_bsize as u64
    };
    let avail = (st.f_bavail as u64).saturating_mul(frsize);
    let total = (st.f_blocks as u64).saturating_mul(frsize);
    Some(DiskSpace::from_total_avail(total, avail))
}

pub fn probe_disk_space(role: DiskRole, path: &Path) -> Option<DiskSpace> {
    let env_key = match role {
        DiskRole::Primary => "DISK_PRIMARY_FAKE_SPACE",
        DiskRole::Overflow => "DISK_OVERFLOW_FAKE_SPACE",
    };
    if let Some(fake) = parse_fake_space(&setting(env_key, "")) {
        return Some(fake);
    }
    statvfs_space(path)
}

pub fn has_fake_space_override() -> bool {
    !setting("DISK_PRIMARY_FAKE_SPACE", "").trim().is_empty()
        || !setting("DISK_OVERFLOW_FAKE_SPACE", "").trim().is_empty()
}

pub trait SpaceProbe {
    fn space(&self, role: DiskRole, path: &Path) -> Option<DiskSpace>;
}

pub struct RealSpaceProbe;

impl SpaceProbe for RealSpaceProbe {
    fn space(&self, role: DiskRole, path: &Path) -> Option<DiskSpace> {
        probe_disk_space(role, path)
    }
}

// ---------------------------------------------------------------------------
// File / Cargo target helpers
// ---------------------------------------------------------------------------

fn entry_disk_bytes(meta: &fs::Metadata) -> u64 {
    let blocks = meta.blocks().saturating_mul(512);
    if blocks > 0 {
        blocks
    } else {
        meta.len()
    }
}

pub fn dir_disk_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_symlink() {
        return entry_disk_bytes(&meta);
    }
    if !meta.is_dir() {
        return entry_disk_bytes(&meta);
    }
    let mut total = entry_disk_bytes(&meta);
    for entry in fs::read_dir(path).into_iter().flatten().flatten() {
        total = total.saturating_add(dir_disk_bytes(&entry.path()));
    }
    total
}

fn is_16_hex(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn split_cargo_hash(stem: &str) -> Option<(&str, &str)> {
    let (prefix, hash) = stem.rsplit_once('-')?;
    if !prefix.is_empty() && is_16_hex(hash) {
        Some((prefix, hash))
    } else {
        None
    }
}

/// Discover workspace/local crate names (normalized `-` -> `_`) from `Cargo.lock` and `Cargo.toml`.
pub fn discover_own_crates(repo_dir: Option<&Path>) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(root) = repo_dir else {
        return out;
    };
    if let Ok(lock) = fs::read_to_string(root.join("Cargo.lock")) {
        let mut in_pkg = false;
        let mut pkg_name: Option<String> = None;
        let mut has_source = false;
        let flush = |name: &mut Option<String>, has_src: &mut bool, set: &mut HashSet<String>| {
            if let Some(n) = name.take() {
                if !*has_src && !n.is_empty() {
                    set.insert(n.replace('-', "_"));
                }
            }
            *has_src = false;
        };
        for raw_line in lock.lines() {
            let line = raw_line.trim();
            if line == "[[package]]" {
                flush(&mut pkg_name, &mut has_source, &mut out);
                in_pkg = true;
                continue;
            }
            if !in_pkg {
                continue;
            }
            if let Some(rest) = line.strip_prefix("name = ") {
                let n = rest.trim().trim_matches('"');
                if !n.is_empty() {
                    pkg_name = Some(n.to_string());
                }
            } else if line.starts_with("source = ") {
                has_source = true;
            }
        }
        flush(&mut pkg_name, &mut has_source, &mut out);
    }
    if let Ok(toml) = fs::read_to_string(root.join("Cargo.toml")) {
        let mut section = String::new();
        for raw_line in toml.lines() {
            let line = raw_line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                section = line.to_string();
                continue;
            }
            if matches!(
                section.as_str(),
                "[package]" | "[lib]" | "[[bin]]" | "[[test]]"
            ) {
                if let Some(rest) = line.strip_prefix("name = ") {
                    let n = rest.trim().trim_matches('"');
                    if !n.is_empty() {
                        out.insert(n.replace('-', "_"));
                    }
                }
            }
        }
    }
    out
}

fn flock_is_held_exclusive(path: &Path) -> bool {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .or_else(|_| OpenOptions::new().read(true).open(path));
    let Ok(f) = file else {
        return false;
    };
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return err.kind() == std::io::ErrorKind::WouldBlock;
    }
    unsafe {
        libc::flock(f.as_raw_fd(), libc::LOCK_UN);
    }
    false
}

fn flock_is_held_shared(path: &Path) -> bool {
    let Ok(f) = OpenOptions::new().read(true).open(path) else {
        return false;
    };
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return err.kind() == std::io::ErrorKind::WouldBlock;
    }
    unsafe {
        libc::flock(f.as_raw_fd(), libc::LOCK_UN);
    }
    false
}

// ---------------------------------------------------------------------------
// Zero-cost cleanup (L0): safe on any non-busy target, zero rebuild cost
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct DeletedEntry {
    pub target: String,
    pub phase: &'static str,
    pub path: PathBuf,
    pub bytes: u64,
}

impl DeletedEntry {
    pub fn to_json(&self) -> Value {
        json!({
            "target": self.target,
            "phase": self.phase,
            "path": self.path.to_string_lossy(),
            "bytes": self.bytes,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ZeroCostStats {
    pub session_bytes: u64,
    pub stale_hash_bytes: u64,
    pub released_exec_bytes: u64,
    pub entries: Vec<DeletedEntry>,
}

impl ZeroCostStats {
    pub fn total_bytes(&self) -> u64 {
        self.session_bytes
            .saturating_add(self.stale_hash_bytes)
            .saturating_add(self.released_exec_bytes)
    }
}

/// Iterate profile directories (`target/<profile>` and `target/<triple>/<profile>`) that contain
/// Cargo subdirs (`deps`, `incremental`, `.fingerprint`, `build`, `examples`).
fn profile_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(target.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        let mut has_cargo_sub = false;
        let mut subdirs = Vec::new();
        for entry in rd.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if matches!(
                s.as_ref(),
                "deps" | "incremental" | ".fingerprint" | "build" | "examples"
            ) {
                has_cargo_sub = true;
            } else if depth < 2 {
                subdirs.push(entry.path());
            }
        }
        if has_cargo_sub && depth >= 1 {
            out.push(dir);
        }
        if depth < 2 {
            for sub in subdirs {
                stack.push((sub, depth + 1));
            }
        }
    }
    out
}

/// L0a: Drop superseded finished `s-*` incremental sessions per crate, keeping the newest finished
/// session, any `s-*-working` session, and any session whose `.lock` is actively flocked.
pub fn prune_sessions_in(
    target: &Path,
    target_id: &str,
    dry_run: bool,
    entries_out: &mut Vec<DeletedEntry>,
) -> u64 {
    let mut freed = 0u64;
    for pdir in profile_dirs(target) {
        let inc = pdir.join("incremental");
        if !inc.is_dir() {
            continue;
        }
        for krate in fs::read_dir(&inc).into_iter().flatten().flatten() {
            if !krate.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            freed = freed.saturating_add(prune_crate_sessions(
                &krate.path(),
                target_id,
                dry_run,
                entries_out,
            ));
        }
    }
    freed
}

fn prune_crate_sessions(
    krate: &Path,
    target_id: &str,
    dry_run: bool,
    entries_out: &mut Vec<DeletedEntry>,
) -> u64 {
    let mut finished: Vec<(String, String)> = fs::read_dir(krate)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let mut parts = name.splitn(4, '-');
            let (Some("s"), Some(stamp), Some(random), Some(tail)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return None;
            };
            (tail != "working").then(|| (stamp.to_string(), format!("s-{stamp}-{random}")))
        })
        .collect();
    finished.sort_by(|a, b| (a.0.len(), &a.0).cmp(&(b.0.len(), &b.0)));
    finished.pop();

    let mut freed = 0u64;
    for (_, prefix) in finished {
        let lock_path = krate.join(format!("{prefix}.lock"));
        if lock_path.exists() && flock_is_held_exclusive(&lock_path) {
            continue;
        }
        for entry in fs::read_dir(krate).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != format!("{prefix}.lock") && !name.starts_with(&format!("{prefix}-")) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() && !path.is_symlink() {
                let mut dir_freed = 0u64;
                for file in fs::read_dir(&path).into_iter().flatten().flatten() {
                    if let Ok(meta) = file.metadata() {
                        // Files shared with the newest session via hard link free nothing.
                        if meta.nlink() == 1 {
                            dir_freed = dir_freed.saturating_add(entry_disk_bytes(&meta));
                        }
                    }
                }
                if !dry_run {
                    let _ = fs::remove_dir_all(&path);
                }
                freed = freed.saturating_add(dir_freed);
                entries_out.push(DeletedEntry {
                    target: target_id.to_string(),
                    phase: "l0_session",
                    path,
                    bytes: dir_freed,
                });
            } else if !dry_run {
                let _ = fs::remove_file(&path);
            }
        }
    }
    freed
}

fn is_deps_test_executable(name: &str, meta: &fs::Metadata) -> Option<(String, String)> {
    if !meta.is_file() || name.contains('.') {
        return None;
    }
    if (meta.mode() & 0o111) == 0 {
        return None;
    }
    let (prefix, hash) = split_cargo_hash(name)?;
    Some((prefix.to_string(), hash.to_string()))
}

fn remove_path_counting(path: &Path, dry_run: bool) -> u64 {
    let bytes = dir_disk_bytes(path);
    if !dry_run {
        if path.is_dir() && !path.is_symlink() {
            let _ = fs::remove_dir_all(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
    bytes
}

/// L0b: Drop older-hash own-crate artifacts in `deps/`:
/// - All executables `<prefix>-<16hex>` (always workspace test/bin executables): per prefix keep newest mtime.
/// - Own-crate libraries `lib<prefix>-<16hex>.{rlib,rmeta}` where `<prefix>` is in `own_crates`:
///   per `(prefix, ext)` keep newest hash by mtime, dropping superseded hashes + companion `.d`/`.fingerprint`.
pub fn prune_stale_hashed_artifacts(
    target: &Path,
    repo_dir: Option<&Path>,
    target_id: &str,
    dry_run: bool,
    entries_out: &mut Vec<DeletedEntry>,
) -> u64 {
    let own_crates = discover_own_crates(repo_dir);
    let mut freed = 0u64;

    for pdir in profile_dirs(target) {
        let deps = pdir.join("deps");
        if !deps.is_dir() {
            continue;
        }
        let fingerprint_dir = pdir.join(".fingerprint");

        type HashedEntry = (i64, String, PathBuf);
        // Group executables by prefix: prefix -> Vec<(mtime, hash, path)>
        let mut execs: BTreeMap<String, Vec<HashedEntry>> = BTreeMap::new();
        // Group own-crate libs by (prefix, ext): (prefix, ext) -> Vec<(mtime, hash, path)>
        let mut own_libs: BTreeMap<(String, String), Vec<HashedEntry>> = BTreeMap::new();

        for entry in fs::read_dir(&deps).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_symlink() || !meta.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some((prefix, hash)) = is_deps_test_executable(&name, &meta) {
                execs
                    .entry(prefix)
                    .or_default()
                    .push((meta.mtime(), hash, path));
                continue;
            }
            if let Some((stem, ext)) = name.rsplit_once('.') {
                if !matches!(ext, "rlib" | "rmeta") {
                    continue;
                }
                let Some(rest) = stem.strip_prefix("lib") else {
                    continue;
                };
                let Some((prefix, hash)) = split_cargo_hash(rest) else {
                    continue;
                };
                if own_crates.contains(prefix) {
                    own_libs
                        .entry((prefix.to_string(), ext.to_string()))
                        .or_default()
                        .push((meta.mtime(), hash.to_string(), path));
                }
            }
        }

        // Drop older-hash executables per prefix.
        for (prefix, mut list) in execs {
            if list.len() <= 1 {
                continue;
            }
            list.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let kept_hash = list.pop().map(|x| x.1).unwrap_or_default();
            for (_, hash, path) in list {
                if hash == kept_hash {
                    continue;
                }
                let mut b = remove_path_counting(&path, dry_run);
                let dep_file = deps.join(format!("{prefix}-{hash}.d"));
                if dep_file.is_file() {
                    b = b.saturating_add(remove_path_counting(&dep_file, dry_run));
                }
                let fp = fingerprint_dir.join(format!("{prefix}-{hash}"));
                if fp.is_dir() {
                    b = b.saturating_add(remove_path_counting(&fp, dry_run));
                }
                freed = freed.saturating_add(b);
                entries_out.push(DeletedEntry {
                    target: target_id.to_string(),
                    phase: "l0_stale_hash",
                    path,
                    bytes: b,
                });
            }
        }

        // Track kept hashes across rlib/rmeta so we only delete companion `.d`/`.fingerprint` when
        // no file for that `(prefix, hash)` survives.
        let mut surviving_hashes: HashSet<(String, String)> = HashSet::new();
        let mut stale_lib_items: Vec<(String, String, PathBuf)> = Vec::new();
        for ((prefix, _ext), mut list) in own_libs {
            list.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            if let Some((_, kept_hash, _)) = list.pop() {
                surviving_hashes.insert((prefix.clone(), kept_hash.clone()));
                for (_, hash, path) in list {
                    if hash == kept_hash {
                        continue;
                    }
                    stale_lib_items.push((prefix.clone(), hash, path));
                }
            }
        }
        let mut cleaned_companions: HashSet<(String, String)> = HashSet::new();
        for (prefix, hash, path) in stale_lib_items {
            let mut b = remove_path_counting(&path, dry_run);
            let key = (prefix.clone(), hash.clone());
            if !surviving_hashes.contains(&key) && cleaned_companions.insert(key) {
                for cand in [
                    deps.join(format!("lib{prefix}-{hash}.d")),
                    deps.join(format!("{prefix}-{hash}.d")),
                ] {
                    if cand.is_file() {
                        b = b.saturating_add(remove_path_counting(&cand, dry_run));
                    }
                }
                let fp = fingerprint_dir.join(format!("{prefix}-{hash}"));
                if fp.is_dir() {
                    b = b.saturating_add(remove_path_counting(&fp, dry_run));
                }
            }
            freed = freed.saturating_add(b);
            entries_out.push(DeletedEntry {
                target: target_id.to_string(),
                phase: "l0_stale_hash",
                path,
                bytes: b,
            });
        }
    }
    freed
}

/// L0c: For a released pool slot, remove test executables (`<prefix>-<16hex>`, `+x`, no extension)
/// and their `.d` files from `deps/`, keeping `.rlib`/`.rmeta`/`.so`/`build/` warm for reuse.
pub fn prune_released_slot_test_executables(
    target: &Path,
    target_id: &str,
    dry_run: bool,
    entries_out: &mut Vec<DeletedEntry>,
) -> u64 {
    let mut freed = 0u64;
    for pdir in profile_dirs(target) {
        let deps = pdir.join("deps");
        if !deps.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&deps).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((prefix, hash)) = is_deps_test_executable(&name, &meta) else {
                continue;
            };
            let mut b = remove_path_counting(&path, dry_run);
            let dep_file = deps.join(format!("{prefix}-{hash}.d"));
            if dep_file.is_file() {
                b = b.saturating_add(remove_path_counting(&dep_file, dry_run));
            }
            freed = freed.saturating_add(b);
            entries_out.push(DeletedEntry {
                target: target_id.to_string(),
                phase: "l0_released_exec",
                path,
                bytes: b,
            });
        }
    }
    freed
}

/// Run all L0 zero-cost cleanups on a single non-busy target.
pub fn zero_cost_cleanup(
    target: &Path,
    repo_dir: Option<&Path>,
    is_released_slot: bool,
    target_id: &str,
    dry_run: bool,
) -> ZeroCostStats {
    let mut stats = ZeroCostStats::default();
    if !target.is_dir() {
        return stats;
    }
    stats.session_bytes = prune_sessions_in(target, target_id, dry_run, &mut stats.entries);
    stats.stale_hash_bytes =
        prune_stale_hashed_artifacts(target, repo_dir, target_id, dry_run, &mut stats.entries);
    if is_released_slot {
        stats.released_exec_bytes =
            prune_released_slot_test_executables(target, target_id, dry_run, &mut stats.entries);
    }
    stats
}

// ---------------------------------------------------------------------------
// Three-signal liveness probing (zero hard time protection)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusyReason {
    CargoFlock { lock_path: PathBuf },
    ProcRef { pid: u32, kind: &'static str, path: PathBuf },
    LaneTicket { ticket: PathBuf, label: String },
}

impl BusyReason {
    pub fn kind_str(&self) -> &'static str {
        match self {
            BusyReason::CargoFlock { .. } => "cargo_flock",
            BusyReason::ProcRef { .. } => "proc_ref",
            BusyReason::LaneTicket { .. } => "lane_ticket",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            BusyReason::CargoFlock { lock_path } => lock_path.to_string_lossy().into_owned(),
            BusyReason::ProcRef { pid, kind, path } => {
                format!("pid {pid} ({kind}): {}", path.display())
            }
            BusyReason::LaneTicket { ticket, label } => {
                let name = ticket
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("ticket");
                if label.is_empty() {
                    name.to_string()
                } else {
                    format!("{name} ({label})")
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ManagedTarget {
    pub id: String,
    pub target_dir: PathBuf,
    pub canonical_target: PathBuf,
    pub logical_target: PathBuf,
    pub slot_dir: Option<PathBuf>,
    pub repo_dir: Option<PathBuf>,
    pub disk: DiskRole,
    pub is_released_slot: bool,
}

/// Signal A: non-blocking `flock(LOCK_EX | LOCK_NB)` on `.cargo-lock` / `.cargo-build-lock`
/// inside `<target>`, `<target>/<profile>`, and `<target>/<triple>/<profile>`.
pub fn probe_cargo_flock(target: &Path) -> Option<BusyReason> {
    let check_dir = |dir: &Path| -> Option<BusyReason> {
        for name in [".cargo-lock", ".cargo-build-lock"] {
            let p = dir.join(name);
            if p.is_file() && flock_is_held_exclusive(&p) {
                return Some(BusyReason::CargoFlock { lock_path: p });
            }
        }
        None
    };
    if let Some(reason) = check_dir(target) {
        return Some(reason);
    }
    let mut stack = vec![(target.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if matches!(
                s.as_ref(),
                "deps" | "incremental" | ".fingerprint" | "build" | "examples"
            ) {
                continue;
            }
            let sub = entry.path();
            if let Some(reason) = check_dir(&sub) {
                return Some(reason);
            }
            if depth < 1 {
                stack.push((sub, depth + 1));
            }
        }
    }
    None
}

fn clean_proc_link(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_suffix(" (deleted)") {
        PathBuf::from(stripped)
    } else {
        p
    }
}

fn path_inside_target(candidate: &Path, target: &ManagedTarget) -> bool {
    candidate == target.canonical_target
        || candidate.starts_with(&target.canonical_target)
        || candidate == target.logical_target
        || candidate.starts_with(&target.logical_target)
        || candidate == target.target_dir
        || candidate.starts_with(&target.target_dir)
}

/// Signal B: single-pass `/proc/[0-9]*/{fd/*,cwd,exe}` scan matching any managed target.
pub fn probe_proc_refs_in(
    proc_root: &Path,
    targets: &[ManagedTarget],
    skip_pid: Option<u32>,
) -> HashMap<usize, BusyReason> {
    let mut hits: HashMap<usize, BusyReason> = HashMap::new();
    if targets.is_empty() {
        return hits;
    }
    let Ok(rd) = fs::read_dir(proc_root) else {
        return hits;
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if skip_pid == Some(pid) {
            continue;
        }
        let pdir = ent.path();
        for (kind, link_path) in [("cwd", pdir.join("cwd")), ("exe", pdir.join("exe"))] {
            if let Ok(dest) = fs::read_link(&link_path) {
                let cleaned = clean_proc_link(dest);
                for (idx, t) in targets.iter().enumerate() {
                    if !hits.contains_key(&idx) && path_inside_target(&cleaned, t) {
                        hits.insert(
                            idx,
                            BusyReason::ProcRef {
                                pid,
                                kind,
                                path: cleaned.clone(),
                            },
                        );
                    }
                }
            }
        }
        if let Ok(fds) = fs::read_dir(pdir.join("fd")) {
            for fd_ent in fds.flatten() {
                let Ok(dest) = fs::read_link(fd_ent.path()) else {
                    continue;
                };
                let cleaned = clean_proc_link(dest);
                for (idx, t) in targets.iter().enumerate() {
                    if !hits.contains_key(&idx) && path_inside_target(&cleaned, t) {
                        hits.insert(
                            idx,
                            BusyReason::ProcRef {
                                pid,
                                kind: "fd",
                                path: cleaned.clone(),
                            },
                        );
                    }
                }
            }
        }
        if hits.len() == targets.len() {
            break;
        }
    }
    hits
}

fn path_matches_slot_or_target(candidate: &Path, target: &ManagedTarget) -> bool {
    let cand_canon = fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
    if path_inside_target(candidate, target) || path_inside_target(&cand_canon, target) {
        return true;
    }
    for anchor in [target.slot_dir.as_deref(), target.repo_dir.as_deref()]
        .into_iter()
        .flatten()
    {
        let anc_canon = fs::canonicalize(anchor).unwrap_or_else(|_| anchor.to_path_buf());
        if candidate == anchor
            || candidate.starts_with(anchor)
            || cand_canon == anc_canon
            || cand_canon.starts_with(&anc_canon)
        {
            return true;
        }
    }
    false
}

/// Signal C: active `lane` tickets (`*.ticket` held by flock with `running: true`) whose run or
/// context maps to a managed target's slot/repo/target.
pub fn probe_lane_tickets_in(
    lane_dir: &Path,
    targets: &[ManagedTarget],
) -> HashMap<usize, BusyReason> {
    let mut hits: HashMap<usize, BusyReason> = HashMap::new();
    if targets.is_empty() {
        return hits;
    }
    let Ok(rd) = fs::read_dir(lane_dir) else {
        return hits;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if !path.extension().is_some_and(|e| e == "ticket") {
            continue;
        }
        if !flock_is_held_shared(&path) {
            continue;
        }
        let doc = json(&path);
        if !b(&doc, "running") {
            continue;
        }
        let label = s(&doc, "label").to_string();
        let mut candidate_paths: Vec<PathBuf> = Vec::new();
        for key in ["context", "slot", "target", "workdir", "cwd"] {
            let v = s(&doc, key);
            if !v.is_empty() {
                let p = PathBuf::from(v);
                candidate_paths.push(p.clone());
                // Context may itself be a run directory containing meta.json.
                let meta_p = p.join("meta.json");
                if meta_p.is_file() {
                    let wt = s(&json(&meta_p)["worktree"], "path").to_string();
                    if !wt.is_empty() {
                        candidate_paths.push(PathBuf::from(wt));
                    }
                }
            }
        }
        let run_str = s(&doc, "run");
        if !run_str.is_empty() {
            let run_p = PathBuf::from(run_str);
            let meta = json(run_p.join("meta.json"));
            let wt = s(&meta["worktree"], "path");
            if !wt.is_empty() {
                candidate_paths.push(PathBuf::from(wt));
            }
            let wd = s(&meta, "workdir");
            if !wd.is_empty() {
                candidate_paths.push(PathBuf::from(wd));
            }
        }
        for (idx, t) in targets.iter().enumerate() {
            if !hits.contains_key(&idx)
                && candidate_paths
                    .iter()
                    .any(|cand| path_matches_slot_or_target(cand, t))
            {
                hits.insert(
                    idx,
                    BusyReason::LaneTicket {
                        ticket: path.clone(),
                        label: label.clone(),
                    },
                );
            }
        }
    }
    hits
}

/// Probe liveness for all `targets` using all 3 signals (A: Cargo flock, B: `/proc`, C: lane ticket).
pub fn probe_all_busy(
    targets: &[ManagedTarget],
    proc_root: &Path,
    lane_dir: &Path,
) -> HashMap<usize, BusyReason> {
    let mut busy = HashMap::new();
    for (idx, t) in targets.iter().enumerate() {
        if let Some(reason) = probe_cargo_flock(&t.target_dir) {
            busy.insert(idx, reason);
        }
    }
    let proc_hits = probe_proc_refs_in(proc_root, targets, Some(std::process::id()));
    for (idx, reason) in proc_hits {
        busy.entry(idx).or_insert(reason);
    }
    let lane_hits = probe_lane_tickets_in(lane_dir, targets);
    for (idx, reason) in lane_hits {
        busy.entry(idx).or_insert(reason);
    }
    busy
}

pub fn is_slot_busy(slot: &Path, cfg: &DiskConfig) -> bool {
    let Some(target) = target_for_slot(slot, cfg, false) else {
        return false;
    };
    let list = [target];
    let lane_dir = state_dir().join("lane");
    !probe_all_busy(&list, Path::new("/proc"), &lane_dir).is_empty()
}

// ---------------------------------------------------------------------------
// Target discovery
// ---------------------------------------------------------------------------

pub fn path_is_under_overflow(path: &Path, cfg: &DiskConfig) -> bool {
    let Some(ref ov) = cfg.overflow_root else {
        return false;
    };
    let ov_canon = fs::canonicalize(ov).unwrap_or_else(|_| ov.clone());
    let p_canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.starts_with(ov) || p_canon.starts_with(&ov_canon)
}

fn resolve_slot_link(slot: &Path) -> Option<(PathBuf, PathBuf)> {
    let link = slot.join("target");
    if link.is_symlink() {
        let raw = fs::read_link(&link).ok()?;
        let dest = if raw.is_absolute() {
            raw
        } else {
            slot.join(raw)
        };
        Some((link, dest))
    } else if link.is_dir() {
        Some((link.clone(), link))
    } else {
        None
    }
}

fn target_for_slot(slot: &Path, cfg: &DiskConfig, is_released: bool) -> Option<ManagedTarget> {
    let (logical_target, target_dir) = resolve_slot_link(slot)?;
    if !target_dir.is_dir() {
        return None;
    }
    let name = slot
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let disk = if path_is_under_overflow(&target_dir, cfg) {
        DiskRole::Overflow
    } else {
        DiskRole::Primary
    };
    let canonical_target = fs::canonicalize(&target_dir).unwrap_or_else(|_| target_dir.clone());
    Some(ManagedTarget {
        id: format!("slot:{name}"),
        target_dir,
        canonical_target,
        logical_target,
        slot_dir: Some(slot.to_path_buf()),
        repo_dir: Some(slot.to_path_buf()),
        disk,
        is_released_slot: is_released,
    })
}

pub fn discover_targets(
    cfg: &DiskConfig,
    pool_root: &Path,
    slot_held_fn: &dyn Fn(&Path) -> bool,
) -> Vec<ManagedTarget> {
    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    // 1. Active & released pool slots under `pool_root`.
    if let Ok(rd) = fs::read_dir(pool_root) {
        let mut slots: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && !p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with('.') || n.ends_with(".owner"))
            })
            .collect();
        slots.sort();
        for slot in slots {
            let released = !slot_held_fn(&slot);
            if let Some(t) = target_for_slot(&slot, cfg, released) {
                if seen.insert(t.canonical_target.clone()) {
                    out.push(t);
                }
            }
        }
    }

    // 2. Orphaned slot target directories under primaryRoot / overflowRoot.
    for (root_opt, role) in [
        (cfg.primary_root.as_deref(), DiskRole::Primary),
        (cfg.overflow_root.as_deref(), DiskRole::Overflow),
    ] {
        let Some(root) = root_opt else {
            continue;
        };
        let Ok(rd) = fs::read_dir(root) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for ent in entries {
            let tdir = ent.join("target");
            if !tdir.is_dir() {
                continue;
            }
            let canon = fs::canonicalize(&tdir).unwrap_or_else(|_| tdir.clone());
            if !seen.insert(canon.clone()) {
                continue;
            }
            let name = ent
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(ManagedTarget {
                id: format!("orphan:{name}"),
                target_dir: tdir.clone(),
                canonical_target: canon,
                logical_target: tdir,
                slot_dir: None,
                repo_dir: None,
                disk: role,
                is_released_slot: true,
            });
        }
    }

    // 3. Extra registered targets (e.g. main repo target).
    for extra in &cfg.extra_targets {
        if !extra.is_dir() {
            continue;
        }
        let canon = fs::canonicalize(extra).unwrap_or_else(|_| extra.clone());
        if !seen.insert(canon.clone()) {
            continue;
        }
        let role = if path_is_under_overflow(extra, cfg) {
            DiskRole::Overflow
        } else {
            DiskRole::Primary
        };
        out.push(ManagedTarget {
            id: format!("extra:{}", extra.display()),
            target_dir: extra.clone(),
            canonical_target: canon,
            logical_target: extra.clone(),
            slot_dir: None,
            repo_dir: extra.parent().map(Path::to_path_buf),
            disk: role,
            is_released_slot: false,
        });
    }
    for extra in &cfg.extra_overflow_targets {
        if !extra.is_dir() {
            continue;
        }
        let canon = fs::canonicalize(extra).unwrap_or_else(|_| extra.clone());
        if !seen.insert(canon.clone()) {
            continue;
        }
        out.push(ManagedTarget {
            id: format!("extra-overflow:{}", extra.display()),
            target_dir: extra.clone(),
            canonical_target: canon,
            logical_target: extra.clone(),
            slot_dir: None,
            repo_dir: extra.parent().map(Path::to_path_buf),
            disk: DiskRole::Overflow,
            is_released_slot: false,
        });
    }

    out
}

// ---------------------------------------------------------------------------
// Global LRU eviction (only rebuildable Cargo units, non-busy targets only)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EvictTier {
    /// Test executables in `deps/` (fast to relink, big space savings).
    TestExec = 1,
    /// `incremental/<crate-hash>` directories.
    Incremental = 2,
    /// Own-crate `.rlib`/`.rmeta`/`.fingerprint` + `examples/`.
    OwnLib = 3,
    /// Third-party `.rlib`/`.rmeta`/`.so`, `build/`, and remaining `.fingerprint/`.
    OtherBuild = 4,
}

impl EvictTier {
    fn phase_name(self) -> &'static str {
        match self {
            EvictTier::TestExec => "lru_exec",
            EvictTier::Incremental => "lru_incremental",
            EvictTier::OwnLib => "lru_own_lib",
            EvictTier::OtherBuild => "lru_other",
        }
    }
}

#[derive(Clone, Debug)]
struct EvictUnit {
    target_id: String,
    tier: EvictTier,
    atime: i64,
    mtime: i64,
    bytes: u64,
    paths: Vec<PathBuf>,
    primary_path: PathBuf,
}

fn subtree_stats(path: &Path) -> (u64, i64, i64) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return (0, 0, 0);
    };
    let mut bytes = entry_disk_bytes(&meta);
    let mut max_mtime = meta.mtime();
    // Never use a directory's atime (listing it updates atime).
    let mut max_atime = if meta.is_dir() { 0 } else { meta.atime() };
    if meta.is_dir() && !meta.is_symlink() {
        for ent in fs::read_dir(path).into_iter().flatten().flatten() {
            let (b, a, m) = subtree_stats(&ent.path());
            bytes = bytes.saturating_add(b);
            max_atime = max_atime.max(a);
            max_mtime = max_mtime.max(m);
        }
    }
    (bytes, max_atime, max_mtime)
}

fn incremental_dir_has_working_or_locked(krate_dir: &Path) -> bool {
    for ent in fs::read_dir(krate_dir).into_iter().flatten().flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with("s-") && name.ends_with("-working") {
            return true;
        }
        if name.ends_with(".lock") && flock_is_held_exclusive(&ent.path()) {
            return true;
        }
    }
    false
}

fn incremental_dir_atime(krate_dir: &Path) -> (u64, i64, i64) {
    let (bytes, mut atime, mtime) = subtree_stats(krate_dir);
    // Prefer `s-*/dep-graph.bin` atime if present (most reliable signal of rustc reuse).
    let mut dep_atime = 0i64;
    for ent in fs::read_dir(krate_dir).into_iter().flatten().flatten() {
        let p = ent.path();
        if p.is_dir() {
            let dg = p.join("dep-graph.bin");
            if let Ok(m) = fs::symlink_metadata(&dg) {
                dep_atime = dep_atime.max(m.atime());
            }
        }
    }
    if dep_atime > 0 {
        atime = dep_atime;
    } else if atime == 0 {
        atime = mtime;
    }
    (bytes, atime, mtime)
}

fn unit_key(dir: &str, name: &str) -> String {
    if dir == "incremental" {
        return name.to_string();
    }
    let (stem, ext) = name.split_once('.').unwrap_or((name, ""));
    let library = ["rlib", "rmeta", "so", "a", "dylib"]
        .contains(&ext.rsplit('.').next().unwrap_or(""));
    match stem.strip_prefix("lib") {
        Some(rest) if library => rest.to_string(),
        _ => stem.to_string(),
    }
}

fn collect_evictable_units(target: &ManagedTarget) -> Vec<EvictUnit> {
    let mut out = Vec::new();
    if !target.target_dir.is_dir() {
        return out;
    }
    let own_crates = discover_own_crates(target.repo_dir.as_deref());

    for pdir in profile_dirs(&target.target_dir) {
        // 1. Incremental directories (one unit per `<crate-hash>` dir, skipping active working/locked).
        let inc_dir = pdir.join("incremental");
        if inc_dir.is_dir() {
            for ent in fs::read_dir(&inc_dir).into_iter().flatten().flatten() {
                let p = ent.path();
                if !p.is_dir() || p.is_symlink() {
                    continue;
                }
                if incremental_dir_has_working_or_locked(&p) {
                    continue;
                }
                let (bytes, atime, mtime) = incremental_dir_atime(&p);
                if bytes == 0 {
                    continue;
                }
                out.push(EvictUnit {
                    target_id: target.id.clone(),
                    tier: EvictTier::Incremental,
                    atime,
                    mtime,
                    bytes,
                    primary_path: p.clone(),
                    paths: vec![p],
                });
            }
        }

        // 2. Grouped units across `deps/`, `.fingerprint/`, `build/`, `examples/`.
        type EvictGroupVal = (Vec<PathBuf>, u64, i64, i64);
        let mut grouped: BTreeMap<(EvictTier, String), EvictGroupVal> = BTreeMap::new();
        let mut exec_keys: HashSet<String> = HashSet::new();

        // First scan `deps/` to identify test executables vs own libs vs 3p libs.
        let deps_dir = pdir.join("deps");
        if deps_dir.is_dir() {
            for ent in fs::read_dir(&deps_dir).into_iter().flatten().flatten() {
                let p = ent.path();
                let Ok(meta) = fs::symlink_metadata(&p) else {
                    continue;
                };
                let name = ent.file_name().to_string_lossy().into_owned();
                let key = unit_key("deps", &name);
                if is_deps_test_executable(&name, &meta).is_some() {
                    exec_keys.insert(key.clone());
                    let (b, a, m) = subtree_stats(&p);
                    let e = grouped.entry((EvictTier::TestExec, key)).or_default();
                    e.0.push(p);
                    e.1 = e.1.saturating_add(b);
                    e.2 = e.2.max(a);
                    e.3 = e.3.max(m);
                } else {
                    let prefix = split_cargo_hash(&key).map(|(pr, _)| pr).unwrap_or("");
                    let tier = if exec_keys.contains(&key) {
                        EvictTier::TestExec
                    } else if !prefix.is_empty() && own_crates.contains(prefix) {
                        EvictTier::OwnLib
                    } else {
                        EvictTier::OtherBuild
                    };
                    let (b, a, m) = subtree_stats(&p);
                    let e = grouped.entry((tier, key)).or_default();
                    e.0.push(p);
                    e.1 = e.1.saturating_add(b);
                    e.2 = e.2.max(a);
                    e.3 = e.3.max(m);
                }
            }
        }

        // Companion `.d` files that were scanned before their executable get folded into TestExec.
        for key in &exec_keys {
            if let Some(other) = grouped.remove(&(EvictTier::OtherBuild, key.clone())) {
                let e = grouped
                    .entry((EvictTier::TestExec, key.clone()))
                    .or_default();
                e.0.extend(other.0);
                e.1 = e.1.saturating_add(other.1);
                e.2 = e.2.max(other.2);
                e.3 = e.3.max(other.3);
            }
            if let Some(own) = grouped.remove(&(EvictTier::OwnLib, key.clone())) {
                let e = grouped
                    .entry((EvictTier::TestExec, key.clone()))
                    .or_default();
                e.0.extend(own.0);
                e.1 = e.1.saturating_add(own.1);
                e.2 = e.2.max(own.2);
                e.3 = e.3.max(own.3);
            }
        }

        for (sub_name, default_tier) in [
            ("examples", EvictTier::OwnLib),
            (".fingerprint", EvictTier::OtherBuild),
            ("build", EvictTier::OtherBuild),
        ] {
            let sub = pdir.join(sub_name);
            if !sub.is_dir() {
                continue;
            }
            for ent in fs::read_dir(&sub).into_iter().flatten().flatten() {
                let p = ent.path();
                let name = ent.file_name().to_string_lossy().into_owned();
                let key = unit_key(sub_name, &name);
                let prefix = split_cargo_hash(&key).map(|(pr, _)| pr).unwrap_or("");
                let tier = if exec_keys.contains(&key) {
                    EvictTier::TestExec
                } else if sub_name == "examples"
                    || (!prefix.is_empty() && own_crates.contains(prefix))
                {
                    EvictTier::OwnLib
                } else {
                    default_tier
                };
                let (b, a, m) = subtree_stats(&p);
                let e = grouped.entry((tier, key)).or_default();
                e.0.push(p);
                e.1 = e.1.saturating_add(b);
                e.2 = e.2.max(a);
                e.3 = e.3.max(m);
            }
        }

        for ((tier, _key), (paths, bytes, atime, mtime)) in grouped {
            if bytes == 0 || paths.is_empty() {
                continue;
            }
            let eff_atime = if atime > 0 { atime } else { mtime };
            let primary_path = paths[0].clone();
            out.push(EvictUnit {
                target_id: target.id.clone(),
                tier,
                atime: eff_atime,
                mtime,
                bytes,
                paths,
                primary_path,
            });
        }
    }
    out
}

fn evict_units_global_lru(
    targets: &[ManagedTarget],
    busy: &HashMap<usize, BusyReason>,
    role: DiskRole,
    bytes_to_free: u64,
    dry_run: bool,
    entries_out: &mut Vec<DeletedEntry>,
) -> (u64, usize, bool) {
    let mut units: Vec<EvictUnit> = Vec::new();
    for (idx, t) in targets.iter().enumerate() {
        if t.disk != role || busy.contains_key(&idx) {
            continue;
        }
        units.extend(collect_evictable_units(t));
    }
    if bytes_to_free == 0 || units.is_empty() {
        return (0, 0, !units.is_empty());
    }
    units.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then_with(|| a.atime.cmp(&b.atime))
            .then_with(|| a.mtime.cmp(&b.mtime))
            .then_with(|| a.primary_path.cmp(&b.primary_path))
    });
    let total_units = units.len();
    let mut freed = 0u64;
    let mut evicted_count = 0usize;
    for unit in units {
        if freed >= bytes_to_free {
            break;
        }
        if !dry_run {
            for p in &unit.paths {
                if p.is_dir() && !p.is_symlink() {
                    let _ = fs::remove_dir_all(p);
                } else {
                    let _ = fs::remove_file(p);
                }
            }
        }
        freed = freed.saturating_add(unit.bytes);
        evicted_count += 1;
        entries_out.push(DeletedEntry {
            target: unit.target_id,
            phase: unit.tier.phase_name(),
            path: unit.primary_path,
            bytes: unit.bytes,
        });
    }
    let has_remaining = evicted_count < total_units;
    (freed, evicted_count, has_remaining)
}

/// Returns true if primary disk has any non-busy target with evictable build units.
pub fn has_evictable_non_busy_on_primary(
    cfg: &DiskConfig,
    pool_root: &Path,
    proc_root: &Path,
    lane_dir: &Path,
    slot_held_fn: &dyn Fn(&Path) -> bool,
) -> bool {
    let targets = discover_targets(cfg, pool_root, slot_held_fn);
    let busy = probe_all_busy(&targets, proc_root, lane_dir);
    for (idx, t) in targets.iter().enumerate() {
        if t.disk != DiskRole::Primary || busy.contains_key(&idx) {
            continue;
        }
        if !collect_evictable_units(t).is_empty() {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Overflow routing, hysteresis return, and cold idle slot migration
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetDecision {
    /// Keep local `<slot>/target` (drop symlink if any).
    Local,
    /// Link `<slot>/target` -> `<primaryRoot>/<slot>/target`.
    Primary(PathBuf),
    /// Link `<slot>/target` -> `<overflowRoot>/<slot>/target`.
    Overflow(PathBuf),
    /// Return from overflow back to primary (or local) and delete old overflow dir.
    ReturnToPrimary {
        new_dest: Option<PathBuf>,
        old_overflow_dir: PathBuf,
    },
}

fn slot_has_warm_primary_cache(slot: &Path, cfg: &DiskConfig) -> bool {
    let name = slot.file_name().unwrap_or_default();
    let pdir = match &cfg.primary_root {
        Some(r) => r.join(name).join("target"),
        None => {
            let local = slot.join("target");
            if local.is_symlink() {
                return false;
            }
            local
        }
    };
    if !pdir.is_dir() {
        return false;
    }
    // Non-empty Cargo profile subdir means this slot already holds warm primary cache.
    for psub in profile_dirs(&pdir) {
        for sub in ["deps", "incremental", "build"] {
            let d = psub.join(sub);
            if fs::read_dir(&d)
                .into_iter()
                .flatten()
                .flatten()
                .next()
                .is_some()
            {
                return true;
            }
        }
    }
    false
}

/// Decide where `<slot>/target` should point during `pool::attach_target` / `pool::reset`.
pub fn decide_slot_target(
    slot: &Path,
    cfg: &DiskConfig,
    probe: &dyn SpaceProbe,
    pool_root: &Path,
    proc_root: &Path,
    lane_dir: &Path,
    slot_held_fn: &dyn Fn(&Path) -> bool,
) -> TargetDecision {
    let name = slot.file_name().unwrap_or_default();
    let primary_dest = cfg.primary_root.as_ref().map(|r| r.join(name).join("target"));
    let overflow_dest = cfg
        .overflow_root
        .as_ref()
        .map(|r| r.join(name).join("target"));

    let primary_space = probe.space(DiskRole::Primary, &cfg.primary_probe_path(pool_root));
    let link = slot.join("target");

    // Case 1: slot is currently linked to overflowRoot.
    if link.is_symlink() {
        if let Ok(cur) = fs::read_link(&link) {
            let cur_abs = if cur.is_absolute() {
                cur
            } else {
                slot.join(cur)
            };
            if path_is_under_overflow(&cur_abs, cfg) {
                if primary_space
                    .as_ref()
                    .is_some_and(|sp| cfg.primary_can_return(sp))
                {
                    return TargetDecision::ReturnToPrimary {
                        new_dest: primary_dest,
                        old_overflow_dir: cur_abs,
                    };
                }
                // Hysteresis: stay on overflow until primary drops <= returnPct.
                return TargetDecision::Overflow(cur_abs);
            }
        }
    }

    // Case 2: primary is at high watermark, overflowRoot is configured & has headroom,
    // this slot is empty on primary, AND primary has no evictable non-busy bytes.
    if let (Some(ov_dest), Some(ov_root), Some(p_sp)) =
        (&overflow_dest, &cfg.overflow_root, &primary_space)
    {
        if cfg.primary_is_high(p_sp) && !slot_has_warm_primary_cache(slot, cfg) {
            let ov_ok = probe
                .space(DiskRole::Overflow, ov_root)
                .is_none_or(|osp| !cfg.overflow_is_high(&osp));
            if ov_ok
                && !has_evictable_non_busy_on_primary(cfg, pool_root, proc_root, lane_dir, slot_held_fn)
            {
                return TargetDecision::Overflow(ov_dest.clone());
            }
        }
    }

    match primary_dest {
        Some(dest) => TargetDecision::Primary(dest),
        None => TargetDecision::Local,
    }
}

pub fn atomic_replace_symlink(dest: &Path, link: &Path) -> Result<(), String> {
    let parent = link.parent().ok_or("symlink has no parent directory")?;
    let tmp = parent.join(format!(
        ".target.tmp.{}.{}",
        std::process::id(),
        now_ns()
    ));
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(dest, &tmp).map_err(|e| e.to_string())?;
    if link.is_dir() && !link.is_symlink() {
        fs::remove_dir_all(link).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            e.to_string()
        })?;
    }
    fs::rename(&tmp, link).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

pub fn cleanup_overflow_slot_dir(old_overflow_target: &Path, overflow_root: Option<&Path>) {
    if old_overflow_target.is_dir() && !old_overflow_target.is_symlink() {
        let _ = fs::remove_dir_all(old_overflow_target);
    }
    if let (Some(slot_parent), Some(ov_root)) = (old_overflow_target.parent(), overflow_root) {
        if slot_parent != ov_root && slot_parent.starts_with(ov_root) {
            let _ = fs::remove_dir(slot_parent);
        }
    }
}

/// If `slot/target` points into `overflowRoot` and primary is `<= returnPct`, switch back to
/// primary (or local) and delete the overflow directory. Returns `true` if returned.
pub fn maybe_return_slot_to_primary(
    slot: &Path,
    cfg: &DiskConfig,
    probe: &dyn SpaceProbe,
    pool_root: &Path,
) -> Result<bool, String> {
    let link = slot.join("target");
    if !link.is_symlink() {
        return Ok(false);
    }
    let cur = fs::read_link(&link).map_err(|e| e.to_string())?;
    let cur_abs = if cur.is_absolute() {
        cur
    } else {
        slot.join(cur)
    };
    if !path_is_under_overflow(&cur_abs, cfg) {
        return Ok(false);
    }
    let Some(p_sp) = probe.space(DiskRole::Primary, &cfg.primary_probe_path(pool_root)) else {
        return Ok(false);
    };
    if !cfg.primary_can_return(&p_sp) {
        return Ok(false);
    }
    let name = slot.file_name().unwrap_or_default();
    if let Some(ref p_root) = cfg.primary_root {
        let dest = p_root.join(name).join("target");
        fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        atomic_replace_symlink(&dest, &link)?;
    } else {
        fs::remove_file(&link).map_err(|e| e.to_string())?;
    }
    cleanup_overflow_slot_dir(&cur_abs, cfg.overflow_root.as_deref());
    Ok(true)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<u64, String> {
    fs::create_dir_all(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    let mut copied = 0u64;
    for ent in fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))? {
        let ent = ent.map_err(|e| e.to_string())?;
        let sp = ent.path();
        let dp = dst.join(ent.file_name());
        let meta = fs::symlink_metadata(&sp).map_err(|e| e.to_string())?;
        if meta.is_symlink() {
            let link_to = fs::read_link(&sp).map_err(|e| e.to_string())?;
            let _ = fs::remove_file(&dp);
            std::os::unix::fs::symlink(&link_to, &dp).map_err(|e| e.to_string())?;
        } else if meta.is_dir() {
            copied = copied.saturating_add(copy_dir_recursive(&sp, &dp)?);
        } else if meta.is_file() {
            fs::copy(&sp, &dp).map_err(|e| format!("{} -> {}: {e}", sp.display(), dp.display()))?;
            let _ = fs::set_permissions(&dp, meta.permissions());
            if let (Ok(at), Ok(mt)) = (meta.accessed(), meta.modified()) {
                let times = fs::FileTimes::new().set_accessed(at).set_modified(mt);
                let _ = File::options()
                    .write(true)
                    .open(&dp)
                    .and_then(|f| f.set_times(times));
            }
            copied = copied.saturating_add(entry_disk_bytes(&meta));
        }
    }
    Ok(copied)
}

#[derive(Clone, Debug)]
pub struct MigrationRecord {
    pub slot: String,
    pub direction: &'static str,
    pub bytes: u64,
}

impl MigrationRecord {
    pub fn to_json(&self) -> Value {
        json!({
            "slot": self.slot,
            "direction": self.direction,
            "bytes": self.bytes,
        })
    }
}

fn migrate_coldest_idle_slot_to_overflow(
    targets: &[ManagedTarget],
    busy: &HashMap<usize, BusyReason>,
    cfg: &DiskConfig,
    dry_run: bool,
) -> Option<MigrationRecord> {
    let ov_root = cfg.overflow_root.as_ref()?;
    let mut best: Option<(i64, u64, &ManagedTarget)> = None;
    for (idx, t) in targets.iter().enumerate() {
        if t.disk != DiskRole::Primary || busy.contains_key(&idx) {
            continue;
        }
        if t.slot_dir.is_none() {
            continue;
        }
        let (bytes, atime, mtime) = subtree_stats(&t.target_dir);
        if bytes == 0 {
            continue;
        }
        let eff_atime = if atime > 0 { atime } else { mtime };
        match &best {
            None => best = Some((eff_atime, bytes, t)),
            Some((ba, _, _)) if eff_atime < *ba => best = Some((eff_atime, bytes, t)),
            _ => {}
        }
    }
    let (_, bytes, target) = best?;
    let slot_dir = target.slot_dir.as_ref()?;
    let slot_name = slot_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dst = ov_root.join(&slot_name).join("target");
    if !dry_run {
        if dst.exists() {
            let _ = fs::remove_dir_all(&dst);
        }
        if copy_dir_recursive(&target.target_dir, &dst).is_err() {
            let _ = fs::remove_dir_all(&dst);
            return None;
        }
        let link = slot_dir.join("target");
        if atomic_replace_symlink(&dst, &link).is_err() {
            let _ = fs::remove_dir_all(&dst);
            return None;
        }
        if target.target_dir != link {
            let _ = fs::remove_dir_all(&target.target_dir);
        }
    }
    Some(MigrationRecord {
        slot: slot_name,
        direction: "to_overflow",
        bytes,
    })
}

// ---------------------------------------------------------------------------
// Stale run occupying slot detection (>24h idle & >10 GiB; advisory only)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleSlotRun {
    pub run: String,
    pub name: String,
    pub slot: String,
    pub idle_hours: u64,
    pub bytes: u64,
}

impl StaleSlotRun {
    pub fn to_json(&self) -> Value {
        json!({
            "run": self.run,
            "name": self.name,
            "slot": self.slot,
            "idleHours": self.idle_hours,
            "bytes": self.bytes,
            "next": format!("delegate apply {} 或 delegate clean {} --force", self.run, self.run),
        })
    }
}

fn run_last_progress_epoch(run: &Path, meta: &Value) -> i64 {
    let mut best = meta["startedEpoch"].as_i64().unwrap_or(0);
    let summary_p = run.join("summary.json");
    if summary_p.is_file() {
        let sum = json(&summary_p);
        if let Some(ep) = iso_epoch(s(&sum, "finishedAt")) {
            best = best.max(ep);
        }
        if let Ok(m) = fs::metadata(&summary_p) {
            best = best.max(m.mtime());
        }
    }
    for fname in ["events.jsonl", "agent.log", "accept.log", "exit_code"] {
        if let Ok(m) = fs::metadata(run.join(fname)) {
            best = best.max(m.mtime());
        }
    }
    best
}

pub fn find_stale_slot_runs(
    runs: &[PathBuf],
    pool_root: &Path,
    cfg: &DiskConfig,
    now_epoch: i64,
) -> Vec<StaleSlotRun> {
    if cfg.stale_run_hours == 0 || cfg.stale_run_gib == 0 {
        return Vec::new();
    }
    let min_idle_secs = cfg.stale_run_secs();
    let min_bytes = cfg.stale_run_bytes();

    // Collect superseded parent IDs so only the latest round in a reply chain is reported.
    let mut parent_ids: HashSet<String> = HashSet::new();
    let mut metas: Vec<(PathBuf, Value)> = Vec::new();
    for run in runs {
        let mp = run.join("meta.json");
        if !mp.is_file() {
            continue;
        }
        let meta = json(&mp);
        let p = s(&meta, "parent");
        if !p.is_empty() {
            parent_ids.insert(p.to_string());
        }
        metas.push((run.clone(), meta));
    }

    let mut by_slot: BTreeMap<String, (u64, StaleSlotRun)> = BTreeMap::new();
    for (run, meta) in metas {
        if s(&meta, "mode") != "write" {
            continue;
        }
        if run.join(".applied").exists() || run.join(".applied-cleaned").exists() {
            continue;
        }
        let run_id = {
            let r = s(&meta, "run");
            if !r.is_empty() {
                r.to_string()
            } else {
                run.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }
        };
        if parent_ids.contains(&run_id) {
            continue;
        }
        let wt_path_str = s(&meta["worktree"], "path");
        if wt_path_str.is_empty() || wt_path_str.ends_with("#released") {
            continue;
        }
        let wt_path = Path::new(wt_path_str);
        if wt_path.parent() != Some(pool_root) && !b(&meta["worktree"], "pooled") {
            continue;
        }
        if !wt_path.is_dir() {
            continue;
        }
        let bytes = meta["worktreeBytes"]
            .as_u64()
            .or_else(|| meta["targetBytes"].as_u64())
            .unwrap_or(0);
        if bytes < min_bytes {
            continue;
        }
        let last_ep = run_last_progress_epoch(&run, &meta);
        if last_ep <= 0 {
            continue;
        }
        let idle_secs = now_epoch.saturating_sub(last_ep);
        if idle_secs < min_idle_secs {
            continue;
        }
        let slot_name = wt_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| wt_path_str.to_string());
        let name = {
            let n = s(&meta, "name");
            if n.is_empty() {
                run_id.clone()
            } else {
                n.to_string()
            }
        };
        let started_ns = meta["startedNs"].as_u64().unwrap_or(0);
        let item = StaleSlotRun {
            run: run_id,
            name,
            slot: slot_name.clone(),
            idle_hours: (idle_secs / 3600) as u64,
            bytes,
        };
        match by_slot.get(&slot_name) {
            Some((prev_ns, _)) if *prev_ns >= started_ns => {}
            _ => {
                by_slot.insert(slot_name, (started_ns, item));
            }
        }
    }
    let mut out: Vec<StaleSlotRun> = by_slot.into_values().map(|(_, item)| item).collect();
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| b.idle_hours.cmp(&a.idle_hours)));
    out
}

// ---------------------------------------------------------------------------
// Full prune report & diagnostic persistence
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct PruneReport {
    pub generated_at: String,
    pub generated_epoch: i64,
    pub dry_run: bool,
    pub primary_root: PathBuf,
    pub primary_before: Option<DiskSpace>,
    pub primary_after: Option<DiskSpace>,
    pub primary_level: DiskLevel,
    pub overflow_root: Option<PathBuf>,
    pub overflow_before: Option<DiskSpace>,
    pub overflow_after: Option<DiskSpace>,
    pub overflow_level: Option<DiskLevel>,
    pub overflow_slots: Vec<String>,
    pub busy_targets: Vec<(String, PathBuf, BusyReason)>,
    pub l0_bytes: u64,
    pub l0_items: usize,
    pub lru_bytes: u64,
    pub lru_items: usize,
    pub overflow_lru_bytes: u64,
    pub overflow_lru_items: usize,
    pub entries: Vec<DeletedEntry>,
    pub migrations: Vec<MigrationRecord>,
    pub stale_runs: Vec<StaleSlotRun>,
    pub diagnostic_path: Option<PathBuf>,
}

impl PruneReport {
    pub fn total_freed_bytes(&self) -> u64 {
        self.l0_bytes
            .saturating_add(self.lru_bytes)
            .saturating_add(self.overflow_lru_bytes)
    }

    pub fn total_deleted_items(&self) -> usize {
        self.l0_items + self.lru_items + self.overflow_lru_items
    }

    pub fn to_json(&self, cfg: &DiskConfig) -> Value {
        let vol_json = |root: &Path,
                        before: Option<DiskSpace>,
                        after: Option<DiskSpace>,
                        level: DiskLevel,
                        high_pct: u64,
                        target_pct: u64,
                        min_free: u64|
         -> Value {
            let a = after.or(before).unwrap_or_default();
            let b = before.unwrap_or(a);
            json!({
                "root": root.to_string_lossy(),
                "totalBytes": a.total_bytes,
                "usedBytes": a.used_bytes,
                "availBytes": a.avail_bytes,
                "usedPct": (a.used_pct() * 10.0).round() / 10.0,
                "beforeUsedBytes": b.used_bytes,
                "beforeAvailBytes": b.avail_bytes,
                "beforeUsedPct": (b.used_pct() * 10.0).round() / 10.0,
                "level": level.as_str(),
                "highPct": high_pct,
                "targetPct": target_pct,
                "minFreeBytes": min_free,
            })
        };
        let overflow_json = match (&self.overflow_root, self.overflow_level) {
            (Some(ov_root), Some(ov_level)) => vol_json(
                ov_root,
                self.overflow_before,
                self.overflow_after,
                ov_level,
                cfg.overflow_high_pct,
                cfg.overflow_target_pct,
                cfg.overflow_min_free_bytes(),
            ),
            _ => Value::Null,
        };
        let truncated = self.entries.len().saturating_sub(MAX_DIAG_ENTRIES);
        let entries_json: Vec<Value> = self
            .entries
            .iter()
            .take(MAX_DIAG_ENTRIES)
            .map(DeletedEntry::to_json)
            .collect();
        let busy_json: Vec<Value> = self
            .busy_targets
            .iter()
            .map(|(id, path, reason)| {
                json!({
                    "id": id,
                    "path": path.to_string_lossy(),
                    "reason": reason.kind_str(),
                    "detail": reason.detail(),
                })
            })
            .collect();
        json!({
            "generatedAt": self.generated_at,
            "generatedEpoch": self.generated_epoch,
            "dryRun": self.dry_run,
            "primary": vol_json(
                &self.primary_root,
                self.primary_before,
                self.primary_after,
                self.primary_level,
                cfg.high_pct,
                cfg.target_pct,
                cfg.min_free_bytes(),
            ),
            "overflow": overflow_json,
            "overflowSlots": self.overflow_slots,
            "overflowSlotCount": self.overflow_slots.len(),
            "busyTargets": busy_json,
            "deleted": {
                "l0Bytes": self.l0_bytes,
                "l0Items": self.l0_items,
                "lruBytes": self.lru_bytes,
                "lruItems": self.lru_items,
                "overflowLruBytes": self.overflow_lru_bytes,
                "overflowLruItems": self.overflow_lru_items,
                "totalBytes": self.total_freed_bytes(),
                "totalItems": self.total_deleted_items(),
                "entries": entries_json,
                "entriesTruncated": truncated,
            },
            "migrations": self.migrations.iter().map(MigrationRecord::to_json).collect::<Vec<_>>(),
            "staleRuns": self.stale_runs.iter().map(StaleSlotRun::to_json).collect::<Vec<_>>(),
            "diagnosticPath": self.diagnostic_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        })
    }
}

pub fn diagnostic_file(state_root: &Path) -> PathBuf {
    state_root.join("disk-health.json")
}

pub fn list_overflow_slots(pool_root: &Path, cfg: &DiskConfig) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(pool_root) else {
        return out;
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if !p.is_dir() {
            continue;
        }
        let link = p.join("target");
        if !link.is_symlink() {
            continue;
        }
        let Ok(dest) = fs::read_link(&link) else {
            continue;
        };
        let abs = if dest.is_absolute() {
            dest
        } else {
            p.join(dest)
        };
        if path_is_under_overflow(&abs, cfg) {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    out
}

/// Core testable prune engine.
#[allow(clippy::too_many_arguments)]
pub fn prune_with(
    cfg: &DiskConfig,
    pool_root: &Path,
    state_root: &Path,
    proc_root: &Path,
    lane_dir: &Path,
    runs: &[PathBuf],
    probe: &dyn SpaceProbe,
    slot_held_fn: &dyn Fn(&Path) -> bool,
    dry_run: bool,
    now_epoch: i64,
) -> Res<PruneReport> {
    let primary_probe = cfg.primary_probe_path(pool_root);
    let primary_before = probe.space(DiskRole::Primary, &primary_probe);
    let overflow_before = cfg
        .overflow_root
        .as_deref()
        .and_then(|r| probe.space(DiskRole::Overflow, r));

    let targets = discover_targets(cfg, pool_root, slot_held_fn);
    let busy = probe_all_busy(&targets, proc_root, lane_dir);

    let mut entries: Vec<DeletedEntry> = Vec::new();
    let mut l0_primary_bytes = 0u64;
    let mut l0_overflow_bytes = 0u64;
    let mut l0_items = 0usize;

    // Step 1: Zero-cost cleanup (L0) on every non-busy target.
    for (idx, t) in targets.iter().enumerate() {
        if busy.contains_key(&idx) {
            continue;
        }
        let stats = zero_cost_cleanup(
            &t.target_dir,
            t.repo_dir.as_deref(),
            t.is_released_slot,
            &t.id,
            dry_run,
        );
        let total = stats.total_bytes();
        match t.disk {
            DiskRole::Primary => l0_primary_bytes = l0_primary_bytes.saturating_add(total),
            DiskRole::Overflow => l0_overflow_bytes = l0_overflow_bytes.saturating_add(total),
        }
        l0_items += stats.entries.len();
        entries.extend(stats.entries);
    }

    let l0_bytes = l0_primary_bytes.saturating_add(l0_overflow_bytes);

    // Step 2: Primary disk watermark -> global LRU across non-busy primary targets.
    let mut primary_cur = primary_before.map(|sp| sp.after_freed(l0_primary_bytes));
    let mut lru_bytes = 0u64;
    let mut lru_items = 0usize;
    let mut primary_has_more_evictable = true;

    if let Some(sp) = primary_cur {
        let need = cfg.primary_bytes_to_free(&sp);
        if need > 0 {
            let (freed, count, has_more) = evict_units_global_lru(
                &targets,
                &busy,
                DiskRole::Primary,
                need,
                dry_run,
                &mut entries,
            );
            lru_bytes = freed;
            lru_items = count;
            primary_has_more_evictable = has_more;
            primary_cur = Some(sp.after_freed(freed));
        }
    }

    // Step 3: Overflow disk watermark -> global LRU across non-busy overflow targets.
    let mut overflow_cur = overflow_before.map(|sp| sp.after_freed(l0_overflow_bytes));
    let mut overflow_lru_bytes = 0u64;
    let mut overflow_lru_items = 0usize;

    if let Some(sp) = overflow_cur {
        let need = cfg.overflow_bytes_to_free(&sp);
        if need > 0 {
            let (freed, count, _) = evict_units_global_lru(
                &targets,
                &busy,
                DiskRole::Overflow,
                need,
                dry_run,
                &mut entries,
            );
            overflow_lru_bytes = freed;
            overflow_lru_items = count;
            overflow_cur = Some(sp.after_freed(freed));
        }
    }

    // Step 4: Cold idle slot migration (ONLY when primary >= migratePct AND nothing evictable left).
    let mut migrations = Vec::new();
    if let (Some(p_sp), Some(ov_sp)) = (primary_cur, overflow_cur) {
        if p_sp.used_pct() >= cfg.migrate_pct as f64
            && !primary_has_more_evictable
            && !cfg.overflow_is_high(&ov_sp)
        {
            if let Some(mig) = migrate_coldest_idle_slot_to_overflow(&targets, &busy, cfg, dry_run) {
                primary_cur = Some(p_sp.after_freed(mig.bytes));
                overflow_cur = Some(ov_sp.after_added(mig.bytes));
                migrations.push(mig);
            }
        }
    }

    // Step 5: If primary dropped <= returnPct, return any released non-busy overflow slots.
    if primary_cur.is_some_and(|sp| cfg.primary_can_return(&sp)) {
        for (idx, t) in targets.iter().enumerate() {
            if t.disk != DiskRole::Overflow || busy.contains_key(&idx) || !t.is_released_slot {
                continue;
            }
            let Some(ref slot_dir) = t.slot_dir else {
                continue;
            };
            let bytes = dir_disk_bytes(&t.target_dir);
            let slot_name = slot_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !dry_run {
                let _ = maybe_return_slot_to_primary(slot_dir, cfg, probe, pool_root);
            }
            if let Some(ov_sp) = overflow_cur {
                overflow_cur = Some(ov_sp.after_freed(bytes));
            }
            migrations.push(MigrationRecord {
                slot: slot_name,
                direction: "to_primary",
                bytes,
            });
        }
    }

    let overflow_slots = list_overflow_slots(pool_root, cfg);
    let stale_runs = find_stale_slot_runs(runs, pool_root, cfg, now_epoch);

    let mut busy_targets: Vec<(String, PathBuf, BusyReason)> = targets
        .iter()
        .enumerate()
        .filter_map(|(idx, t)| {
            busy.get(&idx)
                .map(|r| (t.id.clone(), t.target_dir.clone(), r.clone()))
        })
        .collect();
    busy_targets.sort_by(|a, b| a.0.cmp(&b.0));

    let primary_level = primary_cur
        .map(|sp| cfg.primary_level(&sp))
        .unwrap_or(DiskLevel::Ok);
    let overflow_level = overflow_cur.map(|sp| cfg.overflow_level(&sp));

    let diag_path = diagnostic_file(state_root);
    let mut report = PruneReport {
        generated_at: iso(),
        generated_epoch: now_epoch,
        dry_run,
        primary_root: primary_probe,
        primary_before,
        primary_after: primary_cur,
        primary_level,
        overflow_root: cfg.overflow_root.clone(),
        overflow_before,
        overflow_after: overflow_cur,
        overflow_level,
        overflow_slots,
        busy_targets,
        l0_bytes,
        l0_items,
        lru_bytes,
        lru_items,
        overflow_lru_bytes,
        overflow_lru_items,
        entries,
        migrations,
        stale_runs,
        diagnostic_path: (!dry_run).then(|| diag_path.clone()),
    };

    if !dry_run {
        if let Some(parent) = diag_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let doc = report.to_json(cfg);
        atomic_write(
            &diag_path,
            format!("{}\n", serde_json::to_string_pretty(&doc).unwrap()),
        )?;
        report.diagnostic_path = Some(diag_path);
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Strictly read-only, zero-lock health summary (status / snapshot / Stop hook)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct VolumeHealth {
    pub root: PathBuf,
    pub space: DiskSpace,
    pub level: DiskLevel,
    pub high_pct: u64,
    pub target_pct: u64,
    pub min_free_bytes: u64,
}

impl VolumeHealth {
    pub fn to_json(&self) -> Value {
        json!({
            "root": self.root.to_string_lossy(),
            "totalBytes": self.space.total_bytes,
            "usedBytes": self.space.used_bytes,
            "availBytes": self.space.avail_bytes,
            "usedPct": (self.space.used_pct() * 10.0).round() / 10.0,
            "level": self.level.as_str(),
            "highPct": self.high_pct,
            "targetPct": self.target_pct,
            "minFreeBytes": self.min_free_bytes,
        })
    }
}

#[derive(Clone, Debug)]
pub struct DiskHealthSummary {
    pub configured: bool,
    pub primary: Option<VolumeHealth>,
    pub overflow: Option<VolumeHealth>,
    pub total_slots: usize,
    pub overflow_slots: Vec<String>,
    pub stale_runs: Vec<StaleSlotRun>,
    pub last_prune_at: Option<String>,
    pub last_prune_freed_bytes: Option<u64>,
    pub config_error: Option<String>,
}

impl DiskHealthSummary {
    pub fn overflow_slot_count(&self) -> usize {
        self.overflow_slots.len()
    }

    pub fn worst_level(&self) -> DiskLevel {
        let p = self.primary.as_ref().map_or(DiskLevel::Ok, |v| v.level);
        let o = self.overflow.as_ref().map_or(DiskLevel::Ok, |v| v.level);
        p.max(o)
    }

    /// True when `status` human output should display a disk line.
    pub fn should_show_in_status(&self) -> bool {
        self.configured
            || self.config_error.is_some()
            || self.overflow_slot_count() > 0
            || !self.stale_runs.is_empty()
            || self.worst_level() >= DiskLevel::Warn
            || has_fake_space_override()
    }

    /// True when Stop hook should surface a disk/stale-slot reminder.
    pub fn should_warn_stop(&self) -> bool {
        self.config_error.is_some()
            || self.worst_level() >= DiskLevel::High
            || self.overflow_slot_count() > 0
            || !self.stale_runs.is_empty()
    }

    pub fn format_compact_line(&self) -> Option<String> {
        if let Some(ref err) = self.config_error {
            return Some(format!("⚠ 磁盘配置错误：{err}"));
        }
        let mut parts = Vec::new();
        if let Some(ref p) = self.primary {
            let tag = match p.level {
                DiskLevel::Critical => "，临界",
                DiskLevel::High => "，超水位",
                DiskLevel::Warn => "，注意",
                DiskLevel::Ok => "",
            };
            parts.push(format!(
                "主盘 {:.0}%（余 {}{tag}）",
                p.space.used_pct(),
                format_bytes(p.space.avail_bytes),
            ));
        }
        if let Some(ref o) = self.overflow {
            let tag = match o.level {
                DiskLevel::Critical => "，临界",
                DiskLevel::High => "，超水位",
                DiskLevel::Warn => "，注意",
                DiskLevel::Ok => "",
            };
            parts.push(format!(
                "溢出盘 {:.0}%（余 {}{tag}）",
                o.space.used_pct(),
                format_bytes(o.space.avail_bytes),
            ));
        }
        if self.overflow.is_some() || !self.overflow_slots.is_empty() {
            parts.push(format!("溢出槽 {}", self.overflow_slots.len()));
        }
        if parts.is_empty() {
            return None;
        }
        let prefix = match self.worst_level() {
            DiskLevel::Critical | DiskLevel::High => "⚠ 磁盘：",
            DiskLevel::Warn => "提示：磁盘：",
            DiskLevel::Ok => "磁盘：",
        };
        Some(format!("{prefix}{}", parts.join(" · ")))
    }

    pub fn format_stale_lines(&self) -> Vec<String> {
        self.stale_runs
            .iter()
            .map(|r| {
                format!(
                    "⚠ 占槽提醒：{}（{}）已 {}h 无进展且占 {}（不自动删 run；请检查后 delegate apply {} 或 delegate clean {} --force）",
                    r.name,
                    r.slot,
                    r.idle_hours,
                    format_bytes(r.bytes),
                    r.run,
                    r.run,
                )
            })
            .collect()
    }

    pub fn format_stop_reminder(&self) -> Option<String> {
        if !self.should_warn_stop() {
            return None;
        }
        let mut segs = Vec::new();
        if let Some(line) = self.format_compact_line() {
            segs.push(line);
        }
        if !self.stale_runs.is_empty() {
            let preview: Vec<String> = self
                .stale_runs
                .iter()
                .take(2)
                .map(|r| {
                    format!(
                        "{}({} {}h/{})",
                        r.name,
                        r.slot,
                        r.idle_hours,
                        format_bytes(r.bytes)
                    )
                })
                .collect();
            let extra = if self.stale_runs.len() > 2 {
                format!(" 等{}个", self.stale_runs.len())
            } else {
                String::new()
            };
            segs.push(format!(
                "占槽>24h：{}{}（不自动删 run，请检查 apply/clean）",
                preview.join("、"),
                extra
            ));
        }
        if segs.is_empty() {
            None
        } else {
            Some(segs.join("；"))
        }
    }

    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "configured": self.configured,
            "level": self.worst_level().as_str(),
            "primary": self.primary.as_ref().map(VolumeHealth::to_json),
            "overflow": self.overflow.as_ref().map(VolumeHealth::to_json),
            "totalSlots": self.total_slots,
            "overflowSlots": self.overflow_slots,
            "overflowSlotCount": self.overflow_slots.len(),
            "staleRuns": self.stale_runs.iter().map(StaleSlotRun::to_json).collect::<Vec<_>>(),
            "lastPruneAt": self.last_prune_at,
            "lastPruneFreedBytes": self.last_prune_freed_bytes,
        });
        if let Some(ref err) = self.config_error {
            obj["configError"] = json!(err);
        }
        obj
    }
}

pub fn inspect_readonly_with(
    cfg: &DiskConfig,
    pool_root: &Path,
    state_root: &Path,
    runs: &[PathBuf],
    probe: &dyn SpaceProbe,
    now_epoch: i64,
) -> DiskHealthSummary {
    let p_root = cfg.primary_probe_path(pool_root);
    let primary = probe.space(DiskRole::Primary, &p_root).map(|space| VolumeHealth {
        root: p_root,
        level: cfg.primary_level(&space),
        space,
        high_pct: cfg.high_pct,
        target_pct: cfg.target_pct,
        min_free_bytes: cfg.min_free_bytes(),
    });
    let overflow = cfg.overflow_root.as_ref().and_then(|ov_root| {
        probe
            .space(DiskRole::Overflow, ov_root)
            .map(|space| VolumeHealth {
                root: ov_root.clone(),
                level: cfg.overflow_level(&space),
                space,
                high_pct: cfg.overflow_high_pct,
                target_pct: cfg.overflow_target_pct,
                min_free_bytes: cfg.overflow_min_free_bytes(),
            })
    });
    let total_slots = fs::read_dir(pool_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.path().is_dir()
                && !e
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with('.') || n.ends_with(".owner"))
        })
        .count();
    let overflow_slots = list_overflow_slots(pool_root, cfg);
    let stale_runs = find_stale_slot_runs(runs, pool_root, cfg, now_epoch);

    let diag = json(diagnostic_file(state_root));
    let last_prune_at = {
        let v = s(&diag, "generatedAt");
        (!v.is_empty()).then(|| v.to_string())
    };
    let last_prune_freed_bytes = diag["deleted"]["totalBytes"].as_u64();

    DiskHealthSummary {
        configured: cfg.configured,
        primary,
        overflow,
        total_slots,
        overflow_slots,
        stale_runs,
        last_prune_at,
        last_prune_freed_bytes,
        config_error: None,
    }
}

pub fn inspect_readonly(state_root: &Path, cache_root: &Path, now_epoch: i64) -> DiskHealthSummary {
    match load_user_config_strict() {
        Ok(cfg) => {
            let pool_root = cache_root.join("pool");
            let runs = collect_all_run_dirs_readonly(state_root);
            inspect_readonly_with(&cfg, &pool_root, state_root, &runs, &RealSpaceProbe, now_epoch)
        }
        Err(err) => DiskHealthSummary {
            configured: true,
            primary: None,
            overflow: None,
            total_slots: 0,
            overflow_slots: Vec::new(),
            stale_runs: Vec::new(),
            last_prune_at: None,
            last_prune_freed_bytes: None,
            config_error: Some(err),
        },
    }
}

fn collect_all_run_dirs_readonly(state_root: &Path) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut roots = vec![state_root.join("runs")];
    if let Some(old) = legacy_runs_root() {
        roots.push(old);
    }
    for root in roots {
        let Ok(rd) = fs::read_dir(root) else {
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.join("meta.json").is_file() && seen.insert(p.clone()) {
                out.push(p);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// CLI entrypoint: `delegate disk [status|prune] [--dry-run] [--json]`
// ---------------------------------------------------------------------------

pub fn command(args: &[String]) -> Res<i32> {
    let (sub, rest) = match args.first().map(String::as_str) {
        None => ("status", &args[0..]),
        Some(a) if a.starts_with('-') => ("status", args),
        Some("status") => ("status", &args[1..]),
        Some("prune") => ("prune", &args[1..]),
        Some(other) => {
            return Err(format!(
                "unknown disk subcommand '{other}' (expected: prune, status)"
            ));
        }
    };

    let mut dry_run = false;
    let mut json_mode = crate::output::json_enabled();
    for arg in rest {
        match arg.as_str() {
            "--dry-run" if sub == "prune" => dry_run = true,
            "--json" => json_mode = true,
            other => {
                let valid: &[&str] = if sub == "prune" {
                    &["--dry-run", "--json"]
                } else {
                    &["--json"]
                };
                return Err(unrecognized_arg(other, valid, "disk"));
            }
        }
    }

    let cfg = load_config()?;
    let state = state_dir();
    let cache = cache_dir();
    let pool_root = cache.join("pool");
    let now = epoch() as i64;

    match sub {
        "status" => {
            let runs = collect_all_run_dirs_readonly(&state);
            let summary =
                inspect_readonly_with(&cfg, &pool_root, &state, &runs, &RealSpaceProbe, now);
            if json_mode {
                println!("{}", summary.to_json());
            } else {
                if let Some(line) = summary.format_compact_line() {
                    println!("{line}");
                } else {
                    println!("磁盘：无法读取文件系统水位");
                }
                if !summary.overflow_slots.is_empty() {
                    println!("溢出槽位：{}", summary.overflow_slots.join("、"));
                }
                for line in summary.format_stale_lines() {
                    println!("{line}");
                }
                if let Some(ref at) = summary.last_prune_at {
                    let freed = summary.last_prune_freed_bytes.unwrap_or(0);
                    println!(
                        "上次清理：{at}（释放 {}；诊断 {}）",
                        format_bytes(freed),
                        diagnostic_file(&state).display()
                    );
                }
            }
            Ok(0)
        }
        "prune" => {
            let lane_dir = state.join("lane");
            let runs = collect_all_run_dirs_readonly(&state);
            let report = prune_with(
                &cfg,
                &pool_root,
                &state,
                Path::new("/proc"),
                &lane_dir,
                &runs,
                &RealSpaceProbe,
                &crate::pool::is_slot_held,
                dry_run,
                now,
            )?;
            if json_mode {
                println!("{}", report.to_json(&cfg));
            } else {
                let tag = if dry_run { "（预演 dry-run）" } else { "" };
                if let (Some(b), Some(a)) = (report.primary_before, report.primary_after) {
                    println!(
                        "主盘水位{tag}：{:.1}%（余 {}）→ {:.1}%（余 {}）",
                        b.used_pct(),
                        format_bytes(b.avail_bytes),
                        a.used_pct(),
                        format_bytes(a.avail_bytes),
                    );
                }
                if let (Some(b), Some(a)) = (report.overflow_before, report.overflow_after) {
                    println!(
                        "溢出盘水位{tag}：{:.1}%（余 {}）→ {:.1}%（余 {}）· 溢出槽 {}",
                        b.used_pct(),
                        format_bytes(b.avail_bytes),
                        a.used_pct(),
                        format_bytes(a.avail_bytes),
                        report.overflow_slots.len(),
                    );
                }
                println!(
                    "已清理{tag}：零代价 {}（{} 项）· 主盘 LRU {}（{} 项）· 溢出盘 LRU {}（{} 项）· 合计 {}",
                    format_bytes(report.l0_bytes),
                    report.l0_items,
                    format_bytes(report.lru_bytes),
                    report.lru_items,
                    format_bytes(report.overflow_lru_bytes),
                    report.overflow_lru_items,
                    format_bytes(report.total_freed_bytes()),
                );
                if !report.busy_targets.is_empty() {
                    let list: Vec<String> = report
                        .busy_targets
                        .iter()
                        .map(|(id, _, r)| format!("{id}[{}]", r.kind_str()))
                        .collect();
                    println!("跳过在用构建根（{}）：{}", list.len(), list.join("、"));
                }
                for m in &report.migrations {
                    println!(
                        "跨盘迁移{tag}：{} {}（{}）",
                        m.slot,
                        m.direction,
                        format_bytes(m.bytes)
                    );
                }
                for r in &report.stale_runs {
                    println!(
                        "⚠ 占槽提醒：{}（{}）已 {}h 无进展且占 {}（不自动删 run；请检查后 delegate apply {} 或 delegate clean {} --force）",
                        r.name,
                        r.slot,
                        r.idle_hours,
                        format_bytes(r.bytes),
                        r.run,
                        r.run,
                    );
                }
                if let Some(ref p) = report.diagnostic_path {
                    println!("诊断文件：{}", p.display());
                }
            }
            Ok(0)
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{attach_target_with, release_with};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(prefix: &str) -> PathBuf {
        let n = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    struct SimProbe {
        primary: DiskSpace,
        overflow: DiskSpace,
    }

    impl SpaceProbe for SimProbe {
        fn space(&self, role: DiskRole, _path: &Path) -> Option<DiskSpace> {
            Some(match role {
                DiskRole::Primary => self.primary,
                DiskRole::Overflow => self.overflow,
            })
        }
    }

    fn write_sized(path: &Path, size: usize, age_secs: u64, executable: bool) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, vec![0x42u8; size]).unwrap();
        if executable {
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        let times = fs::FileTimes::new().set_accessed(when).set_modified(when);
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(times)
            .unwrap();
    }

    #[test]
    fn config_validation_rejects_repo_disk_and_invalid_watermarks() {
        let valid = json!({
            "primaryRoot": "/path/to/primary",
            "overflowRoot": "/path/to/overflow",
            "extraTargets": ["/path/to/repo/target"],
            "warnPct": 75,
            "highPct": 85,
            "minFreeGib": 50,
            "targetPct": 70,
            "migratePct": 90,
            "returnPct": 60,
            "overflowHighPct": 90,
            "overflowTargetPct": 80,
            "staleRunHours": 24,
            "staleRunGib": 10
        });
        assert!(validate_config(Some(&valid), false).is_ok());
        assert!(validate_config(Some(&valid), true).is_err());

        let bad_target = json!({"highPct": 80, "targetPct": 85});
        let err = load_config_from(Some(&bad_target)).unwrap_err();
        assert!(err.contains("disk.targetPct"), "err={err}");

        let bad_return = json!({"highPct": 80, "returnPct": 80});
        let err = load_config_from(Some(&bad_return)).unwrap_err();
        assert!(err.contains("disk.returnPct"), "err={err}");

        let unknown = json!({"bogusField": 1});
        let err = load_config_from(Some(&unknown)).unwrap_err();
        assert!(err.contains("bogusField"), "err={err}");

        let bad_pct = json!({"highPct": 120});
        let err = load_config_from(Some(&bad_pct)).unwrap_err();
        assert!(err.contains("disk.highPct"), "err={err}");
    }

    #[test]
    fn zero_cost_cleanup_prunes_sessions_stale_own_hashes_and_released_execs() {
        let base = temp_dir("delegate-disk-l0");
        let slot = base.join("demo-1");
        let target = slot.join("target");
        fs::create_dir_all(&slot).unwrap();
        // Cargo.lock marks `my_app` as local (no source) and `serde` as 3p (with source).
        fs::write(
            slot.join("Cargo.lock"),
            "[[package]]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        )
        .unwrap();

        let inc = target.join("debug/incremental/my_app-abc");
        write_sized(&inc.join("s-hnu2a-1x-svh/query-cache.bin"), 16384, 300, false);
        write_sized(&inc.join("s-hnu2b-2y-svh/query-cache.bin"), 16384, 100, false);
        write_sized(&inc.join("s-hnu2c-3z-working/query-cache.bin"), 16384, 50, false);
        // Locked old session must be preserved even if superseded.
        write_sized(&inc.join("s-hnu20-0w-svh/query-cache.bin"), 16384, 500, false);
        let lock_file = locked_file(&inc.join("s-hnu20-0w.lock"), "").unwrap();

        let deps = target.join("debug/deps");
        // Own crate old vs new rlib hashes.
        write_sized(&deps.join("libmy_app-1111111111111111.rlib"), 32768, 400, false);
        write_sized(&deps.join("libmy_app-2222222222222222.rlib"), 32768, 100, false);
        // Third-party crate with two hashes must BOTH be kept by L0.
        write_sized(&deps.join("libserde-aaaaaaaaaaaaaaaa.rlib"), 32768, 400, false);
        write_sized(&deps.join("libserde-bbbbbbbbbbbbbbbb.rlib"), 32768, 100, false);
        // Own test executables (old + new).
        write_sized(&deps.join("my_app-1111111111111111"), 32768, 400, true);
        write_sized(&deps.join("my_app-2222222222222222"), 32768, 100, true);

        // First run as non-released slot: keeps newest exec, drops old exec + old own rlib + old unlocked session.
        let stats = zero_cost_cleanup(&target, Some(&slot), false, "slot:demo-1", false);
        assert!(stats.session_bytes >= 16384);
        assert!(stats.stale_hash_bytes >= 65536);
        assert_eq!(stats.released_exec_bytes, 0);
        assert!(!inc.join("s-hnu2a-1x-svh").exists());
        assert!(inc.join("s-hnu2b-2y-svh").exists());
        assert!(inc.join("s-hnu2c-3z-working").exists());
        assert!(inc.join("s-hnu20-0w-svh").exists(), "locked session must be skipped");
        assert!(!deps.join("libmy_app-1111111111111111.rlib").exists());
        assert!(deps.join("libmy_app-2222222222222222.rlib").exists());
        assert!(deps.join("libserde-aaaaaaaaaaaaaaaa.rlib").exists());
        assert!(deps.join("libserde-bbbbbbbbbbbbbbbb.rlib").exists());
        assert!(!deps.join("my_app-1111111111111111").exists());
        assert!(deps.join("my_app-2222222222222222").exists());

        // Now run as released slot: drops remaining test exec too while keeping rlib warm.
        let rel_stats = zero_cost_cleanup(&target, Some(&slot), true, "slot:demo-1", false);
        assert!(rel_stats.released_exec_bytes >= 32768);
        assert!(!deps.join("my_app-2222222222222222").exists());
        assert!(deps.join("libmy_app-2222222222222222.rlib").exists());

        drop(lock_file);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn three_liveness_signals_each_protect_target_independently() {
        let base = temp_dir("delegate-disk-live");
        let pool = base.join("pool");
        let state = base.join("state");
        let lane_dir = state.join("lane");
        let runs_dir = state.join("runs");
        fs::create_dir_all(&pool).unwrap();
        fs::create_dir_all(&lane_dir).unwrap();
        fs::create_dir_all(&runs_dir).unwrap();

        let make_slot = |name: &str| {
            let slot = pool.join(name);
            let target = slot.join("target");
            write_sized(
                &target.join("debug/deps/libpkg-1111111111111111.rlib"),
                65536,
                1000,
                false,
            );
            (slot, target)
        };

        // Slot A: protected by Cargo flock on `.cargo-build-lock`.
        let (_slot_a, target_a) = make_slot("repo-1");
        let cargo_lock = locked_file(&target_a.join("debug/.cargo-build-lock"), "").unwrap();

        // Slot B: protected by `/proc/<pid>/{cwd,fd,exe}` pointing into `target_b`.
        let (_slot_b, target_b) = make_slot("repo-2");
        let open_fd = File::open(target_b.join("debug/deps/libpkg-1111111111111111.rlib")).unwrap();
        // Verify real Linux `/proc/<self>/fd/*` detection works with an open file handle.
        let real_proc_targets = discover_targets(
            &DiskConfig {
                min_free_gib: 0,
                ..Default::default()
            },
            &pool,
            &|_| false,
        );
        let real_proc_hits = probe_proc_refs_in(Path::new("/proc"), &real_proc_targets, None);
        let b_idx = real_proc_targets
            .iter()
            .position(|t| t.id == "slot:repo-2")
            .unwrap();
        assert_eq!(
            real_proc_hits.get(&b_idx).map(BusyReason::kind_str),
            Some("proc_ref"),
            "real /proc/<pid>/fd scan must detect open handle inside target"
        );
        drop(open_fd);

        // Synthetic proc_root exercising cwd/fd/exe for full prune_with without spawning a child
        // process that could inherit non-CLOEXEC pipes from parallel tests.
        let fake_proc = base.join("proc");
        fs::create_dir_all(fake_proc.join("42424/fd")).unwrap();
        std::os::unix::fs::symlink(&target_b, fake_proc.join("42424/cwd")).unwrap();

        // Slot C: protected by an active lane ticket (`running: true`) pointing at run -> slot_c.
        let (slot_c, _target_c) = make_slot("repo-3");
        let run_c = runs_dir.join("20261126-120000-taskc");
        fs::create_dir_all(&run_c).unwrap();
        write_json(
            run_c.join("meta.json"),
            &json!({"run": "20261126-120000-taskc", "worktree": {"path": slot_c}}),
        )
        .unwrap();
        let ticket_c = locked_file(
            &lane_dir.join("00000000000000000001-99999.ticket"),
            &json!({"pid": 99999, "label": "cargo test", "run": run_c, "running": true})
                .to_string(),
        )
        .unwrap();

        // Slot D: idle (no flock, no proc, no lane ticket).
        let (_slot_d, target_d) = make_slot("repo-4");

        let cfg = DiskConfig {
            min_free_gib: 0,
            ..Default::default()
        };
        let targets = discover_targets(&cfg, &pool, &|_| false);
        let busy = probe_all_busy(&targets, &fake_proc, &lane_dir);

        let reason_for = |id: &str| -> Option<&str> {
            let idx = targets.iter().position(|t| t.id == id).unwrap();
            busy.get(&idx).map(BusyReason::kind_str)
        };
        assert_eq!(reason_for("slot:repo-1"), Some("cargo_flock"));
        assert_eq!(reason_for("slot:repo-2"), Some("proc_ref"));
        assert_eq!(reason_for("slot:repo-3"), Some("lane_ticket"));
        assert_eq!(reason_for("slot:repo-4"), None);

        // Also verify `/proc/<pid>/exe` (including Linux ` (deleted)` suffix) triggers Signal B.
        let fake_proc_exe = base.join("proc-exe");
        fs::create_dir_all(fake_proc_exe.join("42425/fd")).unwrap();
        std::os::unix::fs::symlink(
            format!("{}/debug/deps/test_bin (deleted)", target_d.display()),
            fake_proc_exe.join("42425/exe"),
        )
        .unwrap();
        let exe_hits = probe_proc_refs_in(&fake_proc_exe, &targets, None);
        let d_idx = targets.iter().position(|t| t.id == "slot:repo-4").unwrap();
        assert_eq!(exe_hits.get(&d_idx).map(BusyReason::kind_str), Some("proc_ref"));

        // High watermark prune must skip A/B/C and evict ONLY D.
        let probe = SimProbe {
            primary: DiskSpace::from_total_avail(1_000_000, 100_000), // 90% used
            overflow: DiskSpace::from_total_avail(1_000_000, 800_000),
        };
        let report = prune_with(
            &cfg,
            &pool,
            &state,
            &fake_proc,
            &lane_dir,
            &[],
            &probe,
            &|_| false,
            false,
            1_795_000_000,
        )
        .unwrap();

        assert_eq!(report.busy_targets.len(), 3);
        assert!(target_a.join("debug/deps/libpkg-1111111111111111.rlib").exists());
        assert!(target_b.join("debug/deps/libpkg-1111111111111111.rlib").exists());
        assert!(slot_c.join("target/debug/deps/libpkg-1111111111111111.rlib").exists());
        assert!(!target_d.join("debug/deps/libpkg-1111111111111111.rlib").exists());

        drop(cargo_lock);
        drop(ticket_c);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn high_watermark_global_lru_evicts_oldest_to_target_and_writes_diagnostic() {
        let base = temp_dir("delegate-disk-lru");
        let pool = base.join("pool");
        let primary_root = base.join("primary");
        let extra_target = base.join("main-repo/target");
        let state = base.join("state");
        let lane_dir = state.join("lane");
        fs::create_dir_all(&pool).unwrap();
        fs::create_dir_all(&primary_root).unwrap();
        fs::create_dir_all(&lane_dir).unwrap();

        // Pool slot 1 (oldest non-busy: 200 KiB, age 5000s)
        let s1 = pool.join("app-1");
        fs::create_dir_all(&s1).unwrap();
        let s1_t = primary_root.join("app-1/target");
        write_sized(&s1_t.join("debug/deps/liboldest-1111111111111111.rlib"), 200 << 10, 5000, false);
        std::os::unix::fs::symlink(&s1_t, s1.join("target")).unwrap();

        // Extra target (middle age: 200 KiB, age 2000s)
        write_sized(
            &extra_target.join("debug/deps/libmiddle-2222222222222222.rlib"),
            200 << 10,
            2000,
            false,
        );

        // Pool slot 2 (newest: 200 KiB, age 100s — should survive once targetPct reached)
        let s2 = pool.join("app-2");
        fs::create_dir_all(&s2).unwrap();
        let s2_t = primary_root.join("app-2/target");
        write_sized(&s2_t.join("debug/deps/libnewest-3333333333333333.rlib"), 200 << 10, 100, false);
        std::os::unix::fs::symlink(&s2_t, s2.join("target")).unwrap();

        // Pool slot 3 (very old BUT BUSY — must never be evicted!)
        let s3 = pool.join("app-3");
        fs::create_dir_all(&s3).unwrap();
        let s3_t = primary_root.join("app-3/target");
        write_sized(&s3_t.join("debug/deps/libbusy-4444444444444444.rlib"), 200 << 10, 99999, false);
        std::os::unix::fs::symlink(&s3_t, s3.join("target")).unwrap();
        let busy_lock = locked_file(&s3_t.join(".cargo-lock"), "").unwrap();

        let cfg = DiskConfig {
            primary_root: Some(primary_root),
            extra_targets: vec![extra_target.clone()],
            high_pct: 85,
            target_pct: 70,
            min_free_gib: 0,
            ..Default::default()
        };
        // Total = 2 MiB, Used = 1.8 MiB (90%), target 70% = 1.4 MiB -> need to free 400 KiB.
        let total = 2 * 1024 * 1024;
        let avail = total - (1800 * 1024);
        let probe = SimProbe {
            primary: DiskSpace::from_total_avail(total, avail),
            overflow: DiskSpace::from_total_avail(total, total),
        };

        // Dry-run must NOT delete anything and must NOT write disk-health.json.
        let dry_report = prune_with(
            &cfg,
            &pool,
            &state,
            Path::new("/nonexistent"),
            &lane_dir,
            &[],
            &probe,
            &|_| false,
            true,
            1_795_000_000,
        )
        .unwrap();
        assert!(dry_report.lru_bytes >= 350 * 1024);
        assert!(s1_t.join("debug/deps/liboldest-1111111111111111.rlib").exists());
        assert!(!diagnostic_file(&state).exists());

        // Real run evicts oldest (slot 1) + middle (extra target), stops at <=70%, keeps newest (slot 2) & busy (slot 3).
        let report = prune_with(
            &cfg,
            &pool,
            &state,
            Path::new("/nonexistent"),
            &lane_dir,
            &[],
            &probe,
            &|_| false,
            false,
            1_795_000_000,
        )
        .unwrap();
        assert!(!s1_t.join("debug/deps/liboldest-1111111111111111.rlib").exists());
        assert!(!extra_target.join("debug/deps/libmiddle-2222222222222222.rlib").exists());
        assert!(s2_t.join("debug/deps/libnewest-3333333333333333.rlib").exists());
        assert!(s3_t.join("debug/deps/libbusy-4444444444444444.rlib").exists());
        assert!(
            report.primary_after.unwrap().used_pct() <= 70.5,
            "after used_pct: {}",
            report.primary_after.unwrap().used_pct()
        );

        let diag_path = diagnostic_file(&state);
        assert!(diag_path.is_file(), "diagnostic file must be written");
        let diag = json(&diag_path);
        assert!(diag["deleted"]["lruBytes"].as_u64().unwrap() >= 350 * 1024);
        assert!(diag["primary"]["beforeUsedPct"].as_f64().unwrap() >= 85.0);
        assert!(diag["primary"]["usedPct"].as_f64().unwrap() <= 70.5);
        assert!(!diag["deleted"]["entries"].as_array().unwrap().is_empty());

        drop(busy_lock);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn overflow_new_empty_slot_when_primary_high_and_return_on_release_below_60pct() {
        let base = temp_dir("delegate-disk-overflow");
        let pool = base.join("pool");
        let primary_root = base.join("primary");
        let overflow_root = base.join("overflow");
        let runs_dir = base.join("runs");
        fs::create_dir_all(&pool).unwrap();
        fs::create_dir_all(&primary_root).unwrap();
        fs::create_dir_all(&overflow_root).unwrap();
        fs::create_dir_all(&runs_dir).unwrap();

        // Slot 1 on primary is BUSY, so primary has ZERO evictable non-busy bytes.
        let s1 = pool.join("repo-1");
        fs::create_dir_all(&s1).unwrap();
        let s1_t = primary_root.join("repo-1/target");
        write_sized(&s1_t.join("debug/deps/libbusy-1111111111111111.rlib"), 65536, 100, false);
        std::os::unix::fs::symlink(&s1_t, s1.join("target")).unwrap();
        let busy_lock = locked_file(&s1_t.join(".cargo-lock"), "").unwrap();

        let cfg = DiskConfig {
            primary_root: Some(primary_root.clone()),
            overflow_root: Some(overflow_root.clone()),
            high_pct: 85,
            target_pct: 70,
            return_pct: 60,
            min_free_gib: 0,
            overflow_high_pct: 90,
            overflow_target_pct: 80,
            overflow_min_free_gib: 0,
            overflow_target_free_gib: 0,
            ..Default::default()
        };

        // Primary at 88%, overflow at 30% -> new empty slot 2 spills to overflowRoot!
        let s2 = pool.join("repo-2");
        fs::create_dir_all(&s2).unwrap();
        let run2 = runs_dir.join("20261126-130000-r2");
        fs::create_dir_all(&run2).unwrap();
        fs::write(pool.join("repo-2.owner"), run2.to_string_lossy().as_bytes()).unwrap();

        let high_probe = SimProbe {
            primary: DiskSpace::from_total_avail(1000, 120), // 88% used
            overflow: DiskSpace::from_total_avail(1000, 700), // 30% used
        };
        attach_target_with(
            &s2,
            &cfg,
            &high_probe,
            &pool,
            Path::new("/nonexistent"),
            &base,
            &|_| false,
        )
        .unwrap();
        let expected_ov = overflow_root.join("repo-2/target");
        assert_eq!(fs::read_link(s2.join("target")).unwrap(), expected_ov);
        write_sized(&expected_ov.join("debug/deps/libx-1111111111111111.rlib"), 16384, 50, false);

        // Hysteresis check: at 70% primary (> 60% returnPct), re-attaching keeps it on overflow.
        let mid_probe = SimProbe {
            primary: DiskSpace::from_total_avail(1000, 300), // 70% used
            overflow: DiskSpace::from_total_avail(1000, 700),
        };
        attach_target_with(
            &s2,
            &cfg,
            &mid_probe,
            &pool,
            Path::new("/nonexistent"),
            &base,
            &|_| false,
        )
        .unwrap();
        assert_eq!(fs::read_link(s2.join("target")).unwrap(), expected_ov);

        // Primary drops to 55% (<= 60% returnPct) -> release switches back to primary and removes overflow dir!
        let low_probe = SimProbe {
            primary: DiskSpace::from_total_avail(1000, 450), // 55% used
            overflow: DiskSpace::from_total_avail(1000, 700),
        };
        assert!(release_with(&s2, &run2, &cfg, &low_probe, &pool));
        assert_eq!(
            fs::read_link(s2.join("target")).unwrap(),
            primary_root.join("repo-2/target")
        );
        assert!(
            !expected_ov.exists(),
            "overflow target directory must be deleted on return to primary"
        );

        drop(busy_lock);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn overflow_disk_has_own_watermark_and_cold_migration_only_at_90pct() {
        let base = temp_dir("delegate-disk-ov-lru");
        let pool = base.join("pool");
        let primary_root = base.join("primary");
        let overflow_root = base.join("overflow");
        let state = base.join("state");
        let lane_dir = state.join("lane");
        fs::create_dir_all(&pool).unwrap();
        fs::create_dir_all(&primary_root).unwrap();
        fs::create_dir_all(&overflow_root).unwrap();
        fs::create_dir_all(&lane_dir).unwrap();

        // Overflow slot A (busy) and slot B (non-busy) on overflow disk.
        let ov_a = pool.join("repo-1");
        let ov_b = pool.join("repo-2");
        fs::create_dir_all(&ov_a).unwrap();
        fs::create_dir_all(&ov_b).unwrap();
        let ov_a_t = overflow_root.join("repo-1/target");
        let ov_b_t = overflow_root.join("repo-2/target");
        write_sized(&ov_a_t.join("debug/deps/liba-1111111111111111.rlib"), 200 << 10, 5000, false);
        write_sized(&ov_b_t.join("debug/deps/libb-2222222222222222.rlib"), 200 << 10, 4000, false);
        std::os::unix::fs::symlink(&ov_a_t, ov_a.join("target")).unwrap();
        std::os::unix::fs::symlink(&ov_b_t, ov_b.join("target")).unwrap();
        let ov_a_lock = locked_file(&ov_a_t.join(".cargo-lock"), "").unwrap();

        let cfg = DiskConfig {
            primary_root: Some(primary_root.clone()),
            overflow_root: Some(overflow_root.clone()),
            min_free_gib: 0,
            overflow_high_pct: 90,
            overflow_target_pct: 80,
            overflow_min_free_gib: 0,
            overflow_target_free_gib: 0,
            ..Default::default()
        };
        let total = 1024 * 1024;
        // Primary healthy (50%), overflow at 92% -> evicts non-busy ov_b, keeps busy ov_a.
        let probe = SimProbe {
            primary: DiskSpace::from_total_avail(total, total / 2),
            overflow: DiskSpace::from_total_avail(total, total * 8 / 100),
        };
        let report = prune_with(
            &cfg,
            &pool,
            &state,
            Path::new("/nonexistent"),
            &lane_dir,
            &[],
            &probe,
            &|_| false,
            false,
            1_795_000_000,
        )
        .unwrap();
        assert!(report.overflow_lru_bytes >= 150 * 1024);
        assert!(ov_a_t.join("debug/deps/liba-1111111111111111.rlib").exists());
        assert!(!ov_b_t.join("debug/deps/libb-2222222222222222.rlib").exists());

        // Now test cold idle migration at >=90% when primary has no evictable Cargo units
        // (only non-evictable top-level target artifact on an idle slot, plus a busy slot).
        let p_cold = pool.join("repo-3");
        let p_busy = pool.join("repo-4");
        fs::create_dir_all(&p_cold).unwrap();
        fs::create_dir_all(&p_busy).unwrap();
        let p_cold_t = primary_root.join("repo-3/target");
        let p_busy_t = primary_root.join("repo-4/target");
        write_sized(&p_cold_t.join("debug/custom-bin"), 65536, 5000, false);
        write_sized(&p_busy_t.join("debug/custom-bin"), 65536, 9000, false);
        std::os::unix::fs::symlink(&p_cold_t, p_cold.join("target")).unwrap();
        std::os::unix::fs::symlink(&p_busy_t, p_busy.join("target")).unwrap();
        let p_busy_lock = locked_file(&p_busy_t.join(".cargo-lock"), "").unwrap();

        let crit_probe = SimProbe {
            primary: DiskSpace::from_total_avail(total, total * 8 / 100), // 92% >= 90%
            overflow: DiskSpace::from_total_avail(total, total * 70 / 100), // 30%
        };
        let mig_report = prune_with(
            &cfg,
            &pool,
            &state,
            Path::new("/nonexistent"),
            &lane_dir,
            &[],
            &crit_probe,
            &|_| false,
            false,
            1_795_000_000,
        )
        .unwrap();
        assert_eq!(mig_report.migrations.len(), 1);
        assert_eq!(mig_report.migrations[0].slot, "repo-3");
        assert_eq!(mig_report.migrations[0].direction, "to_overflow");
        assert_eq!(
            fs::read_link(p_cold.join("target")).unwrap(),
            overflow_root.join("repo-3/target")
        );
        assert!(overflow_root.join("repo-3/target/debug/custom-bin").is_file());
        assert!(!p_cold_t.exists());
        // Busy slot must NOT be migrated even though it is older!
        assert_eq!(fs::read_link(p_busy.join("target")).unwrap(), p_busy_t);

        drop(ov_a_lock);
        drop(p_busy_lock);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn stale_slot_run_reminder_thresholds_and_never_deletes_run() {
        let base = temp_dir("delegate-disk-stale");
        let pool = base.join("pool");
        let state = base.join("state");
        let runs_dir = state.join("runs");
        fs::create_dir_all(&pool).unwrap();
        fs::create_dir_all(&runs_dir).unwrap();

        let now = 1_795_000_000i64;
        let make_run = |id: &str, slot_name: &str, bytes: u64, age_hours: i64, applied: bool| {
            let slot = pool.join(slot_name);
            fs::create_dir_all(&slot).unwrap();
            let rdir = runs_dir.join(id);
            fs::create_dir_all(&rdir).unwrap();
            let started = now - age_hours * 3600;
            write_json(
                rdir.join("meta.json"),
                &json!({
                    "run": id,
                    "name": id,
                    "mode": "write",
                    "startedEpoch": started,
                    "startedNs": (started as u64) * 1_000_000_000,
                    "worktree": {"path": slot, "pooled": true},
                    "worktreeBytes": bytes,
                }),
            )
            .unwrap();
            let ts = format_epoch(started, false, "%Y-%m-%dT%H:%M:%SZ");
            let sum_p = rdir.join("summary.json");
            write_json(&sum_p, &json!({"finishedAt": ts})).unwrap();
            // Backdate file mtime so `run_last_progress_epoch` sees the simulated age.
            let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(started as u64);
            let times = fs::FileTimes::new().set_accessed(mtime).set_modified(mtime);
            let _ = File::options().write(true).open(&sum_p).unwrap().set_times(times);
            if applied {
                fs::write(rdir.join(".applied"), "{}").unwrap();
            }
            rdir
        };

        // 1. Over 24h AND over 10 GiB -> MUST alert.
        let r_stale = make_run("r-stale", "repo-1", 12 * GIB, 26, false);
        // 2. Over 24h BUT under 10 GiB -> must NOT alert.
        let r_small = make_run("r-small", "repo-2", 9 * GIB, 30, false);
        // 3. Over 10 GiB BUT under 24h -> must NOT alert.
        let r_fresh = make_run("r-fresh", "repo-3", 15 * GIB, 20, false);
        // 4. Already applied -> must NOT alert.
        let r_applied = make_run("r-applied", "repo-4", 15 * GIB, 30, true);

        let cfg = DiskConfig::default();
        let probe = SimProbe {
            primary: DiskSpace::from_total_avail(300 * GIB, 150 * GIB),
            overflow: DiskSpace::from_total_avail(1000 * GIB, 800 * GIB),
        };
        let summary = inspect_readonly_with(
            &cfg,
            &pool,
            &state,
            &[r_stale.clone(), r_small, r_fresh, r_applied],
            &probe,
            now,
        );
        assert_eq!(summary.stale_runs.len(), 1);
        assert_eq!(summary.stale_runs[0].run, "r-stale");
        assert!(summary.stale_runs[0].idle_hours >= 26);
        assert!(summary.should_show_in_status());
        assert!(summary.should_warn_stop());
        let stop_msg = summary.format_stop_reminder().unwrap();
        assert!(stop_msg.contains("r-stale"));
        assert!(stop_msg.contains("不自动删"));
        // Advisory only: run directory MUST still exist untouched!
        assert!(r_stale.is_dir());

        let _ = fs::remove_dir_all(&base);
    }
}
