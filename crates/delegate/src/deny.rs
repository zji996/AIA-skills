use crate::common::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const EXIT_DENIED: i32 = 77;

pub fn validate(value: Option<&Value>) -> Res<Value> {
    validate_config(value, false)
}

pub fn validate_config(value: Option<&Value>, repo: bool) -> Res<Value> {
    let Some(value) = value else {
        return Ok(json!([]));
    };
    let rules = value.as_array().ok_or("agentDeny must be a list")?;
    for rule in rules {
        let argv = rule["argv"]
            .as_array()
            .filter(|args| !args.is_empty())
            .ok_or("agentDeny.argv must be a non-empty list of strings")?;
        if !argv
            .iter()
            .all(|arg| arg.as_str().is_some_and(|s| !s.contains('\0')))
        {
            return Err("agentDeny.argv must contain strings without NUL".into());
        }
        let program = argv[0].as_str().unwrap();
        if program.is_empty()
            || program == "."
            || program == ".."
            || !program
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        {
            return Err("agentDeny.argv[0] must be a program name (no path)".into());
        }
        if let Some(allow) = rule.get("allow") {
            if !allow.is_boolean() || (allow == &json!(true) && !repo) {
                return Err(
                    "agentDeny.allow must be a boolean; true is only allowed in repository config"
                        .into(),
                );
            }
        }
        if rule["allow"] != json!(true)
            && !rule["hint"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty() && !s.contains('\0'))
        {
            return Err("agentDeny.hint must be a non-empty string without NUL".into());
        }
    }
    Ok(value.clone())
}

fn original_path(meta: &Value) -> std::ffi::OsString {
    meta["env"]["PATH"]
        .as_str()
        .map(Into::into)
        .unwrap_or_else(|| env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()))
}

fn resolve(program: &str, path: &std::ffi::OsStr, cwd: &Path) -> Option<PathBuf> {
    env::split_paths(path).find_map(|dir| {
        let file = cwd.join(dir).join(program);
        if fs::metadata(&file).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) {
            // Preserve the executable name: cargo may be a rustup multicall symlink.
            std::path::absolute(file).ok()
        } else {
            None
        }
    })
}

pub fn prepare(meta: &Value, run: &Path) -> Res<()> {
    let rules = validate(meta.get("agentDeny"))?;
    let mut programs = BTreeMap::<&str, Vec<&Value>>::new();
    for rule in rules.as_array().unwrap() {
        programs
            .entry(rule["argv"][0].as_str().unwrap())
            .or_default()
            .push(rule);
    }
    if programs.is_empty() {
        return Ok(());
    }
    let dir = run.join("agent-shims");
    fs::create_dir(&dir).map_err(|e| e.to_string())?;
    let path = original_path(meta);
    for (program, rules) in programs {
        let mut body = "#!/bin/sh\n".to_string();
        for rule in rules {
            let args = rule["argv"].as_array().unwrap();
            body.push_str(&format!("if [ \"$#\" -ge {} ]", args.len() - 1));
            for (i, arg) in args.iter().enumerate().skip(1) {
                body.push_str(&format!(
                    " && [ \"${{{i}}}\" = {} ]",
                    shell_quote(arg.as_str().unwrap())
                ));
            }
            body.push_str(&format!(
                "; then\n  printf '1\\n' >> {}\n  printf '%s\\n' {} >&2\n  exit {EXIT_DENIED}\nfi\n",
                shell_quote(&run.join("denied.log").to_string_lossy()),
                shell_quote(s(rule, "hint")),
            ));
        }
        if let Some(real) = resolve(program, &path, Path::new(s(meta, "workdir"))) {
            body.push_str(&format!(
                "exec {} \"$@\"\n",
                shell_quote(&real.to_string_lossy())
            ));
        } else {
            body.push_str(&format!(
                "printf '%s\\n' {} >&2\nexit 127\n",
                shell_quote(&format!("{program}: not found on the original PATH"))
            ));
        }
        let shim = dir.join(program);
        write(&shim, body)?;
        fs::set_permissions(shim, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn inject(command: &mut Command, meta: &Value, run: &Path) -> Res<()> {
    let dir = run.join("agent-shims");
    if dir.is_dir() {
        let mut paths = vec![dir];
        paths.extend(env::split_paths(&original_path(meta)));
        command.env("PATH", env::join_paths(paths).map_err(|e| e.to_string())?);
    }
    Ok(())
}

pub fn count(run: &Path) -> usize {
    read(run.join("denied.log"))
        .lines()
        .filter(|line| *line == "1")
        .count()
}
