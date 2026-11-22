use crate::common::Res;
use std::env;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN_NAME: &str = "delegate-tui";

fn is_executable_file(path: &Path) -> bool {
    path.is_file()
        && path
            .metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    for dir in env::split_paths(&paths) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn find_tui_binary() -> Option<PathBuf> {
    if let Ok(explicit) = env::var("DELEGATE_TUI_BIN") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if is_executable_file(&p) {
                return Some(p);
            }
            // Explicit override points to a missing/non-executable path: do not silently fall back.
            return None;
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(BIN_NAME);
            if is_executable_file(&sibling) {
                return Some(sibling);
            }
        }
    }
    find_in_path(BIN_NAME)
}

pub fn command(args: &[String]) -> Res<i32> {
    let Some(bin) = find_tui_binary() else {
        eprintln!(
            "delegate tui: 未找到 `{BIN_NAME}` 二进制。\n\
             安装方法：\n\
             \x20 1. 在 AIA-skills 仓库运行: ./scripts/fetch-binary.sh delegate  （或 ./scripts/install.sh delegate）\n\
             \x20 2. 或从源码编译: cargo build --release --manifest-path crates/delegate-tui/Cargo.toml\n\
             \x20    并将 target/release/{BIN_NAME} 放到 delegate 同目录或 PATH 中（或设 DELEGATE_TUI_BIN）。"
        );
        return Ok(1);
    };

    let mut cmd = Command::new(&bin);
    cmd.args(args);
    if env::var_os("DELEGATE_BIN").is_none() {
        if let Ok(exe) = env::current_exe() {
            cmd.env("DELEGATE_BIN", exe);
        }
    }
    let err = cmd.exec();
    Err(format!("failed to exec {}: {err}", bin.display()))
}
