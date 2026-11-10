const LAUNCH: &[(&str, &str)] = &[
    ("words", "Prompt text (or use --prompt / --prompt-file)."),
    ("--prompt TEXT", "Prompt text."),
    ("--prompt-file FILE", "Read the prompt from FILE, or - for stdin."),
    ("--tier scout|review|write", "Purpose: scout reads/explores/summarizes; review reads for rigorous review/root causes; write changes code with acceptance. All three tiers must resolve. Exclusive with --model. Without either, infer review for reads or write for writes, with a visible notice and status marker. Transient failures without changes may retry on the next different model in this tier."),
    ("--name NAME", "Short label in the run id (default: first clause of the prompt, up to 24 characters)."),
    ("--workdir DIR", "Agent working directory (default: cwd)."),
    ("--image PATH", "Attach an image; repeatable. Never changes the selected model. Pi images=no exits 2: add image to that model's input in Pi models.json, or choose an image-capable --model. Empty Pi list skips the guard."),
    ("--share PATH", "Share a read-only input path; repeatable. Inside the repository, link at the same relative worktree path."),
    ("--read-only", "No writes: isolated agents are instructed and checked; Pi uses read-only tools without isolation. In git, reads a worktree snapshot by default."),
    ("--in-place", "Read-only: read the live working tree. Writes: override defaults.worktree and write in place."),
    ("--accept COMMAND", "Shell command run after a write task; overrides .delegate.json accept."),
    ("--accept-also COMMAND", "Append task tests with && after --accept or the configured default; repeatable, ordered. Runs alone if no base command exists."),
    ("--no-accept", "Disable the default acceptance command; conflicts with --accept-also."),
    ("--protect PATH", "Protect a repository-relative file or directory prefix (trailing /); repeatable. Changes reject before acceptance; generated.paths overlaps allow only generator output, checked on completion."),
    ("--protect-reason PATH REASON", "Protect a path with its reason; repeatable. Report necessary protected changes instead of bypassing protection."),
    ("--hide-accept", "Keep the acceptance command from the agent for blind verification."),
    ("--accept-timeout DURATION", "Acceptance command limit (default: 10m)."),
    ("--evidence COMMAND", "Run after successful acceptance or answer; records evidence without changing state or exit code."),
    ("--no-evidence", "Disable the default evidence command."),
    ("--evidence-timeout DURATION", "Evidence command limit excluding queue time (default: 30m)."),
    ("--max-answer CHARS", "Ask for this answer length; failed compression displays a bounded excerpt and preserves the full result."),
    ("--timeout DURATION", "Optional limit for each attempt; no default. Stall guards remain active."),
    ("--retries N", "Malformed-answer reruns, 0-3 (default: 1)."),
    ("--provider NAME", "Pi provider for --model."),
    ("--model ALIAS", "Exact registered alias or full id, else unique case-insensitive substring of aliases/full ids in config models. Unregistered Pi models are never candidates. Exclusive with --tier; pins the model and disables automatic model changes. Do not append :thinking."),
    ("--overflow next|queue", "At model capacity: queue for the selected model (default), or explicitly allow the next tier candidate. Overrides rule overflow for this launch; pinned --model still queues."),
    ("--thinking LEVEL", "off|minimal|low|medium|high|xhigh. Environment > CLI > tier rule > config thinking > Pi default; independent of the model name."),
    ("--allow-parallel-writes", "Allow concurrent write runs in one workdir."),
    ("--worktree", "Write in a detached git worktree seeded from the current tree; merge with apply."),
    ("--after RUN", "Wait for RUN to finish successfully before starting this step; waiting uses no active slot. A write step after a write run starts from RUN's changes; apply RUN first."),
    ("--in RUN", "Read-only: review a separate snapshot of RUN's worktree after it finishes."),
];

const COLLECT: &[(&str, &str)] = &[
    (
        "--max DURATION",
        "Stop waiting after this long; exit 75 if still running. Claude Code defaults to a 50m check-in when omitted.",
    ),
    ("--progress", "Also print writes, errors, and retries."),
    (
        "--full",
        "Print the whole answer instead of the abbreviated head and tail.",
    ),
];

const REPLY: &[(&str, &str)] = &[
    ("run", "Finished parent run to continue. A write timeout continuation does not consume maxRework."),
    ("words", "Follow-up message (or use --prompt / --prompt-file)."),
    ("--prompt TEXT", "Follow-up message text."),
    ("--prompt-file FILE", "Read the message from FILE, or - for stdin."),
    ("--name NAME", "Short label in the new run id."),
    ("--image PATH", "Attach an image; repeatable. Never changes the selected model. Pi images=no exits 2: add image to that model's input in Pi models.json, or choose an image-capable --model. Empty Pi list skips the guard."),
    ("--accept COMMAND", "Replace the parent's acceptance command; '' removes it."),
    ("--accept-also COMMAND", "Append with && after --accept or the inherited acceptance command; repeatable, ordered. Runs alone if no base command exists."),
    ("--no-accept", "Remove the parent's acceptance command; conflicts with --accept-also."),
    ("--protect / --protect-reason", "Protected paths and reasons are inherited from the previous run and cannot be changed in reply."),
    ("--hide-accept", "Keep the acceptance command from the agent."),
    ("--accept-timeout DURATION", "Acceptance command limit (default: 10m)."),
    ("--evidence COMMAND", "Replace the inherited evidence command; failure never changes state or exit code."),
    ("--no-evidence", "Remove the inherited evidence command."),
    ("--evidence-timeout DURATION", "Evidence command limit (inherited; default: 30m)."),
    ("--max-answer CHARS", "Ask for this answer length; above 1.5 times the limit, continue the session once to compress. Keep the original on failure."),
    ("--timeout DURATION", "Optional limit for the attempt; no default. Stall guards remain active."),
    ("--fresh", "Start a new session with only this message; after apply, a new worktree starts from current HEAD."),
    ("--sync", "Merge the caller's later changes into the conversation worktree before replying; conflicts stop the reply."),
    ("--wait", "Wait for the reply's outcome and answer instead of returning after launch."),
    ("--tier scout|review|write", "Continue on this purpose tier; exclusive with --model. Ordinary replies keep the saved model and thinking."),
    ("--model ALIAS", "Continue on a registered model alias or full id; unique registry substring accepted."),
    ("--overflow next|queue", "Override model-capacity behavior when selecting a tier; default queue. Ordinary replies keep their model and queue. Replies have priority over new starts."),
    ("--thinking LEVEL", "Change thinking independently: off|minimal|low|medium|high|xhigh."),
    ("--minor", "Write conversations: a short fix (message up to 600 characters, default timeout 10m) outside the rework budget; a round that changes more than 60 lines counts after all."),
    ("--over-limit REASON", "Write conversations: reply past the .delegate.json maxRework budget (default 1) and record why."),
];

const COMMANDS: &[(&str, &str)] = &[
    ("start", "Create a background run and return at once; capacity shortages queue automatically."),
    ("run", "Start, then wait for the outcome and answer."),
    (
        "reply",
        "Continue a finished run's conversation in the background.",
    ),
    ("wait", "Wait for runs; print outcomes and answers."),
    (
        "status",
        "Print one concise status line per run; --json keeps complete fields.",
    ),
    ("list", "Alias of status."),
    ("result", "Print a run's full answer."),
    ("diff", "Show a run's changes as a git diff."),
    (
        "apply",
        "Merge a worktree conversation into the source tree.",
    ),
    (
        "busy",
        "Query live resource occupants (exit 0 busy, 1 idle).",
    ),
    ("lane", "Queue a heavy command with other machine checks."),
    ("stop", "Terminate runs."),
    ("clean", "Delete finished runs."),
    (
        "tiers",
        "Show purpose rules and registered aliases with Pi capabilities; configure --scout/--review/--write, --alias, --thinking.",
    ),
    (
        "probe",
        "Check each tier model's connectivity through Pi itself before dispatching.",
    ),
    (
        "protocol",
        "Print the harness protocol version, caller and agents as JSON.",
    ),
];

fn options(rows: &[(&str, &str)]) {
    for (name, description) in rows {
        let name = (*name).to_string();
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
        println!("Delegate atomic tasks to Pi colleagues; judge them by results.\n");
        println!("Commands:");
        options(COMMANDS);
        println!("\nOptions:");
        options(&[
            ("-h, --help", "Show this help."),
            ("--version", "Show the Cargo package version."),
        ]);
        println!("Capacity: maxActive=32 and repoMaxActive=0 (unlimited) are runaway guards; model maxActive, maxHeavy=2 and available memory are the actual throttles. Shortages queue without a timeout or new worktree/pool claim. status shows queue reason/ahead/since; queued runs have no runtime elapsed. --after waits upstream before joining; stop cancels. Lost supervisors/reboots leave crashed runs, like deferred runs; no automatic recovery.");
        println!("\nStates: waiting | running | delivered (accept passed) | answered (no --accept) | skipped (upstream failed) | rejected (accept failed or protected path changed) | malformed (empty or leaked tool call after reruns) | failed | timeout | stalled | killed | stopped | crashed.");
        println!(
            "An interrupted upstream (cut stream, reset or timed-out connection, 502/503/504, a 429 Pi did not retry) is not the task failing: the run waits 15s/45s/120s, checks the model through `probe`'s path, and continues the same session up to 3 times (DELEGATE_RESUME_BACKOFF, `0` off); `status --json` reports `resumes` and `transientError`. Auth failures, 400s and missing models are not retried."
        );
        println!(
            "Exit: 0 delivered/answered, 1 other finished, 2 usage, 75 still running at --max, 76 already covered by another waiter, 77 agentDeny shim refusal."
        );
        println!(".delegate.json agentDeny: argv prefixes by default; exact: true matches the whole argv. No global-option normalization. Acceptance/evidence/setup/apply and caller lane keep the original PATH. Shims have no environment bypass.");
        println!("Runs: $DELEGATE_RUNS or <git root of cwd>/.local/run/delegate (old .local/run/pi runs remain readable).");
        return true;
    };
    let (usage, description, rows): (&str, &str, &[(&str, &str)]) = match command {
        "start" => ("[options] [words ...]", "Launch in the background and return at once.", LAUNCH),
        "run" => ("[options] [words ...]", "Start, then wait for the outcome and answer.", LAUNCH),
        "reply" => ("[options] run [words ...]", "Continue a finished run in the background, inheriting its agent, workdir, and worktree by default. Use --wait to collect its answer.", REPLY),
        "wait" => ("[options] [runs ...]", "With no run arguments, return once the next run finishes and print its answer. Repeat the same command for the next result.", &[
            ("runs", "Run ids or directories. With no arguments, collect active and undelivered runs from this caller when known; otherwise collect all in this repository."),
            ("--all", "Collect all active and undelivered runs in this repository, regardless of caller."),
            ("--machine", "Collect active and waiting runs registered across all repositories on this machine."),
            ("--json", "Print complete JSON fields instead of concise status lines. Tasks covered by another live waiter are skipped; exit 76 when all are covered."),
            ("--no-result", "Print only the outcome line."),
            ("--any", "Return once any selected run finishes (the default for no arguments and named runs); skip answers already reported."),
            ("--until-all", "Wait until every selected run finishes before returning; --all controls caller scope only."),
            ("--stream", "Print one outcome line per run as each finishes (answers are read with wait <run>) and exit when none is left; without run arguments it also picks up runs this caller starts meanwhile. For hosts that turn each output line into a notification."),
        ]),
        "status" | "list" => ("[runs ...]", "Print one concise status line per run.", &[
            ("runs", "Run ids or directories (default: all runs)."),
            ("--json", "Print the existing complete JSON status structure."),
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
        "apply" => ("[--merge] [--dry-run] [--verify|--no-verify] [--keep] [--json] [run ...]", "Merge worktree conversations into the source working tree, in the order given; stops at the first that fails.", &[
            ("run", "Run ids or directories (default: last)."),
            ("--keep", "Keep the run and worktree after successful apply (default: clean this run)."),
            ("--json", "Keep the existing JSON conclusion without the human acceptance reminder."),
            ("--merge", "Write conflict markers instead of stopping."),
            ("--dry-run", "Check the merge without writing it."),
            ("--verify", "Rerun acceptance on the merged tree even when acceptStillValid is true. Without it, runs whose acceptance does not carry over rerun their own acceptance once, after the last merge (.delegate.json applyVerify: false turns this off, a command replaces it)."),
            ("--no-verify", "Skip the post-merge acceptance."),
        ]),
        "lane" => ("[--label TEXT] [--measure] [command ...]", "Queue a heavy command with acceptance, evidence, worktree setup, and other machine checks. --measure uses the FIFO measurement reservation and reserved CPUs. DELEGATE_MAX_HEAVY run at once (default: 2). One argument is a shell command; no command lists the lane.", &[
            ("--label TEXT", "Label shown in the lane listing."),
            ("--measure", "Run through the machine measurement reservation."),
            ("command", "Command and arguments to run; omit to list the lane."),
        ]),
        "stop" => ("runs ...", "Terminate one or more runs.", &[
            ("runs", "One or more run ids or directories."),
        ]),
        "clean" => ("[--finished] [--force] [--json] [runs ...]", "Delete finished runs and their compose containers; reclaim containers whose delegate worktree directory is missing.", &[
            ("runs", "Run ids or directories to remove."),
            ("--finished", "Select all finished runs that have been reported in full (answers shown truncated are kept until read with result)."),
            ("--force", "With --finished, also select unreported or truncated finished runs."),
            ("--json", "Print deletion counts and orphan container cleanup as JSON lines."),
        ]),
        "tiers" => ("[--scout ALIAS] [--review ALIAS] [--write ALIAS] [--alias NAME=provider/id] [--thinking LEVEL]", "Print each purpose's rules (alias, full id, thinking) and registered aliases with Pi context/images/thinking capabilities. The final line counts unregistered Pi models without listing them. Settings validate and atomically update user config; --alias is repeatable. All three tiers must resolve for start/run/reply. Old tiers and model :thinking suffixes are unsupported: register aliases, configure the three purposes and move suffixes into thinking.", &[]),
        "probe" => ("[--tier scout|review|write] [--model ALIAS] [--timeout SECONDS]", "Probe only registered models, deduplicated by full id, through the same Pi binary/environment/providers as start. Show ok/FAIL, elapsed/first-token time, matching tier rules and Pi's first error. No worktree, run directory or capacity slot. Exit 1 if an inferred-purpose pick fails, 0 otherwise, 2 for usage.", &[
            ("--tier scout|review|write", "Probe only this tier's registered candidates, including inactive rules."),
            ("--model ALIAS", "Probe one registered model instead; exclusive with --tier."),
            ("--timeout SECONDS", "Limit per model (default: 60)."),
        ]),
        "protocol" => ("", "Print one JSON line for harness adapters: protocol version, delegate version, the caller session and its source variable, and each agent's tier, resolved binary, version and PATH entries it shadows.", &[]),
        _ => return false,
    };
    println!("Usage: delegate {command} {usage}\n");
    println!("{description}\n");
    println!("Options:");
    options(&[("-h, --help", "Show this help.")]);
    options(rows);
    if matches!(command, "start" | "run" | "reply" | "stop") {
        options(&[(
            "--json",
            "Print complete JSON fields instead of concise status lines.",
        )]);
    }
    if matches!(command, "run" | "reply" | "wait") {
        options(COLLECT);
    }
    true
}
