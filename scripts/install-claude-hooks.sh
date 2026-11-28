#!/usr/bin/env bash
set -euo pipefail

# Thin wrapper around `delegate install-hooks --host claude`.
# Keeps the legacy CLI interface (`--settings`, `--stop-only`, `--uninstall`) intact.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SETTINGS=""
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

find_delegate_bin() {
  local cand
  for cand in \
    "${DELEGATE_BIN:-}" \
    "$REPO_ROOT/skills/delegate/bin/delegate" \
    "${CARGO_TARGET_DIR:-$REPO_ROOT/crates/delegate/target}/debug/delegate" \
    "$HOME/.claude/skills/delegate/bin/delegate" \
    "$HOME/.agents/skills/delegate/bin/delegate" \
    "$(command -v delegate 2>/dev/null || true)"; do
    if [ -n "$cand" ] && [ -x "$cand" ]; then
      printf '%s\n' "$cand"
      return 0
    fi
  done
  return 1
}

BIN="$(find_delegate_bin || true)"
if [ -z "$BIN" ]; then
  echo "[FAIL] delegate binary not found; run scripts/install.sh or set DELEGATE_BIN" >&2
  exit 1
fi

ARGS=(install-hooks --host claude)
[ "$CUSTOM_SETTINGS" = "1" ] && ARGS+=(--path "$SETTINGS")
[ "$STOP_ONLY" = "1" ] && ARGS+=(--stop-only)
[ "$ACTION" = "uninstall" ] && ARGS+=(--uninstall)

"$BIN" "${ARGS[@]}"

# When installing into the user's real Claude settings, also run one-time idempotent
# migration of any legacy `<repo>/.local/delegate/deferred.tsv` into `<state>/work.jsonl`.
if [ "$ACTION" = "install" ] && [ "$CUSTOM_SETTINGS" = "0" ]; then
  "$BIN" work --rebuild >/dev/null 2>&1 || true
fi
