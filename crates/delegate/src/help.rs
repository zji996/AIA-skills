const LAUNCH: &[(&str, &str)] = &[
    ("words", "Prompt text (or use --prompt / --prompt-file)."),
    ("--prompt TEXT", "Prompt text."),
    ("--prompt-file FILE", "Read the prompt from FILE, or - for stdin."),
    ("--tier cheap|strong", "Cheap: Pi for reading, summaries, copy, images; strong: Codex for code and rigorous review. Default: cheap for read-only, strong for writes. Eligible failed cheap runs escalate once to strong."),
    ("--agent", "Choose an agent directly; disables tier escalation."),
    ("--name NAME", "Short label in the run id."),
    ("--workdir DIR", "Agent working directory (default: cwd)."),
    ("--image PATH", "Attach an image; repeatable, supported by both agents."),
    ("--read-only", "No writes: isolated agents are instructed and checked; Pi uses read-only tools without isolation. In git, reads a worktree snapshot by default."),
    ("--in-place", "Read-only: read the working tree instead of a snapshot."),
    ("--accept COMMAND", "Shell command run after a write task; overrides .delegate.json accept."),
    ("--no-accept", "Disable the repository's default acceptance command."),
    ("--protect PATH", "Protect a repository-relative file or directory prefix (trailing /); repeatable. Changes reject the run before acceptance."),
    ("--protect-reason PATH REASON", "Protect a path with its reason; repeatable. Report necessary protected changes instead of bypassing protection."),
    ("--hide-accept", "Keep the acceptance command from the agent for blind verification."),
    ("--accept-timeout DURATION", "Acceptance command limit (default: 10m)."),
    ("--timeout DURATION", "Limit for each attempt (default: 25m Pi, 50m Codex)."),
    ("--retries N", "Malformed-answer reruns, 0-3 (default: 1)."),
    ("--provider NAME", "Agent provider override."),
    ("--model NAME", "Agent model override."),
    ("--thinking LEVEL", "Agent thinking level override."),
    ("--allow-parallel-writes", "Allow concurrent write runs in one workdir."),
    ("--worktree", "Write in a detached git worktree seeded from the current tree; merge with apply."),
    ("--after RUN", "Wait for RUN to finish successfully before starting this step; waiting uses no active slot."),
    ("--in RUN", "Read-only: review a separate snapshot of RUN's worktree after it finishes."),
];

const COLLECT: &[(&str, &str)] = &[
    (
        "--max DURATION",
        "Stop waiting after this long; exit 75 if still running.",
    ),
    ("--progress", "Also print writes, errors, and retries."),
    (
        "--full",
        "Print the whole answer instead of the abbreviated head and tail.",
    ),
];

const REPLY: &[(&str, &str)] = &[
    ("run", "Finished parent run to continue."),
    ("words", "Follow-up message (or use --prompt / --prompt-file)."),
    ("--prompt TEXT", "Follow-up message text."),
    ("--prompt-file FILE", "Read the message from FILE, or - for stdin."),
    ("--name NAME", "Short label in the new run id."),
    ("--image PATH", "Attach an image; repeatable."),
    ("--accept COMMAND", "Replace the parent's acceptance command; '' removes it."),
    ("--no-accept", "Remove the parent's acceptance command."),
    ("--protect / --protect-reason", "Protected paths and reasons are inherited from the previous run and cannot be changed in reply."),
    ("--hide-accept", "Keep the acceptance command from the agent."),
    ("--accept-timeout DURATION", "Acceptance command limit (default: 10m)."),
    ("--timeout DURATION", "Limit for the attempt (default: 25m Pi, 50m Codex)."),
    ("--fresh", "Start a new session in the same workdir/worktree and conversation; message must stand alone."),
    ("--sync", "Merge the caller's later changes into the conversation worktree before replying; conflicts stop the reply."),
    ("--wait", "Wait for the reply's outcome and answer instead of returning after launch."),
    ("--agent", "Hand the conversation to this agent; a change of agent starts a fresh session."),
    ("--tier cheap|strong", "Hand the conversation to this tier; a change of agent starts a fresh session."),
];

const COMMANDS: &[(&str, &str)] = &[
    ("start", "Launch in the background and return at once."),
    ("run", "Start, then wait for the outcome and answer."),
    (
        "reply",
        "Continue a finished run's conversation in the background.",
    ),
    ("wait", "Wait for runs; print outcomes and answers."),
    ("status", "Print one JSON status line per run."),
    ("list", "Alias of status."),
    ("result", "Print a run's full answer."),
    ("diff", "Show a run's changes as a git diff."),
    (
        "apply",
        "Merge a worktree conversation into the source tree.",
    ),
    ("lane", "Queue a heavy command with other machine checks."),
    ("stop", "Terminate runs."),
    ("clean", "Delete finished runs."),
    (
        "protocol",
        "Print the harness protocol version, caller and agents as JSON.",
    ),
];

fn options(rows: &[(&str, &str)]) {
    for (name, description) in rows {
        let name = if *name == "--agent" {
            format!("--agent {}", crate::agents::choices("|"))
        } else {
            (*name).to_string()
        };
        let mut line = format!("  {name:<26} ");
        let indent = " ".repeat(29);
        for word in description.split_whitespace() {
            if line.len() + word.len() + 1 > 100 && line.len() > indent.len() {
                println!("{}", line.trim_end());
                line = indent.clone();
            }
            line.push_str(word);
            line.push(' ');
        }
        println!("{}", line.trim_end());
    }
}

pub fn print(command: Option<&str>) -> bool {
    let Some(command) = command else {
        println!("Usage: delegate [-h] [--version] <command> [options]\n");
        println!("Delegate atomic tasks to Pi (Gemini) or Codex (GPT); judge them by results.\n");
        println!("Commands:");
        options(COMMANDS);
        println!("\nOptions:");
        options(&[
            ("-h, --help", "Show this help."),
            ("--version", "Show the Cargo package version."),
        ]);
        println!("\nStates: waiting | running | delivered (accept passed) | answered (no --accept) | skipped (upstream failed) | rejected (accept failed or protected path changed) | malformed (empty or leaked tool call after reruns) | failed | timeout | killed | stopped | crashed.");
        println!(
            "Exit: 0 delivered/answered, 1 other finished, 2 usage, 75 still running at --max."
        );
        println!("Runs: $DELEGATE_RUNS or <git root of cwd>/.local/run/delegate (old .local/run/pi runs remain readable).");
        return true;
    };
    let (usage, description, rows): (&str, &str, &[(&str, &str)]) = match command {
        "start" => ("[options] [words ...]", "Launch in the background and return at once.", LAUNCH),
        "run" => ("[options] [words ...]", "Start, then wait for the outcome and answer.", LAUNCH),
        "reply" => ("[options] run [words ...]", "Continue a finished run in the background, inheriting its agent, workdir, and worktree by default. Use --wait to collect its answer.", REPLY),
        "wait" => ("[options] [runs ...]", "Wait for runs and print outcomes and answers.", &[
            ("runs", "Run ids or directories. With no arguments, collect active and undelivered runs from this caller when known; otherwise collect all in this repository."),
            ("--all", "Collect all active and undelivered runs in this repository, regardless of caller."),
            ("--machine", "Collect active and waiting runs registered across all repositories on this machine."),
            ("--no-result", "Print only the outcome line."),
            ("--any", "Return once any run has finished: report the finished runs and name the rest on stderr. Repeat the same command for the next one."),
            ("--stream", "Print one outcome line per run as each finishes (answers are read with wait <run>) and exit when none is left; without run arguments it also picks up runs this caller starts meanwhile. For hosts that turn each output line into a notification."),
        ]),
        "status" | "list" => ("[runs ...]", "Print one JSON status line per run.", &[
            ("runs", "Run ids or directories (default: all runs)."),
        ]),
        "result" => ("[--path] [run]", "Print a run's full answer.", &[
            ("run", "Run id or directory (default: last)."),
            ("--path", "Print the result file path instead of its contents."),
        ]),
        "diff" => ("[--stat] [--total] [run] [--] [paths...]", "Show a run's changes as a git diff.", &[
            ("run", "Run id or directory (default: last)."),
            ("paths", "Limit the diff to these paths."),
            ("--stat", "Show diff statistics."),
            ("--total", "Show the whole conversation, not only this run."),
        ]),
        "apply" => ("[--merge] [--dry-run] [--verify|--no-verify] [run]", "Merge a worktree conversation into the source working tree.", &[
            ("run", "Run id or directory (default: last)."),
            ("--merge", "Write conflict markers instead of stopping."),
            ("--dry-run", "Check the merge without writing it."),
            ("--verify", "After merging, run the repository acceptance on the merged tree unless acceptStillValid is true (default from .delegate.json applyVerify)."),
            ("--no-verify", "Skip the post-merge acceptance even when .delegate.json enables it."),
        ]),
        "lane" => ("[--label TEXT] [command ...]", "Queue a heavy command with acceptance checks, worktree setup, and other machine checks. DELEGATE_MAX_HEAVY run at once (default: 1). One argument is a shell command; no command lists the lane.", &[
            ("--label TEXT", "Label shown in the lane listing."),
            ("command", "Command and arguments to run; omit to list the lane."),
        ]),
        "stop" => ("runs ...", "Terminate one or more runs.", &[
            ("runs", "One or more run ids or directories."),
        ]),
        "clean" => ("[--finished] [--force] [runs ...]", "Delete finished runs.", &[
            ("runs", "Run ids or directories to remove."),
            ("--finished", "Select all finished runs that have been reported in full (answers shown truncated are kept until read with result)."),
            ("--force", "With --finished, also select unreported or truncated finished runs."),
        ]),
        "protocol" => ("", "Print one JSON line for harness adapters: protocol version, delegate version, the caller session and its source variable, and each agent's tier, resolved binary, version and PATH entries it shadows.", &[]),
        _ => return false,
    };
    println!("Usage: delegate {command} {usage}\n");
    println!("{description}\n");
    println!("Options:");
    options(&[("-h, --help", "Show this help.")]);
    options(rows);
    if matches!(command, "run" | "reply" | "wait") {
        options(COLLECT);
    }
    true
}
