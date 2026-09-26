# Rust delegate

The `delegate` CLI (the only implementation; the contract is `docs/delegate-spec.md`).
Releases: `scripts/release-binary.sh build|publish delegate`; installs: `scripts/fetch-binary.sh delegate`.
The binary uses its own `_supervise` subcommand for each background run.

## Build

```sh
cd crates/delegate
cargo build --release --locked
# Static Linux binary (requires the musl target and musl linker):
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
```

The executables are `target/release/delegate` and
`target/x86_64-unknown-linux-musl/release/delegate`, respectively.
The musl build was verified as a stripped, statically linked PIE (about 1.2 MB).

## Modules

| Module | Responsibility |
| --- | --- |
| `main.rs` | CLI, output, collection, stop and cleanup |
| `common.rs` | Settings, files, processes and Git helpers |
| `launch.rs` | Prompt, admission and run creation |
| `runs.rs` | Run lookup, status and retention |
| `agents.rs` | Pi/Codex commands, events and attempt verdicts |
| `supervise.rs` | Background lifecycle and acceptance |
| `lane.rs` | Machine-wide heavy-work queue |
| `changes.rs` | Git snapshots and change records |
| `worktree.rs` | Isolated worktrees and apply |

Measured on 2026-09-27 with `/proc/<pid>/status`: a live `_supervise` process
whose fake Pi agent was sleeping had `VmRSS: 2908 kB` (2.84 MiB). This measures
the supervisor, not the delegated agent or the start command. Over 10 idle
seconds, `voluntary_ctxt_switches` stayed unchanged for the main thread and
each worker thread; the previous polling implementation added 497 switches
in its stop thread and 200 in its watchdog thread.
