//! Durable one-message mailbox, delivered by a detached worker after the parent settles.
use crate::common::*;
use crate::{launch, runs};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

pub fn status(run: &Path) -> Option<Value> {
    let value = json(run.join("pending-reply.json"));
    value.is_object().then_some(value)
}

pub fn enqueue(args: &[String], o: &mut launch::Options) -> Res<bool> {
    let run = runs::resolve_head(o.run.as_deref().unwrap_or("last"))?;
    if !runs::active(&runs::state(&run)) {
        return Ok(false);
    }
    if o.wait {
        return Err("--when-done cannot be combined with --wait; collect the reply later".into());
    }
    let _guard = lock(&run.join("reply.lock"), true, true)
        .map_err(|_| "reply mailbox busy; try again".to_string())?;
    if status(&run).is_some() {
        return Err("a pending reply already exists; stop cancels it".into());
    }
    // Snapshot stdin/files now, not in a shell that may no longer exist at delivery.
    let prompt = launch::read_prompt(o)?;
    let saved = json!({"args":args,"message":prompt,"queuedAt":iso(),"cwd":std::env::current_dir().map_err(|e|e.to_string())?});
    write_json(run.join("pending-reply.json"), &saved)?;
    let mut command = Command::new(script());
    command
        .arg("_reply-pending")
        .arg(&run)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    group(&mut command);
    if let Err(error) = command.spawn() {
        let _ = fs::remove_file(run.join("pending-reply.json"));
        return Err(error.to_string());
    }
    Ok(true)
}

pub fn cancel(run: &Path) -> Res<()> {
    let _guard = lock(&run.join("reply.lock"), true, false).map_err(|e| e.to_string())?;
    touch(run.join(".stopped"));
    let _ = fs::remove_file(run.join("pending-reply.json"));
    Ok(())
}

pub fn deliver(run: &Path) -> Res<()> {
    loop {
        if status(run).is_none() || run.join(".stopped").exists() {
            return Ok(());
        }
        if !runs::active(&runs::state(run)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _guard = lock(&run.join("reply.lock"), true, false).map_err(|e| e.to_string())?;
    let Some(saved) = status(run) else {
        return Ok(());
    };
    if run.join(".stopped").exists() || runs::state(run) == "stopped" {
        let _ = fs::remove_file(run.join("pending-reply.json"));
        return Ok(());
    }
    let mut options = launch::parse_launch(&strings(&saved["args"]), true, true)?;
    options.when_done = false;
    options.run = Some(run.to_string_lossy().into_owned());
    options.prompt_file = None;
    options.prompt = Some(s(&saved, "message").to_string());
    options.words.clear();
    std::env::set_current_dir(s(&saved, "cwd")).map_err(|e| e.to_string())?;
    // Leave a durable diagnostic instead of silently losing a failed launch.
    match launch::reply(options) {
        Ok(child) => {
            write_json(
                run.join("reply-delivered.json"),
                &json!({"run":child,"at":iso()}),
            )?;
            fs::remove_file(run.join("pending-reply.json")).map_err(|e| e.to_string())?;
        }
        Err(error) => {
            let mut failed = saved;
            failed["error"] = json!(error);
            write_json(run.join("pending-reply.json"), &failed)?;
        }
    }
    Ok(())
}
