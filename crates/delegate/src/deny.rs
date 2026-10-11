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
        if rule.get("exact").is_some_and(|exact| !exact.is_boolean()) {
            return Err("agentDeny.exact must be a boolean".into());
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
    let mut resources = BTreeMap::<&str, Vec<(&str, &Value)>>::new();
    if let Some(all) = meta["resources"].as_object() {
        for (name, spec) in all {
            for argv in spec["commands"].as_array().into_iter().flatten() {
                if let Some(program) = argv[0].as_str() {
                    resources.entry(program).or_default().push((name, argv));
                }
            }
        }
    }
    let mut measures = BTreeMap::<&str, Vec<&Value>>::new();
    if let Some(all) = meta["measure"].as_array() {
        for argv in all {
            if let Some(program) = argv[0].as_str() {
                measures.entry(program).or_default().push(argv);
            }
        }
    }
    for program in resources.keys() {
        programs.entry(program).or_default();
    }
    for program in measures.keys() {
        programs.entry(program).or_default();
    }
    if programs.is_empty() {
        return Ok(());
    }
    let dir = run.join("agent-shims");
    fs::create_dir(&dir).map_err(|e| e.to_string())?;
    let resource_dir = run.join("resource-shims");
    fs::create_dir(&resource_dir).map_err(|e| e.to_string())?;
    let measure_dir = run.join("measure-shims");
    fs::create_dir(&measure_dir).map_err(|e| e.to_string())?;
    let path = original_path(meta);
    for (program, rules) in programs {
        let mut body = "#!/bin/sh\n".to_string();
        for rule in &rules {
            let args = rule["argv"].as_array().unwrap();
            body.push_str(&format!(
                "if [ \"$#\" {} {} ]",
                if b(rule, "exact") { "-eq" } else { "-ge" },
                args.len() - 1
            ));
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
        let real = resolve(program, &path, Path::new(s(meta, "workdir")));
        let mut resource_body = "#!/bin/sh\n".to_string();
        for (name, argv) in resources.get(program).into_iter().flatten() {
            let mut condition = format!("[ \"$#\" -ge {} ]", argv.as_array().unwrap().len() - 1);
            for (i, arg) in argv.as_array().unwrap().iter().enumerate().skip(1) {
                condition.push_str(&format!(
                    " && [ \"${{{i}}}\" = {} ]",
                    shell_quote(arg.as_str().unwrap())
                ));
            }
            if let Some(real) = &real {
                let invoke = format!(
                    "if {condition}; then\n  exec {} _resource {} {} {} \"$@\"\nfi\n",
                    shell_quote(&script().to_string_lossy()),
                    shell_quote(&run.to_string_lossy()),
                    shell_quote(name),
                    shell_quote(&real.to_string_lossy())
                );
                body.push_str(&invoke);
                resource_body.push_str(&invoke);
            }
        }
        let mut measure_body = "#!/bin/sh\n".to_string();
        // The measurement shim is first on PATH so a command can be both
        // measured and denied; preserve agentDeny semantics before reserving.
        for rule in &rules {
            let args = rule["argv"].as_array().unwrap();
            measure_body.push_str(&format!(
                "if [ \"$#\" {} {} ]",
                if b(rule, "exact") { "-eq" } else { "-ge" },
                args.len() - 1
            ));
            for (i, arg) in args.iter().enumerate().skip(1) {
                measure_body.push_str(&format!(
                    " && [ \"${{{i}}}\" = {} ]",
                    shell_quote(arg.as_str().unwrap())
                ));
            }
            measure_body.push_str(&format!(
                "; then\n  printf '1\\n' >> {}\n  printf '%s\\n' {} >&2\n  exit {EXIT_DENIED}\nfi\n",
                shell_quote(&run.join("denied.log").to_string_lossy()),
                shell_quote(s(rule, "hint")),
            ));
        }
        for argv in measures.get(program).into_iter().flatten() {
            let args = argv.as_array().unwrap();
            let mut condition = format!("[ \"$#\" -ge {} ]", args.len() - 1);
            for (i, arg) in args.iter().enumerate().skip(1) {
                condition.push_str(&format!(
                    " && [ \"${{{i}}}\" = {} ]",
                    shell_quote(arg.as_str().unwrap())
                ));
            }
            if let Some(real) = &real {
                measure_body.push_str(&format!(
                    "if {condition}; then\n  exec {} _measure {} {} \"$@\"\nfi\n",
                    shell_quote(&script().to_string_lossy()),
                    shell_quote(&run.to_string_lossy()),
                    shell_quote(&real.to_string_lossy())
                ));
            }
        }
        if let Some(real) = &real {
            body.push_str(&format!(
                "exec {} \"$@\"\n",
                shell_quote(&real.to_string_lossy())
            ));
            resource_body.push_str(&format!(
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
        if resources.contains_key(program) {
            let shim = resource_dir.join(program);
            write(&shim, resource_body)?;
            fs::set_permissions(shim, fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        if measures.contains_key(program) {
            measure_body.push_str(&format!(
                "exec {} \"$@\"\n",
                shell_quote(
                    &real
                        .as_ref()
                        .map_or_else(|| program.to_string(), |x| x.to_string_lossy().into_owned())
                )
            ));
            let shim = measure_dir.join(program);
            write(&shim, measure_body)?;
            fs::set_permissions(shim, fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
pub fn resource_path(meta: &Value, run: &Path) -> Res<Option<std::ffi::OsString>> {
    let dir = run.join("resource-shims");
    let measure = run.join("measure-shims");
    if !dir.is_dir() && !measure.is_dir() {
        return Ok(None);
    }
    let mut paths = vec![dir];
    if measure.is_dir() {
        paths.insert(0, measure);
    }
    paths.extend(env::split_paths(&original_path(meta)));
    Ok(Some(env::join_paths(paths).map_err(|e| e.to_string())?))
}

pub fn inject(command: &mut Command, meta: &Value, run: &Path) -> Res<()> {
    let dir = run.join("agent-shims");
    let resource = run.join("resource-shims");
    let measure = run.join("measure-shims");
    if dir.is_dir() || resource.is_dir() || measure.is_dir() {
        let mut paths = Vec::new();
        if dir.is_dir() {
            paths.push(dir);
        }
        if resource.is_dir() {
            paths.push(resource);
        }
        if measure.is_dir() {
            paths.insert(0, measure);
        }
        paths.extend(env::split_paths(&original_path(meta)));
        command.env("PATH", env::join_paths(paths).map_err(|e| e.to_string())?);
    }
    Ok(())
}

fn words(text: &str) -> Vec<String> {
    text.replace(['`', '\r'], " ")
        .split_whitespace()
        .map(|word| {
            word.trim_matches(['"', '\'', '`', ';', '|', '&'])
                .to_string()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

pub fn conflicts(accept: Option<&str>, prompt: &str, rules: &Value) -> Vec<String> {
    let mut texts = Vec::new();
    if let Some(command) = accept.filter(|x| !x.trim().is_empty()) {
        texts.push(command.to_string());
    }
    texts.extend(
        prompt
            .lines()
            .filter(|line| {
                let line = line.trim();
                line.starts_with('$')
                    || line.starts_with("cargo ")
                    || line.starts_with("bash ")
                    || line.starts_with("sh ")
            })
            .map(str::to_string),
    );
    let mut out = Vec::new();
    for text in texts {
        let tokens = words(&text);
        for rule in rules.as_array().into_iter().flatten() {
            let argv = rule["argv"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>();
            if argv.is_empty() || tokens.len() < argv.len() {
                continue;
            }
            let matched = tokens
                .windows(argv.len())
                .any(|window| window.iter().zip(&argv).all(|(a, b)| a == b));
            let exact = b(rule, "exact");
            let matched = matched && (!exact || tokens.len() == argv.len());
            if matched {
                let command = argv
                    .iter()
                    .map(|x| shell_quote(x))
                    .collect::<Vec<_>>()
                    .join(" ");
                if !out.contains(&command) {
                    out.push(command);
                }
            }
        }
    }
    out
}

pub fn count(run: &Path) -> usize {
    read(run.join("denied.log"))
        .lines()
        .filter(|line| *line == "1")
        .count()
}
