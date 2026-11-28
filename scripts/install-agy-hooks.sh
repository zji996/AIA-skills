#!/usr/bin/env bash
set -euo pipefail

# Install or uninstall Antigravity CLI (agy) hooks for delegate into
# ~/.gemini/config/hooks.json (or a custom path via --hooks PATH).
# Idempotent: only manages the top-level "delegate" hook entry, preserves all
# other user hooks, and writes a .bak backup before modifying an existing file.

HOOKS="${HOME}/.gemini/config/hooks.json"
CUSTOM_HOOKS=0
ACTION="install"
STOP_ONLY=0

usage() {
  cat >&2 <<'EOF'
usage: install-agy-hooks.sh [--hooks PATH] [--stop-only] [--uninstall]

  --hooks PATH  Target agy hooks.json (default: ~/.gemini/config/hooks.json)
  --stop-only   Only configure/remove the Stop hook (leave PreToolUse untouched)
  --uninstall   Remove delegate hook entries from hooks.json
EOF
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --hooks)
      [ $# -ge 2 ] || usage
      HOOKS="$2"
      CUSTOM_HOOKS=1
      shift 2
      ;;
    --stop-only)
      STOP_ONLY=1
      shift
      ;;
    --uninstall)
      ACTION="uninstall"
      shift
      ;;
    -h|--help)
      usage
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage
      ;;
  esac
done

python3 - "$HOOKS" "$ACTION" "$STOP_ONLY" <<'PY'
import json
import os
import shutil
import sys
from pathlib import Path

hooks_path = Path(sys.argv[1]).expanduser()
action = sys.argv[2]
stop_only = sys.argv[3] == "1"

STOP_CMD = '"$HOME/.gemini/config/skills/delegate/bin/delegate" hook check --format agy'
BG_CMD = 'python3 "$HOME/.gemini/config/skills/delegate/hooks/claude-code-background.py" --format agy'

STOP_MARKERS = ("hook check --format agy", "claude-code-stop.py", "delegate hook stop")
BG_MARKERS = ("claude-code-background.py",)


def strip_flat_handlers(entries: list, markers: tuple[str, ...]) -> tuple[list, int]:
    """Remove flat handler objects (Stop) matching markers."""
    out = []
    removed = 0
    for entry in entries:
        if (
            isinstance(entry, dict)
            and isinstance(entry.get("command"), str)
            and any(m in entry["command"] for m in markers)
        ):
            removed += 1
            continue
        out.append(entry)
    return out, removed


def strip_grouped_handlers(entries: list, markers: tuple[str, ...]) -> tuple[list, int]:
    """Remove matching handlers inside grouped PreToolUse entries; drop empty groups."""
    out = []
    removed = 0
    for entry in entries:
        if not isinstance(entry, dict):
            out.append(entry)
            continue
        subhooks = entry.get("hooks")
        if isinstance(subhooks, list):
            kept = []
            for h in subhooks:
                if (
                    isinstance(h, dict)
                    and isinstance(h.get("command"), str)
                    and any(m in h["command"] for m in markers)
                ):
                    removed += 1
                else:
                    kept.append(h)
            if kept:
                updated = dict(entry)
                updated["hooks"] = kept
                out.append(updated)
            elif not subhooks:
                out.append(entry)
        elif isinstance(entry.get("command"), str) and any(m in entry["command"] for m in markers):
            removed += 1
        else:
            out.append(entry)
    return out, removed


old_raw = None
if hooks_path.is_file():
    old_raw = hooks_path.read_text(encoding="utf-8")
    stripped = old_raw.strip()
    data = json.loads(stripped) if stripped else {}
    if not isinstance(data, dict):
        sys.exit(f"[FAIL] {hooks_path} must contain a JSON object")
else:
    if action == "uninstall":
        print(f"Nothing to uninstall: {hooks_path} does not exist.")
        sys.exit(0)
    data = {}

delegate_hook = data.get("delegate")
if delegate_hook is None:
    delegate_hook = {}
if not isinstance(delegate_hook, dict):
    sys.exit(f"[FAIL] {hooks_path}: 'delegate' must be a JSON object")

stop_list = delegate_hook.get("Stop")
if stop_list is None:
    stop_list = []
if not isinstance(stop_list, list):
    sys.exit(f"[FAIL] {hooks_path}: 'delegate.Stop' must be a JSON array")

stop_list, removed_stop = strip_flat_handlers(stop_list, STOP_MARKERS)

if action == "install":
    stop_list.append({"type": "command", "command": STOP_CMD})
    delegate_hook["Stop"] = stop_list

    if not stop_only:
        pre_list = delegate_hook.get("PreToolUse")
        if pre_list is None:
            pre_list = []
        if not isinstance(pre_list, list):
            sys.exit(f"[FAIL] {hooks_path}: 'delegate.PreToolUse' must be a JSON array")
        pre_list, _ = strip_grouped_handlers(pre_list, BG_MARKERS)
        pre_list.append({
            "matcher": "run_command",
            "hooks": [{"type": "command", "command": BG_CMD}],
        })
        delegate_hook["PreToolUse"] = pre_list

    data["delegate"] = delegate_hook
else:
    if stop_list:
        delegate_hook["Stop"] = stop_list
    else:
        delegate_hook.pop("Stop", None)

    if not stop_only and "PreToolUse" in delegate_hook:
        pre_list = delegate_hook.get("PreToolUse")
        if not isinstance(pre_list, list):
            sys.exit(f"[FAIL] {hooks_path}: 'delegate.PreToolUse' must be a JSON array")
        pre_list, _ = strip_grouped_handlers(pre_list, BG_MARKERS)
        if pre_list:
            delegate_hook["PreToolUse"] = pre_list
        else:
            delegate_hook.pop("PreToolUse", None)

    if delegate_hook:
        data["delegate"] = delegate_hook
    else:
        data.pop("delegate", None)

new_raw = json.dumps(data, indent=2, ensure_ascii=False) + "\n"
if old_raw != new_raw:
    hooks_path.parent.mkdir(parents=True, exist_ok=True)
    if hooks_path.is_file():
        backup = hooks_path.with_name(hooks_path.name + ".bak")
        shutil.copy2(hooks_path, backup)
    tmp = hooks_path.with_name(hooks_path.name + ".tmp")
    tmp.write_text(new_raw, encoding="utf-8")
    os.replace(tmp, hooks_path)

if action == "install":
    note = f" (replaced {removed_stop} previous Stop entry/entries)" if removed_stop else ""
    print(f"Configured delegate agy hooks in {hooks_path}{note}.")
else:
    print(f"Removed delegate agy hooks from {hooks_path}.")
PY

# When installing into the user's real agy hooks, also run one-time idempotent
# migration of any legacy `<repo>/.local/delegate/deferred.tsv` into `<state>/work.jsonl`.
if [ "$ACTION" = "install" ] && [ "$CUSTOM_HOOKS" = "0" ]; then
  DELEGATE_BIN="${HOME}/.gemini/config/skills/delegate/bin/delegate"
  if [ ! -x "$DELEGATE_BIN" ]; then
    DELEGATE_BIN="$(command -v delegate 2>/dev/null || true)"
  fi
  if [ -n "$DELEGATE_BIN" ] && [ -x "$DELEGATE_BIN" ]; then
    "$DELEGATE_BIN" work --rebuild >/dev/null 2>&1 || true
  fi
fi
