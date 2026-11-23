const LAUNCH: &[(&str, &str)] = &[
    ("words", "Prompt text (or use --prompt / --prompt-file)."),
    ("--prompt TEXT", "Prompt text."),
    ("--prompt-file FILE", "Read the prompt from FILE, or - for stdin."),
    ("--tier scout|review|write", "Purpose: scout reads/explores/summarizes; review reads for rigorous review/root causes; write changes code with acceptance. All three tiers must resolve. May combine with --model; scout/review imply read-only. Without either, infer review for reads or write for writes, with a visible notice and status marker. Transient failures without changes may retry on the next different model in this tier."),
    ("--name NAME", "Short label in the run id (default: first clause of the prompt, up to 24 characters)."),
    ("--workdir DIR", "Agent working directory (default: cwd)."),
    ("--image PATH", "Attach an image; repeatable. Never changes the selected model. Pi images=no exits 2: add image to that model's input in Pi models.json, or choose an image-capable --model. Empty Pi list skips the guard."),
    ("--share PATH", "Share a read-only input path; repeatable. Inside the repository, link at the same relative worktree path."),
    ("--read-only", "No writes: Pi uses read-only tools; --shell adds bash in an isolated snapshot. In git, reads a worktree snapshot by default."),
    ("--shell", "Read-only investigation with bash (logs, read-only queries), in a git snapshot only. File changes reject the task; never apply."),
    ("--in-place", "Read-only: read the live working tree. Writes: override defaults.worktree and write in place."),
    ("--accept COMMAND", "Shell command run after a write task; overrides .delegate.json accept."),
    ("--accept-also COMMAND", "Append task tests with && after --accept or the configured default; repeatable, ordered. Runs alone if no base command exists."),
    ("--no-accept", "Disable the default acceptance command; conflicts with --accept-also."),
    ("--protect PATH", "Protect a repository-relative file or directory prefix (trailing /); repeatable. Changes reject before acceptance; generated.paths overlaps allow only generator output, checked on completion."),
    ("--protect-reason PATH REASON", "Protect a path with its reason; repeatable. Report necessary protected changes instead of bypassing protection."),
    ("--allow PATH", "Open a protected path for this run and its replies only; repeatable, inside a protected path. Directories may use a trailing /. Echoed at start."),
    ("--hide-accept", "Keep the acceptance command from the agent for blind verification."),
    ("--accept-timeout DURATION", "Optional acceptance command limit (default: .delegate.json acceptTimeout, else unlimited; silent accept emits stall_warning)."),
    ("--evidence COMMAND", "Run after successful acceptance or answer; records evidence without changing state or exit code."),
    ("--no-evidence", "Disable the default evidence command."),
    ("--evidence-timeout DURATION", "Evidence command limit excluding queue time (default: 30m)."),
    ("--max-answer CHARS", "Ask for this answer length; failed compression displays a bounded excerpt and preserves the full result."),
    ("--timeout DURATION", "Optional limit for each attempt; no default. Stall guards remain active."),
    ("--retries N", "Malformed-answer reruns, 0-3 (default: 1)."),
    ("--provider NAME", "Pi provider for --model."),
    ("--model ALIAS", "Exact registered alias or full id, else unique case-insensitive substring of aliases/full ids in config models. Unregistered Pi models are never candidates. May combine with --tier; pins the model and disables automatic model changes. Do not append :thinking."),
    ("--overflow next|queue", "At model capacity: queue for the selected model (default), or explicitly allow the next tier candidate. Overrides rule overflow for this launch; pinned --model still queues."),
    ("--class scout|build|hard|design", "Show user-local fit card advice for this task class; advisory only (delegate fit)."),
    ("--thinking LEVEL", "off|minimal|low|medium|high|xhigh. Environment > CLI > tier rule > alias thinking > config thinking > Pi default; independent of the model name."),
    ("--allow-parallel-writes", "Allow concurrent write runs in one workdir."),
    ("--worktree", "Write in a detached git worktree seeded from the current tree; merge with apply."),
    ("--in RUN", "Read-only: review a separate snapshot of a finished RUN's worktree."),
    ("--work ID", "Attach this run to a persistent work item (e.g. W019); replies inherit workId automatically."),
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
    ("--allow PATH", "Open a protected path for this reply and its follow-up replies only; repeatable, inside a protected path. Directories may use a trailing /. Echoed at start."),
    ("--hide-accept", "Keep the acceptance command from the agent."),
    ("--accept-timeout DURATION", "Optional acceptance command limit (inherited; default: .delegate.json acceptTimeout, else unlimited)."),
    ("--evidence COMMAND", "Replace the inherited evidence command; failure never changes state or exit code."),
    ("--no-evidence", "Remove the inherited evidence command."),
    ("--evidence-timeout DURATION", "Evidence command limit (inherited; default: 30m)."),
    ("--max-answer CHARS", "Ask for this answer length; above 1.5 times the limit, continue the session once to compress. Keep the original on failure."),
    ("--timeout DURATION", "Optional limit for the attempt; no default. Stall guards remain active."),
    ("--fresh", "Start a new session with only this message; after apply, a new worktree starts from current HEAD."),
    ("--sync", "Force-merge the caller's later changes into the conversation worktree before replying; conflicts stop the reply (default auto-sync keeps the old baseline on conflict)."),
    ("--sync=agent", "Hand text conflicts to the colleague to resolve first; remaining markers fail acceptance. Binary/structural conflicts still stop the reply."),
    ("--no-sync", "Skip automatic baseline sync on reply and keep the worktree's existing baseline."),
    ("--when-done", "Record one pending reply while the run is active; detached delivery survives shell exit. Visible in status; stop cancels it. Cannot combine with --wait."),
    ("--wait", "Wait for the reply's outcome and answer instead of returning after launch."),
    ("--tier scout|review|write", "Continue on this purpose tier; may combine with --model. Ordinary replies keep the saved model and thinking."),
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
    ("wait", "Wait for runs or events (--since SEQ); print outcomes, answers, or new cursor."),
    (
        "work",
        "Manage the machine-wide unified work table (<state>/work.jsonl): list, show, add, wait (--after-applied), close, drop, defer, import-md.",
    ),
    (
        "events",
        "Show or follow the machine-wide append-only event stream (--since, --follow, --rebuild, --json).",
    ),
    (
        "queue",
        "Manage the deterministic DAG continuation queue (add, list, remove/cancel).",
    ),
    (
        "sign",
        "Sign a run's controllerReviewPaths diff SHA-256 hash (or --check validity; invalidated on any edit).",
    ),
    (
        "card",
        "Show a one-screen merge review card (L1: summary, risk items, tests/acceptance layers, folded changes, next steps).",
    ),
    (
        "status",
        "Show counts and concise one-line status for runs/chains needing action (status <run> adds path and note details); --all lists every round.",
    ),
    ("list", "Alias of status."),
    (
        "snapshot",
        "Zero-lock, zero-subprocess JSON snapshot of all repos, runs, work items, events, lane, DAG queue, merges, and radar candidates.",
    ),
    (
        "note",
        "Attach, list, or complete post-merge todos on a run or reply chain (survives clean until --done).",
    ),
    (
        "handoff",
        "Write one-page Markdown handoff (.local/run/delegate/HANDOFF.md) for uncleaned, unmerged runs and pending todos.",
    ),
    (
        "history",
        "Search per-repository finished run index (oldest first) by keywords, --since, or --state.",
    ),
    (
        "show",
        "Print a run's Layer-2 summary (prompt, conclusion, acceptance, post-apply commits, chain, archive).",
    ),
    (
        "board",
        "Print (and refresh) the automatically maintained workspace delegation board Markdown.",
    ),
    (
        "tui",
        "Launch the interactive terminal dashboard (execs delegate-tui if installed).",
    ),
    ("result", "Print a run's full answer."),
    ("diff", "Show a run's changes as a git diff."),
    ("patch", "Print retained patch, optionally filtered by path."),
    (
        "apply",
        "Merge a worktree conversation into the source tree.",
    ),
    (
        "merge",
        "Serially integrate delivered runs in an isolated integration worktree, run per-run accept + mergeAccept, and write merge-report.md (delegate apply --keep-commits <merge-run> to land).",
    ),
    (
        "radar",
        "Deterministic collect + fingerprint; on change runs a read-only scout to update <state>/radar/<repo>/board.md; pick <id> drafts a brief without dispatching.",
    ),
    (
        "busy",
        "Query live resource occupants (exit 0 busy, 1 idle).",
    ),
    ("lane", "Queue a heavy command with other machine checks."),
    ("remote", "Check SSH remote heavy-command execution (remote check)."),
    ("config", "Show merged effective configuration, including remote."),
    ("stop", "Terminate runs."),
    ("clean", "Delete finished runs."),
    ("fit", "Show or update the four-class model suitability card (fit set; fit --json)."),
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
        println!("Capacity: maxActive=32 and repoMaxActive=0 (unlimited) are runaway guards; model maxActive, maxHeavy (default: max(2, (available_threads - 2) / 12), memory/IO guard) and available memory are the actual throttles; local CPU concurrency uses a shared jobserver FIFO (<state>/jobserver.fifo, budget - maxHeavy tokens) with an in-process per-command proxy FIFO dynamically fair-sharing by seniority (weights 1, 1/2, 1/4…, floor 2; override cap with heavyJobs). Shortages queue without a timeout or new worktree/pool claim. status shows queue reason/ahead/since; queued runs have no runtime elapsed. stop cancels. Lost supervisors/reboots leave crashed runs, like deferred runs; no automatic recovery.");
        println!("\nStates: waiting | running | delivered (accept passed) | answered (no --accept) | rejected (accept failed or protected path changed) | malformed (empty or leaked tool call after reruns) | failed | timeout | stalled | killed | stopped | crashed | needs-redispatch (legacy --after run from 6.21.x).");
        println!(
            "An interrupted upstream (cut stream, reset or timed-out connection, 502/503/504, a 429 Pi did not retry) is not the task failing: the run waits 15s/45s/120s, checks the model through `probe`'s path, and continues the same session up to 3 times (DELEGATE_RESUME_BACKOFF, `0` off); `status --json` reports `resumes` and `transientError`. Auth failures, 400s and missing models are not retried."
        );
        println!(
            "Exit: 0 delivered/answered, 1 other finished, 2 usage, 75 still running at --max, 76 already covered by another waiter, 77 agentDeny shim refusal."
        );
        println!(".delegate.json agentDeny: argv prefixes by default; exact: true matches the whole argv. No global-option normalization. Acceptance/evidence/setup/apply use remote-only shims when enabled; caller lane stays local. Shims have no environment bypass.");
        let cwd = std::env::current_dir().unwrap_or_default();
        match crate::config::load(crate::common::git_top(&cwd).as_deref()) {
            Ok(config) => println!("Remote: {} (user remote.host; DELEGATE_REMOTE=0 disables; remote check probes)", config.value["remote"]),
            Err(error) => eprintln!("delegate config: {error}"),
        }
        println!("Runs: $DELEGATE_RUNS or <git root of cwd>/.local/run/delegate (old .local/run/pi runs remain readable).");
        return true;
    };
    let (usage, description, rows): (&str, &str, &[(&str, &str)]) = match command {
        "start" => ("[options] [words ...]", "Launch in the background and return at once.", LAUNCH),
        "run" => ("[options] [words ...]", "Start, then wait for the outcome and answer.", LAUNCH),
        "reply" => ("[options] run [words ...]", "Continue a finished run in the background, inheriting its agent, workdir, and worktree by default. Use --wait to collect its answer.", REPLY),
        "wait" => ("[options] [runs ...]", "With no run arguments, return once the next run finishes and print its answer. Repeat the same command for the next result. With --since SEQ, wait for new events past SEQ and return the new cursor.", &[
            ("runs", "Run ids or directories. With no arguments, collect active and undelivered runs from this caller when known; otherwise collect all in this repository."),
            ("--since SEQ", "Wait on the machine-wide event stream and return only events with seq > SEQ plus the new cursor (records read cursor for Stop hook)."),
            ("--all", "Wait for ALL selected runs (up to 50m per call); without names, select repository runs regardless of caller. Timeout lists pending names."),
            ("--machine", "Collect active and waiting runs registered across all repositories on this machine."),
            ("--json", "Print complete JSON fields instead of concise status lines. Tasks covered by another live waiter are skipped; exit 76 when all are covered."),
            ("--no-result", "Print only the outcome line."),
            ("--any", "Return once any selected run finishes or asks a question (the default for no arguments and named runs); lists every run settled since the previous wait (full answer for the first, one-line summary + result command for the rest)."),
            ("--until-all", "Wait until every selected run finishes before returning; Alias of --all for waiting semantics."),
            ("--stream", "Print one outcome line per run as each finishes (answers are read with wait <run>) and exit when none is left; without run arguments it also picks up runs this caller starts meanwhile. For hosts that turn each output line into a notification."),
        ]),
        "events" => ("[--since SEQ] [--follow] [--rebuild] [--json]", "Show, follow, or rebuild the machine-wide append-only event stream (<state_dir>/events.jsonl).", &[
            ("--since SEQ", "Only show events with seq > SEQ."),
            ("--follow, -f", "Keep following newly appended events until interrupted."),
            ("--rebuild", "Reconstruct events.jsonl from existing run meta/summary files across known repos."),
            ("--json", "Print raw JSONL event lines."),
        ]),
        "queue" => ("<add|list|remove|cancel> [options]", "Deterministic DAG continuation queue: automatically dispatches the next step with zero LLM calls when upstream runs deliver and pass acceptance (plus review PASS when present), or pauses and emits queue_paused on failure/question/NEEDS_CONTROLLER.", &[
            ("add <name>", "Add a queued step: --after <run|name> (repeatable/comma-separated) --prompt-file <FILE> [delegate start options...]."),
            ("list [--all] [--json]", "List queued, dispatched, and paused DAG items."),
            ("remove|cancel <name>", "Cancel queued item(s)."),
        ]),
        "sign" => ("[--check] [--json] [<run>]", "Record (or --check) the controller sign-off SHA-256 hash over a run's controllerReviewPaths diff. Any 1-byte change to matched paths invalidates the signature.", &[
            ("run", "Run id, name, or directory (default: last)."),
            ("--check", "Verify existing sign.json against the current diff without modifying (exit 0 valid/not required, 1 unsigned or invalidated)."),
            ("--json", "Print structured JSON output."),
        ]),
        "merge" => ("[--ready | <run>...] [--name NAME] [--json] | list [--all] [--json]", "Serially 3-way merge runs into an isolated integration worktree based on source HEAD. Regenerates generated.paths on generated.inputs hits, runs per-run accept and final mergeAccept through lane, preserves original commit titles with Delegate-Run trailers, skips & rolls back on conflict/red test/protected/unsigned or invalidated controllerReviewPaths, marks autoMerge eligibility, and writes merge-report.md.", &[
            ("--ready", "Select all unapplied delivered runs in this repository with passed acceptance and review PASS (or validly signed controllerReviewPaths)."),
            ("--name NAME", "Label for the integration merge run (default: merge)."),
            ("list [--all] [--json]", "List recorded merge runs and reports (--all across repositories)."),
            ("--json", "Print structured JSON output."),
        ]),
        "radar" => ("[run] [--force] [--dry-run] [--json] | list|board [--all] [--json] | pick <id> [--out PATH] [--json] | schedule [--repo PATH]", "Run deterministic radar.collect commands and fingerprint their output; exit with zero tokens when unchanged, or launch a read-only scout (scout tier) to add evidence-anchored candidates to <state>/radar/<repo>/board.md.", &[
            ("run [--force] [--dry-run]", "Collect and fingerprint; run read-only scout when changed (--force ignores unchanged fingerprint; --dry-run prints collected output without calling a model)."),
            ("list | board [--all] [--json]", "List candidates from <state>/radar/<repo>/board.md (--all across repositories)."),
            ("pick <id> [--out PATH]", "Generate a task brief draft for candidate <id> without auto-dispatching."),
            ("schedule [--repo PATH]", "Print systemd user timer and cron installation instructions (00:30-04:30 every 2h) without installing."),
        ]),
        "status" | "list" => ("[--all] [--json] [--files] [runs ...]", "Show a counts line plus concise one-line status for runs needing action (running/queued, questions, unapplied or unread results, unhandled rejections, and pending todos; collapsing reply chains to their latest round) and a footer pointing to delegate history. Passing run names appends indented path and todo details.", &[
            ("runs", "Run ids or directories (default: actionable runs and pending todos)."),
            ("--all", "List every run and every round regardless of handled or cleanup state."),
            ("--json", "Print JSON status without the files array (pass --files to include files)."),
            ("--files", "With --json, include the full files array."),
        ]),
        "work" => ("[list|show|add|wait|close|drop|defer|import-md] ...", "Machine-wide unified work table (<state>/work.jsonl) with 7 states (attention/ready/verify/active/blocked/deferred/done), structured wait_on (including run_applied and read-only owner:Qxx), 6-column table & card views, and automatic migration of notes.json / deferred.tsv.", &[
            ("[--all] [--state ST] [--repo NAME] [--rebuild] [--json]", "Show Layer-0 summary + 6-column table (merges unattached active/actionable runs as R:<id>; folds blocked/deferred by default)."),
            ("show <Wxx|R:id> [--json]", "Show Layer-2 work item card with wait_on status, linked runs, artifacts, followups, and Layer-3 original pointers."),
            ("add \"<title>\" [--done-when TEXT] [--brief FILE] [--after-applied RUN] [--after-delivered RUN] [--after-work Wxx] [--wait-owner Qxx] [--after-time TIME] [--wait-manual TEXT] [--artifact PATH] [--auto -- <start-args>]", "Create a persistent work item."),
            ("wait <Wxx> [--after-applied RUN] [--after-delivered RUN] [--after-work Wxx] [--wait-owner Qxx] [--after-time TIME] [--wait-manual TEXT] [--brief FILE] [--auto -- <start-args>] [--clear]", "Attach or update wait_on conditions on a work item (blocks until satisfied; unlocks on apply/finish or auto-dispatches with --auto)."),
            ("close <Wxx> --evidence \"<text>\"", "Explicitly close a work item with evidence (commit SHA, test/screenshot verification)."),
            ("drop <Wxx> --reason \"<text>\"", "Explicitly drop a work item with a recorded reason."),
            ("defer <Wxx|run> --reason \"<text>\" [--until 4h|24h|YYYY-MM-DD|ISO]", "Defer a work item or unattached run (materializes into work.jsonl) until automatic wake-up."),
            ("import-md <FILE> [--dry-run] [--include-stale] [--repo DIR] [--json]", "Parse a Markdown backlog, cross-check history.jsonl for already-applied/stale items, and preview (--dry-run) or import."),
        ]),
        "note" => ("[<run> [text ... | --done [<index> ... | --all]]] [--all] [--json]", "Thin compatibility wrapper over the unified work table (<state>/work.jsonl) for attaching, listing, or completing post-merge todos on a run or reply chain.", &[
            ("run", "Run id, name, or unique fragment (works after clean while pending todos remain). Omit to list all chains with pending todos."),
            ("text", "Todo text to append (timestamped). Omit to list notes on this chain."),
            ("--done", "Mark note(s) done: specify 1-based index(es), --all, or omit when only one pending todo remains."),
            ("--all", "With --done, complete every pending todo on the chain; with no run, include completed chains."),
            ("--json", "Print structured JSON output."),
        ]),
        "handoff" => ("[--out PATH]", "Write a one-page Markdown summary of uncleaned, unmerged runs and post-merge todos (default: <runs>/HANDOFF.md) and print both the page and its path.", &[
            ("-o, --out PATH", "Write the Markdown handoff to PATH instead of <runs>/HANDOFF.md."),
        ]),
        "history" => ("[keywords ...] [--since DURATION] [--state STATE] [--limit N] [--json]", "Query this repository's permanent history.jsonl index, oldest first. Keywords match name, prompt and conclusion (Chinese unigrams + bigrams).", &[
            ("keywords", "Filter terms matched against run id, name, prompt and conclusion."),
            ("--since DURATION", "Only include runs finished within DURATION (e.g. 14d, 24h)."),
            ("--state STATE", "Filter by state (e.g. delivered, applied, answered, rejected; comma-separated)."),
            ("--limit N", "Show at most the last N matching entries."),
            ("--json", "Print folded JSON lines instead of human summaries."),
        ]),
        "show" => ("[--json] [run]", "Print Layer-2 summary for an active, finished, cleaned/archived, or archive-expired run.", &[
            ("run", "Run id, name, or directory (default: last)."),
            ("--json", "Print structured JSON summary."),
        ]),
        "board" => ("[--path]", "Refresh and print the per-workspace delegation board Markdown (configured by .delegate.json `board`, default BOARD.md next to history.jsonl).", &[
            ("--path", "Print the resolved board file path instead of its contents."),
        ]),
        "tui" => ("[delegate-tui options ...]", "Launch the interactive read-only TUI dashboard by exec'ing `delegate-tui`.", &[
            ("options", "Forwarded directly to delegate-tui (run `delegate-tui --help` for flags)."),
        ]),
        "result" => ("[--path] [--no-mark] [run]", "Print a run's full answer.", &[
            ("run", "Run id or directory (default: last)."),
            ("--path", "Print the result file path instead of its contents."),
            ("--no-mark", "Read without marking resultReadAt or .delivered (used by read-only viewers)."),
        ]),
        "card" => ("[--json] [--round] [run]", "Show a deterministic one-screen L1 merge review card: author summary + result.md pointer, reviewer conclusion + verdict pointer, deduplicated risk items (built-in + .delegate.json riskRules + reviewer focus), test counts & tiered acceptance layers, directory-folded remaining changes, and next commands.", &[
            ("run", "Run id or directory (default: last)."),
            ("--json", "Emit structured JSON card."),
            ("--round", "Inspect only the last round instead of the chain's net total diff."),
        ]),
        "diff" => ("[--stat] [--total] [--risky] [--round] [run] [--] [paths...]", "Show a run's changes as a git diff (or L2 --risky for risk-matched hunks only).", &[
            ("run", "Run id or directory (default: last)."),
            ("paths", "Limit the diff to these paths."),
            ("--stat", "Show diff statistics."),
            ("--total", "Show the whole conversation, not only this run."),
            ("--risky", "L2 view: output only hunks matching deterministic risk rules or reviewer focus items, with surrounding context."),
            ("--round", "With --risky, inspect only the last round instead of the chain's net total diff."),
        ]),
        "patch" => ("run [-- paths...]", "Print retained applied.patch (or delivered changes.patch before apply), optionally limited to exact paths/directory prefixes. Suitable for git apply --3way.", &[]),
        "apply" => ("[--merge] [--dry-run] [--verify|--no-verify] [--keep] [--keep-commits|--commit] [--json] [run ...]", "Merge worktree conversations into the source working tree, in the order given; stops at the first that fails.", &[
            ("run", "Run ids or directories (default: last)."),
            ("--keep", "Keep the run and worktree after successful apply (default: retain lightweight run and patch, release worktree)."),
            ("--json", "Keep the existing JSON conclusion without the human acceptance reminder."),
            ("--merge", "Write conflict markers instead of stopping."),
            ("--dry-run", "Check the merge without writing it."),
            ("--verify", "Rerun acceptance on the merged tree even when acceptStillValid is true. Without it, runs whose acceptance does not carry over rerun their own acceptance once, after the last merge (.delegate.json applyVerify: false turns this off, a command replaces it)."),
            ("--no-verify", "Skip the post-merge acceptance."),
            ("--keep-commits", "Cherry-pick the colleague's commits instead of applying the final tree; refuses uncommitted files (listed in commitsDiffer)."),
            ("--commit", "Stage only the applied paths and create one squash commit using the colleague's commit title(s); refuses when the source index already has staged changes or a 3-way merged file had local uncommitted edits."),
            ("--closes ID", "Explicitly close work item ID (e.g. W019) with this apply's commit/run evidence."),
        ]),
        "config" => ("", "Show merged effective configuration (including the remote host).", &[]),
        "remote" => ("check", "Test SSH, rsync and remote program versions; never falls back locally.", &[]),
        "lane" => ("[--label TEXT] [--measure] [--json] [command ...]", "Queue a heavy command with acceptance, evidence, worktree setup, and other machine checks. --measure uses the FIFO measurement reservation and reserved CPUs. Local builds share a machine jobserver FIFO (<state>/jobserver.fifo, tokens = budget - maxHeavy where budget = max(1, available_threads - 2) and default maxHeavy = max(2, budget / 12)) via an in-process per-command proxy FIFO (MAKEFLAGS/CARGO_MAKEFLAGS/CARGO_BUILD_JOBS) with dynamic seniority-weighted fair sharing (weights 1, 1/2, 1/4…, floor 2; override with DELEGATE_MAX_HEAVY / maxHeavy and DELEGATE_HEAVY_JOBS / heavyJobs). One argument is a shell command; no command shows pool status, shares, and lists the lane.", &[
            ("--label TEXT", "Label shown in the lane listing."),
            ("--measure", "Run through the machine measurement reservation."),
            ("--json", "With no command, print lane occupancy and limits as JSON."),
            ("command", "Command and arguments to run; omit to list the lane."),
        ]),
        "stop" => ("runs ...", "Terminate one or more runs.", &[
            ("runs", "One or more run ids or directories."),
        ]),
        "snapshot" => ("--json [--repo PATH ...] [--root DIR] [--events-limit N]", "Print one consistent JSON snapshot of runs (backed by <state>/index.json), work table, recent events, lane, DAG queue, merges, and radar candidates without acquiring locks or spawning subprocesses.", &[
            ("--json", "Emit JSON snapshot (default)."),
            ("--repo PATH", "Limit snapshot to explicit repository path(s); repeatable."),
            ("--root DIR", "Optional workspace root hint."),
            ("--events-limit N", "Maximum recent events to include (default: 200; 0 for all)."),
        ]),
        "clean" => ("[--finished] [--force] [--no-archive] [--sync] [--json] [runs ...]", "Delete finished runs (staged to trash and reclaimed asynchronously by _gc-worker by default; pass --sync for synchronous GC) and their compose containers; reclaim containers whose delegate worktree directory is missing.", &[
            ("runs", "Run ids or directories to remove."),
            ("--no-archive", "Skip evidence archiving; by default keep ignored non-build outputs and final-answer references under cache/archive/<run>/ (200 MiB/run)."),
            ("--finished", "Select all finished runs that have been reported in full (answers shown truncated are kept until read with result)."),
            ("--force", "With --finished, also select unreported or truncated finished runs."),
            ("--sync", "Run trash/worktree/container garbage collection synchronously before returning."),
            ("--json", "Print deletion counts and orphan container cleanup as JSON lines."),
        ]),
        "tiers" => ("[--scout ALIAS] [--review ALIAS] [--write ALIAS] [--alias NAME=provider/id] [--thinking LEVEL]", "Print each purpose's rules (alias, full id, thinking) and registered aliases with Pi context/images/thinking capabilities. The final line counts unregistered Pi models without listing them. Settings validate and atomically update user config; --alias is repeatable. All three tiers must resolve for start/run/reply. Old tiers and model :thinking suffixes are unsupported: register aliases, configure the three purposes and move suffixes into thinking.", &[]),
        "fit" => ("[--json] | set CLASS ALIAS first|ok|avoid NOTE [--sample FILE]", "Four fixed classes (scout/build/hard/design); user-local fit.json stores only the latest verdict per alias/class, with an 80-character one-line note. Setting first demotes the previous first to ok. Missing evaluations are listed; --json shows full notes and samples.", &[]),
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
