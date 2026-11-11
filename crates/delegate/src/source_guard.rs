//! Bash accident guard, not a shell parser or a permissions sandbox.
use crate::common::*;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

fn inside(path: &Path, source: &Path) -> bool {
    path.starts_with(source) && !path.starts_with(source.join(".local"))
}
fn absolute(cwd: &Path, word: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for part in cwd.join(word).components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out.canonicalize().unwrap_or(out)
}

fn destination<'a>(program: &str, words: &[&'a str]) -> Option<&'a str> {
    let mut target = None;
    let mut last = None;
    let mut positional = false;
    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        if word == ">"
            || (word.bytes().all(|b| b.is_ascii_digit()) && words.get(i + 1) == Some(&">"))
        {
            // Ignore a simple > / >> / fd> output and its target as operands.
            while i < words.len()
                && (words[i] == ">" || words[i].bytes().all(|b| b.is_ascii_digit()))
            {
                i += 1;
            }
            i += 1;
            continue;
        }
        if !positional && word == "--" {
            positional = true;
        } else if !positional
            && program != "rsync"
            && (word == "-t" || word == "--target-directory")
        {
            i += 1;
            target = words.get(i).copied();
        } else if !positional && program != "rsync" && word.starts_with("--target-directory=") {
            target = word.strip_prefix("--target-directory=");
        } else if !positional && program != "rsync" && word.starts_with("-t") && word.len() > 2 {
            target = Some(&word[2..]);
        } else if positional || !word.starts_with('-') {
            last = Some(word);
        }
        i += 1;
    }
    // rsync -t preserves times, unlike cp/install/ln's target-directory option.
    target.or(last).filter(|path| !path.is_empty())
}

pub fn refused(command: &str, cwd: &Path, source: &Path) -> Option<PathBuf> {
    // Only literal words and redirections; expansions, scripts and nested shells
    // cannot be classified reliably without executing/parsing them.
    let separated = command.replace('>', " > ");
    let words = separated
        .split_whitespace()
        .map(|w| w.trim_matches(['\'', '"', ';', '&', '|', '(', ')']))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>();
    let program = words.first()?;
    for pair in words.windows(2) {
        if pair[0] == ">"
            && pair[1] != ">"
            && !pair[1].starts_with('&')
            && inside(&absolute(cwd, pair[1]), source)
        {
            return Some(absolute(cwd, pair[1]));
        }
    }
    let name = Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program);
    if name == "cd" {
        return None;
    } // Reads after cd remain usable.
    if matches!(name, "cp" | "rsync" | "install" | "ln") {
        if let Some(destination) = destination(name, &words[1..]) {
            let path = absolute(cwd, destination);
            return inside(&path, source).then_some(path);
        }
        // An unrecognized/missing destination keeps the conservative fallback.
    }
    let read = matches!(
        name,
        "cat"
            | "head"
            | "tail"
            | "less"
            | "more"
            | "ls"
            | "pwd"
            | "rg"
            | "grep"
            | "wc"
            | "stat"
            | "du"
            | "file"
            | "diff"
            | "test"
            | "["
            | "echo"
            | "printf"
            | "true"
            | "false"
    );
    let simple_read = (name == "sed" && !words.iter().skip(1).any(|w| w.starts_with("-i")))
        || (name == "find"
            && !words.iter().any(|w| {
                matches!(
                    *w,
                    "-delete"
                        | "-exec"
                        | "-execdir"
                        | "-ok"
                        | "-okdir"
                        | "-fprint"
                        | "-fprint0"
                        | "-fprintf"
                        | "-fls"
                )
            }));
    let git_read = name == "git"
        && words.get(1).is_some_and(|w| {
            matches!(
                *w,
                "status" | "diff" | "log" | "show" | "ls-files" | "rev-parse" | "grep"
            )
        });
    // find -delete/-exec, sed -i and arbitrary helpers are intentionally not
    // classified as reads. A helper in the source may build/write there.
    if read || simple_read || git_read {
        return None;
    }
    if inside(cwd, source) {
        // Simple writes explicitly confined to .local are allowed.
        if matches!(name, "touch" | "mkdir" | "rm" | "rmdir") {
            let targets = words
                .iter()
                .skip(1)
                .filter(|w| !w.starts_with('-'))
                .collect::<Vec<_>>();
            if !targets.is_empty()
                && targets
                    .iter()
                    .all(|w| absolute(cwd, w).starts_with(source.join(".local")))
            {
                return None;
            }
        }
        return Some(
            words
                .iter()
                .skip(1)
                .filter(|word| !word.starts_with('-'))
                .map(|word| absolute(cwd, word))
                .find(|path| inside(path, source))
                .unwrap_or_else(|| cwd.to_path_buf()),
        );
    }
    words
        .iter()
        .filter(|word| word.starts_with('/'))
        .map(|word| absolute(cwd, word))
        .find(|path| inside(path, source))
}

pub fn command(args: &[String]) -> Res<i32> {
    if args.len() != 3 {
        return Err("source check requires run, command, cwd".into());
    }
    let meta = json(Path::new(&args[0]).join("meta.json"));
    let source = Path::new(s(&meta["worktree"], "source"));
    let tree = s(&meta["worktree"], "path");
    if source.is_absolute() && !tree.is_empty() && tree != source.to_string_lossy() {
        if let Some(path) = refused(&args[1], Path::new(&args[2]), source) {
            let path = path.to_string_lossy().replace(['\n', '\r'], " ");
            let tree = tree.replace(['\n', '\r'], " ");
            eprintln!("source checkout is read-only for this task (write to {path} refused); work in {tree}");
            return Ok(77);
        }
    }
    Ok(0)
}

pub fn inject(command: &mut Command, meta: &Value, run: &Path) -> Res<()> {
    let source = s(&meta["worktree"], "source");
    if !Path::new(source).is_absolute() || source == s(&meta["worktree"], "path") {
        return Ok(());
    }
    let env = run.join("bash-env");
    let previous = meta["env"]["BASH_ENV"]
        .as_str()
        .map(str::to_string)
        .or_else(|| std::env::var("BASH_ENV").ok());
    let mut body = String::new();
    if let Some(previous) = previous.filter(|p| Path::new(p) != env) {
        body.push_str(&format!("[ ! -f {0} ] || . {0}\n", shell_quote(&previous)));
    }
    body.push_str(&format!(
        "__delegate_source_check() {{\n case \"$PWD/\" in {source}/*) ;; *) case \"$1\" in *{source}*) ;; *) return 0;; esac;; esac\n {binary} _source-check {run} \"$1\" \"$PWD\" || exit 77\n}}\ntrap '__delegate_source_check \"$BASH_COMMAND\"' DEBUG\n",
        source=shell_quote(source), binary=shell_quote(&script().to_string_lossy()), run=shell_quote(&run.to_string_lossy())
    ));
    write(&env, body)?;
    command.env("BASH_ENV", env);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_commands_check_only_the_destination() {
        let root = std::env::current_dir().unwrap();
        let source = root.join("guard-source");
        let tree = root.join("guard-worktree");
        for program in ["cp", "rsync", "install", "ln"] {
            let from_source = format!("{program} {}/a copied", source.display());
            assert_eq!(refused(&from_source, &tree, &source), None, "{from_source}");
            let to_source = format!("{program} a {}/b", source.display());
            assert_eq!(
                refused(&to_source, &tree, &source),
                Some(source.join("b")),
                "{to_source}"
            );
            assert_eq!(
                refused(&format!("{program} a b"), &source, &source),
                Some(source.join("b"))
            );
            let local = format!("{program} a {}/.local/log", source.display());
            assert_eq!(refused(&local, &tree, &source), None);
            if program == "rsync" {
                let times = format!("rsync -t {}/a copied", source.display());
                assert_eq!(refused(&times, &tree, &source), None);
                continue;
            }
            for option in ["-t ", "-t", "--target-directory ", "--target-directory="] {
                let to_source = format!("{program} {option}{} a", source.display());
                assert_eq!(
                    refused(&to_source, &tree, &source),
                    Some(source.clone()),
                    "{to_source}"
                );
                let from_source = format!("{program} {option}. {}/a", source.display());
                assert_eq!(refused(&from_source, &tree, &source), None, "{from_source}");
            }
        }
        let redirected = format!("cp a {}/b 2>/dev/null", source.display());
        assert_eq!(refused(&redirected, &tree, &source), Some(source.join("b")));
    }
}
