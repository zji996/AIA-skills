#!/usr/bin/env bash
set -euo pipefail

# Install or uninstall Claude Code hooks for delegate into ~/.claude/settings.json
# (or a custom path via --settings PATH). Replaces legacy delegate-unmerged.sh and
# delegate-waiter.sh Stop hook entries with skills/delegate/hooks/claude-code-stop.py.

SETTINGS="${HOME}/.claude/settings.json"
CUSTOM_SETTINGS=0
ACTION="install"
STOP_ONLY=0

usage() {
  cat >&2 <<'EOF'
usage: install-claude-hooks.sh [--settings PATH] [--stop-only] [--uninstall]

  --settings PATH  Target Claude Code settings.json (default: ~/.claude/settings.json)
  --stop-only      Only configure the Stop hook (leave PreToolUse untouched)
  --uninstall      Remove delegate hook entries from settings.json
EOF
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --settings)
      [ $# -ge 2 ] || usage
      SETTINGS="$2"
      CUSTOM_SETTINGS=1
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

python3 - "$SETTINGS" "$ACTION" "$STOP_ONLY" <<'PY'
import json
import os
import sys
from pathlib import Path

settings_path = Path(sys.argv[1]).expanduser()
action = sys.argv[2]
stop_only = sys.argv[3] == "1"

STOP_CMD = 'python3 "$HOME/.claude/skills/delegate/hooks/claude-code-stop.py"'
BG_CMD = 'python3 "$HOME/.claude/skills/delegate/hooks/claude-code-background.py"'
LEGACY_SHELL_MARKERS = ("delegate-unmerged.sh", "delegate-waiter.sh")
LEGACY_STOP_MARKERS = (*LEGACY_SHELL_MARKERS, "claude-code-stop.py", "delegate hook stop")
BG_MARKERS = ("claude-code-background.py",)


def entry_has_marker(entry: object, markers: tuple[str, ...]) -> bool:
    if not isinstance(entry, dict):
        return False
    cmd = entry.get("command")
    if isinstance(cmd, str) and any(m in cmd for m in markers):
        return True
    subhooks = entry.get("hooks")
    if isinstance(subhooks, list):
        return any(
            isinstance(h, dict)
            and isinstance(h.get("command"), str)
            and any(m in h["command"] for m in markers)
            for h in subhooks
        )
    return False


def strip_matching_commands(entries: list, markers: tuple[str, ...]) -> tuple[list, int]:
    """Remove individual hook commands matching markers; drop empty wrapper entries."""
    out = []
    removed = 0
    for entry in entries:
        if not isinstance(entry, dict):
            out.append(entry)
            continue
        if isinstance(entry.get("command"), str) and any(m in entry["command"] for m in markers):
            removed += 1
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
        else:
            out.append(entry)
    return out, removed


if settings_path.is_file():
    raw = settings_path.read_text(encoding="utf-8").strip()
    data = json.loads(raw) if raw else {}
    if not isinstance(data, dict):
        sys.exit(f"[FAIL] {settings_path} must contain a JSON object")
else:
    if action == "uninstall":
        print(f"Nothing to uninstall: {settings_path} does not exist.")
        sys.exit(0)
    data = {}

hooks = data.get("hooks")
if hooks is None:
    hooks = {}
if not isinstance(hooks, dict):
    sys.exit(f"[FAIL] {settings_path}: 'hooks' must be a JSON object")

stop_list = hooks.get("Stop")
if stop_list is None:
    stop_list = []
if not isinstance(stop_list, list):
    sys.exit(f"[FAIL] {settings_path}: 'hooks.Stop' must be a JSON array")

stop_list, removed_stop = strip_matching_commands(stop_list, LEGACY_STOP_MARKERS)

if action == "install":
    stop_list.append({"hooks": [{"type": "command", "command": STOP_CMD}]})
    hooks["Stop"] = stop_list

    if not stop_only:
        pre_list = hooks.get("PreToolUse")
        if pre_list is None:
            pre_list = []
        if not isinstance(pre_list, list):
            sys.exit(f"[FAIL] {settings_path}: 'hooks.PreToolUse' must be a JSON array")
        if not any(entry_has_marker(e, BG_MARKERS) for e in pre_list):
            pre_list.append({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": BG_CMD}],
            })
        hooks["PreToolUse"] = pre_list
else:
    if stop_list:
        hooks["Stop"] = stop_list
    else:
        hooks.pop("Stop", None)
    if not stop_only and isinstance(hooks.get("PreToolUse"), list):
        pre_list, _ = strip_matching_commands(hooks["PreToolUse"], BG_MARKERS)
        if pre_list:
            hooks["PreToolUse"] = pre_list
        else:
            hooks.pop("PreToolUse", None)

if hooks:
    data["hooks"] = hooks
else:
    data.pop("hooks", None)

settings_path.parent.mkdir(parents=True, exist_ok=True)
tmp = settings_path.with_name(settings_path.name + ".tmp")
tmp.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
os.replace(tmp, settings_path)

if action == "install":
    note = f" (replaced {removed_stop} previous Stop entry/entries)" if removed_stop else ""
    print(f"Configured unified delegate Claude Code Stop hook in {settings_path}{note}.")
else:
    print(f"Removed delegate Claude Code hooks from {settings_path}.")
PY

# When installing into the user's real Claude settings, also run one-time idempotent
# migration of any legacy `<repo>/.local/delegate/deferred.tsv` into `<state>/work.jsonl`.
if [ "$ACTION" = "install" ] && [ "$CUSTOM_SETTINGS" = "0" ]; then
  DELEGATE_BIN="${HOME}/.claude/skills/delegate/bin/delegate"
  if [ ! -x "$DELEGATE_BIN" ]; then
    DELEGATE_BIN="$(command -v delegate 2>/dev/null || true)"
  fi
  if [ -n "$DELEGATE_BIN" ] && [ -x "$DELEGATE_BIN" ]; then
    "$DELEGATE_BIN" work --rebuild >/dev/null 2>&1 || true
  fi
fi
