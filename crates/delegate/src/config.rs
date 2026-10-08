use crate::common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Config {
    pub value: Value,
    pub sources: Vec<&'static str>,
    pub ignored: Vec<&'static str>,
    pub origins: Value,
}

fn text_value(value: &Value) -> bool {
    value.as_str().is_some_and(|s| !s.contains('\0'))
        || value.as_array().is_some_and(|a| {
            a.iter()
                .all(|v| v.as_str().is_some_and(|s| !s.contains('\0')))
        })
}

fn standing_object(value: &Value) -> Value {
    if value.is_object() {
        value.clone()
    } else {
        json!({"all": value})
    }
}

pub fn text_lines(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        text.to_string()
    } else {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn user_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("delegate/config.json")
}

pub fn read_file(path: &Path) -> Res<Option<Value>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: config: {e}", path.display())),
    };
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: config JSON: {e}", path.display()))?;
    if !value.is_object() {
        return Err(format!("{}: config must be a JSON object", path.display()));
    }
    Ok(Some(value))
}

fn validate(value: &Value, path: &Path, repo: bool) -> Res<()> {
    let check = || -> Res<()> {
        if let Some(v) = value.get("standing") {
            let valid = if let Some(map) = v.as_object() {
                map.iter().all(|(k, v)| {
                    ["all", "write", "readOnly"].contains(&k.as_str()) && text_value(v)
                })
            } else {
                text_value(v)
            };
            if !valid {
                return Err(
                    "standing must be text, text array, or {all, write, readOnly} text fields"
                        .into(),
                );
            }
        }
        if value.get("resultChars").is_some_and(|v| {
            !v.as_u64()
                .is_some_and(|n| n > 0 && usize::try_from(n).is_ok())
        }) {
            return Err("resultChars must be a positive integer".into());
        }
        if let Some(v) = value.get("cleanupKeepExecutables") {
            if !v.as_array().is_some_and(|a| {
                a.iter().all(|v| {
                    v.as_str()
                        .is_some_and(|s| !s.is_empty() && !s.contains(['/', '\0']))
                })
            }) {
                return Err("cleanupKeepExecutables must be an array of executable names".into());
            }
        }
        if let Some(v) = value.get("defaults") {
            let map = v.as_object().ok_or("defaults must be an object")?;
            for (k, v) in map {
                let valid = match k.as_str() {
                    "worktree" => v.is_boolean(),
                    "protect" | "acceptAlso" => v.as_array().is_some_and(|a| {
                        a.iter().all(|v| {
                            v.as_str()
                                .is_some_and(|s| !s.trim().is_empty() && !s.contains('\0'))
                        })
                    }),
                    "timeout" => v.as_str().is_some_and(|s| seconds(s).is_ok()),
                    "evidence" => v.as_str().is_some_and(|s| !s.contains('\0')),
                    _ => false,
                };
                if !valid {
                    return Err(format!("invalid defaults.{k}"));
                }
                if k == "protect" {
                    let mut paths = v
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect();
                    crate::launch::normalize_protect(&mut paths)
                        .map_err(|e| format!("invalid defaults.protect: {e}"))?;
                }
            }
        }
        if let Some(env) = value.get("env").filter(|v| !v.is_null()) {
            let map = env.as_object().ok_or("env must map names to strings")?;
            for (key, val) in map {
                if key.is_empty()
                    || key.contains(['=', '\0'])
                    || !val.as_str().is_some_and(|v| !v.contains('\0'))
                {
                    return Err(format!(
                        "env must map names to strings: env.{key} must have a valid name and string without NUL"
                    ));
                }
            }
        }
        for key in ["accept", "evidence"] {
            if value
                .get(key)
                .is_some_and(|v| !v.as_str().is_some_and(|s| !s.contains('\0')))
            {
                return Err(format!("{key} must be a string without NUL"));
            }
        }
        if value
            .get("maxRework")
            .is_some_and(|v| !v.is_null() && v.as_u64().is_none())
        {
            return Err("maxRework must be a non-negative integer or null".into());
        }
        for key in [
            "maxActive",
            "maxCodex",
            "maxHeavy",
            "repoMaxActive",
            "repoMaxCodex",
        ] {
            if value.get(key).is_some_and(|v| v.as_u64().is_none()) {
                return Err(format!(
                    "{key} must be a non-negative integer (0 = unlimited)"
                ));
            }
        }
        crate::deny::validate_config(value.get("agentDeny"), repo)?;
        Ok(())
    };
    check().map_err(|e| format!("{}: {e}", path.display()))?;
    if repo {
        crate::worktree::work_config(value, path)?;
        crate::worktree::generated_config(value, path)?;
        match value.get("applyVerify") {
            None | Some(Value::Null) | Some(Value::Bool(_)) => {}
            Some(Value::String(s)) if !s.trim().is_empty() && !s.contains('\0') => {}
            _ => {
                return Err(format!(
                    "{}: applyVerify must be true, false or a command",
                    path.display()
                ))
            }
        }
    }
    Ok(())
}

pub fn load(top: Option<&Path>) -> Res<Config> {
    let user_file = user_path();
    let user = read_file(&user_file)?;
    let repo_file = top
        .filter(|top| !top.as_os_str().is_empty())
        .map(|top| top.join(".delegate.json"));
    let repo = repo_file.as_deref().map(read_file).transpose()?.flatten();
    let mut sources = vec![];
    let mut ignored = vec![];
    if let Some(user) = &user {
        validate(user, &user_file, false)?;
        crate::source_build::validate(user, &user_file, top)?;
        sources.push("user");
        for key in ["worktree", "generated", "applyVerify"] {
            if user.get(key).is_some() {
                ignored.push(key);
            }
        }
    }
    if let Some(repo) = &repo {
        validate(repo, repo_file.as_deref().unwrap(), true)?;
        crate::source_build::validate(repo, repo_file.as_deref().unwrap(), top)?;
        sources.push("repo");
    }
    let mut value = repo.clone().unwrap_or(json!({}));
    for key in [
        "accept",
        "evidence",
        "maxRework",
        "sourceBuild",
        "maxActive",
        "maxCodex",
        "maxHeavy",
        "repoMaxActive",
        "repoMaxCodex",
        "resultChars",
    ] {
        if value.get(key).is_none() {
            if let Some(v) = user.as_ref().and_then(|u| u.get(key)) {
                value[key] = v.clone();
            }
        }
    }
    let mut env = json!({});
    let mut defaults = json!({});
    let mut standing = json!({});
    let mut origins = json!({"defaults":{},"standing":{}});
    let mut keep = Vec::<Value>::new();
    let mut rules: Vec<Value> = vec![];
    let mut rule_sources = Vec::new();
    for (source, layer) in [("user", &user), ("repo", &repo)] {
        let Some(layer) = layer else { continue };
        if layer.get("resultChars").is_some() {
            origins["resultChars"] = json!(source);
        }
        if let Some(map) = layer["defaults"].as_object() {
            for (key, val) in map {
                defaults[key] = val.clone();
                origins["defaults"][key] = json!(source);
            }
        }
        if let Some(v) = layer.get("standing") {
            for (key, val) in standing_object(v).as_object().unwrap() {
                standing[key] = val.clone();
                origins["standing"][key] = json!(source);
            }
        }
        for v in layer["cleanupKeepExecutables"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if !keep.contains(v) {
                keep.push(v.clone());
            }
        }
        if let Some(map) = layer["env"].as_object() {
            env.as_object_mut().unwrap().extend(map.clone());
        }
        for rule in layer["agentDeny"].as_array().into_iter().flatten() {
            let position = rules
                .iter()
                .position(|r| r["argv"] == rule["argv"] && b(r, "exact") == b(rule, "exact"));
            if rule["allow"] == json!(true) {
                if let Some(pos) = position {
                    rules.remove(pos);
                    rule_sources.remove(pos);
                }
            } else if let Some(pos) = position {
                rules[pos] = rule.clone();
                rule_sources[pos] = source;
            } else {
                rules.push(rule.clone());
                rule_sources.push(source);
            }
        }
    }
    value["env"] = env;
    value["agentDeny"] = json!(rules);
    origins["agentDeny"] = json!(["user", "repo"]
        .into_iter()
        .filter(|source| rule_sources.contains(source))
        .collect::<Vec<_>>());
    value["defaults"] = defaults;
    value["standing"] = standing;
    value["cleanupKeepExecutables"] = json!(keep);
    Ok(Config {
        value,
        sources,
        ignored,
        origins,
    })
}

pub fn capacity(value: &Value, key: &str, name: &str, default: u64) -> Res<u64> {
    number(name, value[key].as_u64().unwrap_or(default))
}

pub fn capacities(value: &Value) -> Res<Value> {
    Ok(json!({
        "maxActive": capacity(value, "maxActive", "MAX_ACTIVE", 12)?,
        "maxCodex": capacity(value, "maxCodex", "MAX_CODEX", 6)?,
        "maxHeavy": capacity(value, "maxHeavy", "MAX_HEAVY", 2)?,
        "repoMaxActive": capacity(value, "repoMaxActive", "REPO_MAX_ACTIVE", 8)?,
        "repoMaxCodex": capacity(value, "repoMaxCodex", "REPO_MAX_CODEX", 4)?,
    }))
}

pub fn context_capacity(run: Option<&Path>) -> Res<Value> {
    let run = run
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("DELEGATE_RUN_DIR").map(PathBuf::from));
    if let Some(run) = run {
        let meta = json(run.join("meta.json"));
        if meta["configCapacity"].is_object() {
            return Ok(meta["configCapacity"].clone());
        }
        let source = s(&meta["worktree"], "source");
        let top = if source.is_empty() {
            s(&meta, "top")
        } else {
            source
        };
        return Ok(load(Some(Path::new(top)))?.value);
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    Ok(load(git_top(&cwd).as_deref())?.value)
}
