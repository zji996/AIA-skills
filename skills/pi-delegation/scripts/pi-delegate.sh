#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELF="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"
CORE="$SCRIPT_DIR/pi-json-stream.sh"
POLL_SECONDS=${PI_DELEGATE_POLL:-2}
RUNNING_EXIT=75
DEFAULT_MAX=240
RESULT_CHARS=${PI_DELEGATE_RESULT_CHARS:-6000}

usage() {
  cat >&2 <<'EOF'
usage: pi-delegate.sh <command> [options]

  run    [start options] [--max <dur>]    start, then wait; exit 75 if still running at --max
  start  [start options] [<prompt>...]    launch Pi in the background, print the run status
  wait   [<run>...|--all] [--max <dur>] [--quiet] [--no-result] [--full]
                                          stream new progress, then status and the final answer;
                                          --all = active runs plus finished runs not yet reported
  status [<run>...]                       one JSON line per run (default: all runs)
  result [<run>] [--path]                 print the final answer (default: last run)
  stop   <run>...                         terminate runs and their Pi processes
  clean  <run>...|--finished [--force]    delete finished runs; --finished keeps unreported ones
                                          unless --force

start options:
  --prompt-file <file|->  --prompt <text>  or trailing words as the prompt
  --name <label>  --read-only  --provider <name>  --model <name>  --thinking <level>
  --timeout <dur> (default 15m)  --workdir <dir> (default cwd)  --allow-parallel-writes

<run> is a run id, a unique fragment of it, "last", or a run directory.
Runs live in $PI_DELEGATE_RUNS, else <git root of cwd, or cwd>/.local/run/pi (not --workdir).
Reported runs older than $PI_DELEGATE_KEEP_DAYS (default 7, 0 disables) are pruned on start.
--max defaults to 240s; "wait --max 0" peeks without blocking.
Answers longer than $PI_DELEGATE_RESULT_CHARS (default 6000) show only their tail unless --full.
Exit codes: 0 ok, 1 failed/stopped, 2 usage error, 75 still running.
EOF
  exit 2
}

die() {
  echo "pi-delegate: $*" >&2
  exit 2
}

# Name every missing dependency at once, with a concrete way to install it.
require_tools() {
  local tool missing=() hints=() kit
  for tool in pi jq setsid timeout flock; do
    command -v "$tool" >/dev/null && continue
    missing+=("$tool")
    case "$tool" in
      pi)
        kit="$(cd -P "$SCRIPT_DIR/../../.." 2>/dev/null && pwd)/third_party/pi-kit/install.sh"
        if [[ -f "$kit" ]]; then
          hints+=("pi: sh $kit --additive")
        else
          hints+=("pi: curl -fsSL https://git.aiatechco.com:31443/zji996/pi-kit/raw/branch/main/install.sh | sh -s -- --additive"
                  "    (GitHub: https://raw.githubusercontent.com/zji996/pi-kit/main/install.sh)")
        fi ;;
      jq) hints+=("jq: sudo apt install jq  (or your package manager)") ;;
      setsid) hints+=("setsid: sudo apt install util-linux") ;;
      timeout) hints+=("timeout: sudo apt install coreutils") ;;
      flock) hints+=("flock: sudo apt install util-linux") ;;
    esac
  done
  (( ${#missing[@]} )) || return 0
  printf 'pi-delegate: missing required tools: %s\n' "${missing[*]}" >&2
  printf '  %s\n' "${hints[@]}" >&2
  exit 2
}

need_value() {
  (( $1 >= 2 )) && [[ -n "$2" ]] || die "$3 requires a value"
}

runs_root() {
  if [[ -n "${PI_DELEGATE_RUNS:-}" ]]; then
    printf '%s\n' "$PI_DELEGATE_RUNS"
    return
  fi
  local top
  top="$(git rev-parse --show-toplevel 2>/dev/null)" || top=$PWD
  printf '%s/.local/run/pi\n' "$top"
}

to_seconds() {
  [[ "$1" =~ ^([0-9]+)([smh]?)$ ]] || die "invalid duration: $1 (use e.g. 90, 90s, 5m, 1h)"
  case "${BASH_REMATCH[2]}" in
    m) echo $((BASH_REMATCH[1] * 60)) ;;
    h) echo $((BASH_REMATCH[1] * 3600)) ;;
    *) echo $((BASH_REMATCH[1])) ;;
  esac
}

all_runs() {
  local root
  local metas=()
  root="$(runs_root)"
  [[ -d "$root" ]] || return 0
  shopt -s nullglob
  metas=("$root"/*/meta.json)
  shopt -u nullglob
  (( ${#metas[@]} )) || return 0
  jq -r '"\(.startedNs // 0)\t\(input_filename | sub("/meta\\.json$"; ""))"' "${metas[@]}" |
    sort -n | cut -f2-
}

resolve_run() {
  local ref=$1 root match
  local matches=()
  if [[ -f "$ref/meta.json" ]]; then
    (cd "$ref" && pwd -P)
    return
  fi
  root="$(runs_root)"
  if [[ "$ref" == last ]]; then
    match="$(all_runs | tail -n 1)"
    [[ -n "$match" ]] || die "no runs under $root"
    printf '%s\n' "$match"
    return
  fi
  if [[ -f "$root/$ref/meta.json" ]]; then
    printf '%s\n' "$root/$ref"
    return
  fi
  while IFS= read -r match; do
    [[ "$(basename "$match")" == *"$ref"* ]] && matches+=("$match")
  done < <(all_runs)
  (( ${#matches[@]} == 1 )) ||
    die "run '$ref' matched ${#matches[@]} runs under $root (runs live under the git root of the directory where start ran; pass a run directory or set PI_DELEGATE_RUNS)"
  printf '%s\n' "${matches[0]}"
}

supervisor_alive() {
  local pid
  pid="$(cat "$1/pid" 2>/dev/null)" || return 1
  [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null && grep -qa _supervise "/proc/$pid/cmdline" 2>/dev/null
}

run_state() {
  local dir=$1 started
  if [[ -f "$dir/exit_code" ]]; then
    if [[ -f "$dir/stopped" ]]; then
      echo stopped
    elif [[ -s "$dir/summary.json" ]]; then
      jq -r '.status' "$dir/summary.json"
    else
      echo failed
    fi
  elif supervisor_alive "$dir"; then
    echo running
  elif [[ -f "$dir/pid" ]]; then
    echo crashed
  else
    started="$(jq -r '.startedEpoch // 0' "$dir/meta.json")"
    if (( $(date +%s) - started > 15 )); then
      echo crashed
    else
      echo starting
    fi
  fi
}

is_active() {
  [[ "$1" == running || "$1" == starting ]]
}

status_json() {
  local dir=$1 state result="" error=""
  state="$(run_state "$dir")"
  [[ -s "$dir/result.md" ]] && result="$dir/result.md"
  if ! is_active "$state" && [[ "$state" != ok && -s "$dir/stderr.log" ]]; then
    error="$(tail -n 3 "$dir/stderr.log")"
  fi
  jq -n -c \
    --arg state "$state" \
    --arg result "$result" \
    --arg error "$error" \
    --arg exit "$(cat "$dir/exit_code" 2>/dev/null || true)" \
    --argjson chars "$( [[ -n "$result" ]] && jq -Rrs 'length' "$result" || echo 0)" \
    --argjson now "$(date +%s)" \
    --slurpfile meta "$dir/meta.json" \
    --rawfile events <(cat "$dir/events.jsonl" 2>/dev/null || true) '
    $meta[0] as $m |
    [$events | split("\n")[] | select(length > 0) | (try fromjson catch empty)] as $ev |
    [$ev[] | select(.e == "turn")] as $turns |
    ([$ev[] | select(.e == "summary")] | last) as $sum |
    ([$ev[] | select(.e | IN("summary", "result", "turn", "agent_end", "settled", "bash_done", "check_done") | not)] | last) as $last |
    {run:$m.run, state:$state, name:$m.name, mode:$m.mode,
     elapsedSeconds:($sum.elapsedSeconds // ($now - $m.startedEpoch)),
     model:($turns[-1].model // $m.model),
     turns:($turns | length),
     files:([$ev[] | select(.e == "write" or .e == "edit") | .path] | unique),
     bashRuns:([$ev[] | select(.e == "bash_done")] | length),
     failedBashRuns:([$ev[] | select(.e == "bash_done" and .ok == false)] | length),
     toolErrors:([$ev[] | select(.e == "tool_error")] | length)}
    + (if $state == "running" and $last != null then
         {last:($last.e + ([$last.cmd, $last.path, ($last.tool // empty) + " " + ($last.arg // ""),
                            $last.detail, ($last.count | if . then tostring else null end)]
                           | map(select(. != null and . != "")) | if length > 0 then ": " + .[0] else "" end)),
          idleSeconds:($now - ($last.at | fromdateiso8601))}
       else {} end)
    + (if $sum != null then {tokens:$sum.tokens} else {} end)
    + (if $exit != "" then {exitCode:($exit | tonumber)} else {} end)
    + (if $result != "" then {result:$result, resultChars:$chars} else {} end)
    + (if $error != "" then {error:$error} else {} end)
    + {dir:$m.dir}
  '
}

show_progress() {
  local dir=$1 tag=$2 shown total
  [[ -f "$dir/stream.jsonl" ]] || return 0
  shown="$(cat "$dir/.progress" 2>/dev/null || echo 0)"
  total="$(wc -l < "$dir/stream.jsonl")"
  (( total > shown )) || return 0
  tail -n "+$((shown + 1))" "$dir/stream.jsonl" | head -n "$((total - shown))" |
    jq -c --arg run "$tag" '
      select(.e | IN("result", "summary", "settled", "thinking", "answering", "tool_call") | not) |
      select((.e == "bash_done" and .ok) or (.e == "agent_end" and .willRetry != true) | not) |
      if $run == "" then . else {run:$run} + . end
    ' 2>/dev/null || true
  echo "$total" > "$dir/.progress"
}

cmd_supervise() {
  local dir=$1 code=0
  local args=()
  echo $$ > "$dir/pid"
  trap ':' TERM INT HUP
  mapfile -d '' args < "$dir/core.args"
  "$CORE" "${args[@]}" "$dir/prompt.md" "$dir/events.jsonl" \
    > "$dir/stream.jsonl" 2> "$dir/stderr.log" || code=$?
  if [[ -f "$dir/events.jsonl" ]]; then
    jq -Rc 'fromjson? | select(.e == "summary")' "$dir/events.jsonl" 2>/dev/null |
      tail -n 1 > "$dir/summary.json" || true
    [[ -s "$dir/summary.json" ]] || rm -f "$dir/summary.json"
    jq -Rrn '[inputs | fromjson? | select(.e == "result")] | last | .text // empty' \
      "$dir/events.jsonl" > "$dir/result.md.tmp" 2>/dev/null || true
    if [[ -s "$dir/result.md.tmp" ]]; then
      mv "$dir/result.md.tmp" "$dir/result.md"
    else
      rm -f "$dir/result.md.tmp"
    fi
  fi
  echo "$code" > "$dir/exit_code"
}

prune_expired_runs() {
  local days=${PI_DELEGATE_KEEP_DAYS:-7} cutoff dir
  [[ "$days" =~ ^[0-9]+$ ]] && (( days > 0 )) || return 0
  cutoff=$(( $(date +%s) - days * 86400 ))
  while IFS= read -r dir; do
    [[ -f "$dir/.delivered" && -f "$dir/exit_code" ]] || continue
    (( $(stat -c %Y "$dir/exit_code") < cutoff )) && rm -rf -- "$dir"
  done < <(all_runs)
  return 0
}

STARTED_DIR=""
START_MAX=$DEFAULT_MAX

start_run() {
  [[ -z "${PI_DELEGATE_ACTIVE:-}" ]] || die "refusing nested delegation: already running inside a delegated Pi"
  local prompt_file="" prompt_text="" name="" provider="" model="" thinking=""
  local timeout=15m workdir=$PWD read_only=0 allow_parallel=0
  local words=()
  while (( $# )); do
    case "$1" in
      --prompt-file) need_value $# "${2:-}" "$1"; prompt_file=$2; shift 2 ;;
      --prompt) need_value $# "${2:-}" "$1"; prompt_text=$2; shift 2 ;;
      --name) need_value $# "${2:-}" "$1"; name=$2; shift 2 ;;
      --provider) need_value $# "${2:-}" "$1"; provider=$2; shift 2 ;;
      --model) need_value $# "${2:-}" "$1"; model=$2; shift 2 ;;
      --thinking) need_value $# "${2:-}" "$1"; thinking=$2; shift 2 ;;
      --timeout) need_value $# "${2:-}" "$1"; timeout=$2; shift 2 ;;
      --workdir) need_value $# "${2:-}" "$1"; workdir=$2; shift 2 ;;
      --max) need_value $# "${2:-}" "$1"; START_MAX="$(to_seconds "$2")"; shift 2 ;;
      --read-only) read_only=1; shift ;;
      --allow-parallel-writes) allow_parallel=1; shift ;;
      --) shift; words+=("$@"); break ;;
      -*) die "unknown option: $1" ;;
      *) words+=("$1"); shift ;;
    esac
  done

  local prompt
  if [[ "$prompt_file" == - ]]; then
    prompt="$(cat)"
  elif [[ -n "$prompt_file" ]]; then
    [[ -f "$prompt_file" ]] || die "prompt file does not exist: $prompt_file"
    prompt="$(<"$prompt_file")"
  elif [[ -n "$prompt_text" ]]; then
    prompt=$prompt_text
  else
    prompt="${words[*]:-}"
  fi
  [[ "$prompt" =~ [^[:space:]] ]] || die "empty prompt; pass --prompt-file, --prompt, or trailing text"
  if [[ "$timeout" =~ ^0+([.]0+)?[smhd]?$ ]] || [[ ! "$timeout" =~ ^[0-9]+([.][0-9]+)?[smhd]?$ ]]; then
    die "invalid positive timeout: $timeout"
  fi
  [[ -d "$workdir" ]] || die "workdir does not exist: $workdir"
  workdir="$(cd "$workdir" && pwd -P)"
  require_tools

  local mode=write
  (( read_only )) && mode=read-only
  local dir root start_lock_fd
  root="$(runs_root)"
  mkdir -p "$root"
  exec {start_lock_fd}> "$root/.start.lock"
  flock "$start_lock_fd"
  if [[ "$mode" == write ]] && (( ! allow_parallel )); then
    while IFS= read -r dir; do
      if [[ "$(jq -r '.mode + "\t" + .workdir' "$dir/meta.json")" == "write	$workdir" ]] &&
         is_active "$(run_state "$dir")"; then
        die "write run $(basename "$dir") is still active in $workdir; wait for it, use --read-only, or pass --allow-parallel-writes"
      fi
    done < <(all_runs)
  fi

  local slug id
  [[ -n "${PI_DELEGATE_RUNS:-}" || -f "$root/.gitignore" ]] || echo '*' > "$root/.gitignore"
  prune_expired_runs
  slug="$(printf '%s' "$name" | tr -cs 'A-Za-z0-9._-' '-' | sed 's/^-*//; s/-*$//' | cut -c1-40)"
  [[ -n "$slug" ]] || slug="$(printf '%04x' "$RANDOM")"
  id="$(date +%Y%m%d-%H%M%S)-$slug"
  while ! mkdir -m 700 "$root/$id" 2>/dev/null; do
    id="$(date +%Y%m%d-%H%M%S)-$slug-$(printf '%04x' "$RANDOM")"
  done
  dir="$root/$id"

  printf '%s\n' "$prompt" > "$dir/prompt.md"
  local core_args=(--timeout "$timeout" --workdir "$workdir")
  [[ -n "$provider" ]] && core_args+=(--provider "$provider")
  [[ -n "$model" ]] && core_args+=(--model "$model")
  [[ -n "$thinking" ]] && core_args+=(--thinking "$thinking")
  (( read_only )) && core_args+=(--read-only)
  printf '%s\0' "${core_args[@]}" > "$dir/core.args"

  local task
  task="$(printf '%s\n' "$prompt" | grep -m1 '[^[:space:]]' | cut -c1-120)"
  jq -n -c --arg run "$id" --arg dir "$dir" --arg workdir "$workdir" --arg mode "$mode" \
    --arg name "${name:-$task}" --arg provider "$provider" --arg model "$model" \
    --arg thinking "$thinking" --arg timeout "$timeout" \
    --arg startedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson startedEpoch "$(date +%s)" \
    --argjson startedNs "$(date +%s%N)" '
    {run:$run, dir:$dir, workdir:$workdir, mode:$mode, name:$name, timeout:$timeout,
     startedNs:$startedNs,
     provider:(if $provider == "" then null else $provider end),
     model:(if $model == "" then null else $model end),
     thinking:(if $thinking == "" then null else $thinking end),
     startedAt:$startedAt, startedEpoch:$startedEpoch}
  ' > "$dir/meta.json"

  setsid "$SELF" _supervise "$dir" < /dev/null > "$dir/supervisor.log" 2>&1 &
  local tries=0
  until [[ -s "$dir/pid" ]]; do
    (( ++tries <= 50 )) || {
      echo 1 > "$dir/exit_code"
      die "supervisor did not start; see $dir/supervisor.log"
    }
    sleep 0.1
  done
  flock -u "$start_lock_fd"
  exec {start_lock_fd}>&-
  STARTED_DIR=$dir
}

cmd_start() {
  start_run "$@"
  status_json "$STARTED_DIR"
  echo "pi-delegate: started $(basename "$STARTED_DIR"); next: $SELF wait $(basename "$STARTED_DIR")" >&2
}

print_result() {
  local dir=$1 full=$2 run chars
  run="$(basename "$dir")"
  chars="$(jq -Rrs 'length' "$dir/result.md")"
  printf '\n===== result: %s (%s chars) =====\n' "$run" "$chars"
  if (( full || chars <= RESULT_CHARS )); then
    cat "$dir/result.md"
  else
    printf '[showing the last %s chars; full answer: %s]\n...' "$RESULT_CHARS" "$dir/result.md"
    jq -Rrs --argjson n "$RESULT_CHARS" '.[-$n:]' "$dir/result.md"
  fi
  printf '\n===== end: %s =====\n' "$run"
}

cmd_wait() {
  local max=$DEFAULT_MAX quiet=0 show_result=1 all=0 full=0
  local refs=() dirs=()
  while (( $# )); do
    case "$1" in
      --max) need_value $# "${2:-}" "$1"; max="$(to_seconds "$2")"; shift 2 ;;
      --quiet) quiet=1; shift ;;
      --no-result) show_result=0; shift ;;
      --full) full=1; shift ;;
      --all) all=1; shift ;;
      -*) die "unknown option: $1" ;;
      *) refs+=("$1"); shift ;;
    esac
  done
  local dir ref
  if (( all )); then
    while IFS= read -r dir; do
      if [[ ! -f "$dir/.delivered" ]] || is_active "$(run_state "$dir")"; then
        dirs+=("$dir")
      fi
    done < <(all_runs)
    (( ${#dirs[@]} )) || { echo "pi-delegate: no active or undelivered runs" >&2; return 0; }
  else
    (( ${#refs[@]} )) || refs=(last)
    for ref in "${refs[@]}"; do
      dir="$(resolve_run "$ref")"
      dirs+=("$dir")
    done
  fi

  local deadline=$((SECONDS + max)) active tag=""
  while :; do
    active=0
    for dir in "${dirs[@]}"; do
      (( ${#dirs[@]} > 1 )) && tag="$(basename "$dir")"
      (( quiet )) || show_progress "$dir" "$tag"
      is_active "$(run_state "$dir")" && active=1
    done
    (( active && SECONDS < deadline )) || break
    sleep "$POLL_SECONDS"
  done

  local code=0 state
  local finished=()
  for dir in "${dirs[@]}"; do
    (( ${#dirs[@]} > 1 )) && tag="$(basename "$dir")"
    (( quiet )) || show_progress "$dir" "$tag"
    state="$(run_state "$dir")"
    status_json "$dir"
    if is_active "$state"; then
      code=$RUNNING_EXIT
    else
      finished+=("$dir")
      [[ "$state" == ok ]] || (( code )) || code=1
    fi
  done
  for dir in ${finished[@]+"${finished[@]}"}; do
    if [[ ! -s "$dir/result.md" ]]; then
      touch "$dir/.delivered"
    elif (( show_result )); then
      print_result "$dir" "$full"
      touch "$dir/.delivered"
    fi
  done
  if (( code == RUNNING_EXIT )); then
    echo "pi-delegate: still running; call wait again (exit $RUNNING_EXIT)" >&2
  fi
  return "$code"
}

cmd_run() {
  start_run "$@"
  echo "pi-delegate: started $(basename "$STARTED_DIR")" >&2
  cmd_wait --max "$START_MAX" "$STARTED_DIR"
}

cmd_status() {
  local dir ref
  if (( $# )); then
    for ref in "$@"; do
      dir="$(resolve_run "$ref")"
      status_json "$dir"
    done
  else
    while IFS= read -r dir; do
      status_json "$dir"
    done < <(all_runs)
  fi
}

cmd_result() {
  local ref=last path_only=0 dir
  while (( $# )); do
    case "$1" in
      --path) path_only=1; shift ;;
      -*) die "unknown option: $1" ;;
      *) ref=$1; shift ;;
    esac
  done
  dir="$(resolve_run "$ref")"
  if [[ ! -s "$dir/result.md" ]]; then
    echo "pi-delegate: no result for $(basename "$dir") (state: $(run_state "$dir"))" >&2
    return 1
  fi
  if (( path_only )); then
    printf '%s\n' "$dir/result.md"
  else
    cat "$dir/result.md"
  fi
  is_active "$(run_state "$dir")" || touch "$dir/.delivered"
}

session_pids() {
  ps -eo pid=,sid= | awk -v sid="$1" '$2 == sid && $1 != sid {print $1}'
}

cmd_stop() {
  (( $# )) || die "stop requires at least one run"
  local ref dir pid tries
  for ref in "$@"; do
    dir="$(resolve_run "$ref")"
    if is_active "$(run_state "$dir")" && pid="$(cat "$dir/pid" 2>/dev/null)"; then
      touch "$dir/stopped"
      session_pids "$pid" | xargs -r kill -TERM 2>/dev/null || true
      tries=0
      while [[ ! -f "$dir/exit_code" ]] && (( ++tries <= 40 )); do
        (( tries == 25 )) && { session_pids "$pid" | xargs -r kill -KILL 2>/dev/null || true; }
        sleep 0.2
      done
    fi
    [[ -s "$dir/result.md" ]] || touch "$dir/.delivered"
    status_json "$dir"
  done
}

cmd_clean() {
  local finished=0 force=0 dir state
  local dirs=()
  while (( $# )); do
    case "$1" in
      --finished) finished=1; shift ;;
      --force) force=1; shift ;;
      -*) die "unknown option: $1" ;;
      *) dir="$(resolve_run "$1")"; dirs+=("$dir"); shift ;;
    esac
  done
  (( finished || ${#dirs[@]} )) || die "clean requires runs or --finished"
  if (( finished )); then
    while IFS= read -r dir; do
      if [[ -f "$dir/.delivered" ]] || (( force )); then
        dirs+=("$dir")
      elif ! is_active "$(run_state "$dir")"; then
        echo "pi-delegate: keep unreported run $(basename "$dir"); read it with wait/result or pass --force" >&2
      fi
    done < <(all_runs)
  fi
  for dir in ${dirs[@]+"${dirs[@]}"}; do
    [[ -f "$dir/meta.json" ]] || continue
    state="$(run_state "$dir")"
    if is_active "$state"; then
      echo "pi-delegate: skip active run $(basename "$dir")" >&2
      continue
    fi
    rm -rf -- "$dir"
    echo "removed $(basename "$dir") ($state)"
  done
}

main() {
  (( $# )) || usage
  local command=$1
  shift
  case "$command" in
    run) cmd_run "$@" ;;
    start) cmd_start "$@" ;;
    wait) cmd_wait "$@" ;;
    status|list) cmd_status "$@" ;;
    result) cmd_result "$@" ;;
    stop) cmd_stop "$@" ;;
    clean) cmd_clean "$@" ;;
    _supervise) cmd_supervise "$@" ;;
    -h|--help|help) usage ;;
    *) die "unknown command: $command (see --help)" ;;
  esac
}

# Runs outlive edits to this file; keep the call and exit on one line so bash never reads further.
main "$@"; exit
