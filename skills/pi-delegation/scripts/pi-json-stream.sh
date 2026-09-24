#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 [--provider <name>] [--model <name>] [--thinking <level>] [--read-only] [--timeout <duration>] [--workdir <dir>] <prompt-file> <events-log>" >&2
  echo "       $0 [--provider <name>] [--model <name>] [--thinking <level>] [--read-only] <timeout> [<workdir>] <prompt-file> <events-log>  (legacy)" >&2
  exit 2
}


main() {
  if [[ -n "${PI_DELEGATE_ACTIVE:-}" ]]; then
    echo "Refusing nested delegation: already running inside a delegated Pi" >&2
    exit 2
  fi
  provider=""
  model=""
  thinking=""
  read_only=0
  duration=15m
  workdir=$PWD
  duration_option=0
  workdir_option=0
  while (( $# )); do
    case "$1" in
      --provider|--model|--thinking|--timeout|--workdir)
        (( $# >= 2 )) && [[ -n "$2" ]] || usage
        case "$1" in
          --provider) provider=$2 ;;
          --model) model=$2 ;;
          --thinking) thinking=$2 ;;
          --timeout) duration=$2; duration_option=1 ;;
          --workdir) workdir=$2; workdir_option=1 ;;
        esac
        shift 2
        ;;
      --read-only)
        read_only=1
        shift
        ;;
      --)
        shift
        break
        ;;
      -*) usage ;;
      *) break ;;
    esac
  done

  case $# in
    2)
      prompt_file=$1
      events_log=$2
      ;;
    3)
      (( ! duration_option && ! workdir_option )) || usage
      duration=$1
      prompt_file=$2
      events_log=$3
      ;;
    4)
      (( ! duration_option && ! workdir_option )) || usage
      duration=$1
      workdir=$2
      prompt_file=$3
      events_log=$4
      ;;
    *)
      usage
      ;;
  esac

  if [[ ! "$duration" =~ ^[0-9]+([.][0-9]+)?[smhd]?$ ]] || [[ "$duration" =~ ^0+([.]0+)?[smhd]?$ ]]; then
    echo "Invalid positive timeout duration: $duration" >&2
    exit 2
  fi

  if [[ ! -d "$workdir" ]]; then
    echo "Pi working directory does not exist: $workdir" >&2
    exit 2
  fi
  workdir="$(cd "$workdir" && pwd -P)"

  resolve_from_workdir() {
    local path=$1
    if [[ "$path" = /* ]]; then
      printf '%s\n' "$path"
    else
      printf '%s/%s\n' "$workdir" "$path"
    fi
  }

  prompt_file="$(resolve_from_workdir "$prompt_file")"
  events_log="$(resolve_from_workdir "$events_log")"

  if [[ "$prompt_file" -ef "$events_log" ]]; then
    echo "Pi prompt and events log must be different files" >&2
    exit 2
  fi

  if [[ ! -f "$prompt_file" ]]; then
    echo "Pi prompt file does not exist: $prompt_file" >&2
    exit 2
  fi
  command -v pi >/dev/null || { echo "pi is not installed; see pi-delegation SKILL.md for the pi-kit installer" >&2; exit 2; }
  command -v jq >/dev/null || { echo "jq is not installed" >&2; exit 2; }

  pi_args=(--no-session --mode json)
  if [[ -n "$provider" ]]; then pi_args+=(--provider "$provider"); fi
  if [[ -n "$model" ]]; then pi_args+=(--model "$model"); fi
  if [[ -n "$thinking" ]]; then pi_args+=(--thinking "$thinking"); fi
  if (( read_only )); then pi_args+=(--tools read,grep,find,ls); fi

  mkdir -p "$(dirname "$events_log")"
  (umask 077 && : > "$events_log")

  start_seconds=$SECONDS
  pipeline_status=0
  (
    cd "$workdir"
    export PI_DELEGATE_ACTIVE=1
    timeout --kill-after=5s "$duration" pi "${pi_args[@]}" -p < "$prompt_file"
  ) |
    jq --unbuffered -c '
      def clip($n):
        tostring |
        if length > $n then .[0:$n] + "..." else . end;

      (if .type == "tool_execution_start" then
        if .toolName == "bash" then
          {e:"bash", cmd:((.args.command // "") |
            if test("^node[[:space:]]+-e") then "node -e [inline script]" else clip(180) end)}
        elif (.toolName == "edit" or .toolName == "write") then
          {e:.toolName, path:(.args.path // "")}
        elif .toolName == "read" then
          {e:"read", path:(.args.path // "")}
        else
          {e:"tool", tool:.toolName,
           arg:([.args.pattern, .args.path] | map(select(. != null and . != "")) | join(" @ ") | clip(160))}
        end
      elif .type == "tool_execution_end" then
        (if .toolName == "bash" then {e:"bash_done", ok:(.isError != true)} else empty end),
        (if .isError then
          {e:"tool_error", tool:.toolName,
           detail:((.result.content[0].text // "") | split("\n") | map(select(test("\\S"))) |
                   (map(select(test("^Command exited with code") | not)) | last) as $cause |
                   ((last // "") as $exit |
                    if $cause and $cause != $exit then ($cause | clip(240)) + " (" + $exit + ")" else $exit end))}
         else empty end)
      elif .type == "message_end" and .message.role == "assistant" then
        ([.message.content[]? | select(.type == "text") | .text] | join("\n")) as $answer |
        ($answer | gsub("\\s"; "") | length > 0) as $has_answer |
        {e:"turn", provider:.message.provider, model:.message.model,
         stopReason:.message.stopReason,
         hasResult:(.message.stopReason == "stop" and $has_answer),
         usage:(.message.usage | {input, output, cacheRead})},
        (if .message.stopReason == "stop" and $has_answer then
          {e:"result", text:$answer}
         else empty end)
      elif .type == "message_update" then
        ({thinking_start:"thinking", text_start:"answering", toolcall_start:"tool_call"}
         [.assistantMessageEvent.type // ""]) as $phase |
        if $phase then {e:$phase} else empty end
      elif .type == "auto_retry_start" then
        {e:"retry", attempt, maxAttempts,
         errorMessage:((.errorMessage // "") | clip(300))}
      elif (.type == "compaction_start" or .type == "compaction_end") then
        {e:.type, reason}
      elif .type == "agent_end" then
        {e:"agent_end", willRetry:(.willRetry // false)}
      elif .type == "agent_settled" then
        {e:"settled"}
      else
        empty
      end) | . + {at:(now | todateiso8601)}
    ' | tee "$events_log" |
    jq -n --unbuffered -c '
      foreach inputs as $event (
        {checks:0, nodeCheck:false, show:null};
        if $event.e == "bash" and $event.cmd == "node -e [inline script]" then
          .checks += 1 |
          .nodeCheck = true |
          .show = (if .checks == 1 or (.checks % 5) == 0 then
            {e:"checks", count:.checks, at:$event.at}
          else null end)
        elif $event.e == "bash" then
          .nodeCheck = false | .show = $event
        elif $event.e == "bash_done" and .nodeCheck then
          .show = (if ($event.ok == false) or .checks == 1 or (.checks % 5) == 0 then
            {e:"check_done", count:.checks, ok:$event.ok, at:$event.at}
          else null end)
        elif $event.e == "result" and ($event.text | length) > 800 then
          .show = {e:"result", text:("..." + $event.text[-800:]), truncated:true, at:$event.at}
        elif ($event.e == "turn" and $event.stopReason == "toolUse") or
             ($event.e | IN("thinking", "answering", "tool_call")) then
          .show = null
        else
          .show = $event
        end;
        .show // empty
      )
    ' || pipeline_status=$?

  status=ok
  if (( pipeline_status != 0 )); then
    status=failed
    if (( pipeline_status == 124 )); then
      status=timeout
      echo "Pi timed out after $duration" >&2
    elif (( pipeline_status == 137 )); then
      status=killed
      echo "Pi was killed (exit 137); inspect process or upstream timeout logs" >&2
    fi
  elif ! jq -s -e 'any(.[]; .e == "settled") and (([.[] | select(.e == "turn")] | last) | .stopReason == "stop" and .hasResult == true)' "$events_log" >/dev/null; then
    status=failed
    echo "Pi did not settle with a successful nonempty final result" >&2
  fi

  jq -s -c --arg status "$status" --argjson elapsed "$((SECONDS - start_seconds))" '
    [.[] | select(.e == "turn")] as $turns |
    {e:"summary", status:$status, elapsedSeconds:$elapsed,
     provider:($turns[-1].provider // null), model:($turns[-1].model // null),
     turns:($turns | length),
     writes:([.[] | select(.e == "write" or .e == "edit")] | length),
     bashRuns:([.[] | select(.e == "bash_done")] | length),
     failedBashRuns:([.[] | select(.e == "bash_done" and .ok == false)] | length),
     toolErrors:([.[] | select(.e == "tool_error")] | length),
     tokens:{input:([$turns[].usage.input] | add // 0),
             output:([$turns[].usage.output] | add // 0),
             cacheRead:([$turns[].usage.cacheRead] | add // 0)},
     at:(now | todateiso8601)}
  ' "$events_log" | tee -a "$events_log"

  if (( pipeline_status != 0 )); then
    exit "$pipeline_status"
  fi
  if [[ "$status" != ok ]]; then
    exit 1
  fi
}

main "$@"; exit
