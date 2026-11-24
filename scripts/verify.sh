#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
check_args=()
case "${AIA_SKILLS_STRICT_BUDGET:-0}" in
  1|true|TRUE|yes|YES) check_args=(--strict) ;;
esac
for arg in "$@"; do
  case "$arg" in
    --strict|--strict-budget) check_args=(--strict) ;;
    *)
      echo "Unknown option: $arg (supported: --strict)" >&2
      exit 2
      ;;
  esac
done
# Each step is skipped when its inputs match the last pass (iteration-speed step); a push right
# after a local verify reuses it. ITERATION_SPEED_FORCE=1 reruns everything.
STEP="$REPO_ROOT/skills/iteration-speed/scripts/iteration-speed"
step() {
  local name=$1
  shift
  if [ ! -x "$STEP" ]; then
    "$@"
    return
  fi
  # Release checksums and build output do not change what the checks see.
  "$STEP" step --name "aia-skills $name" --exclude 'skills/*/bin.sha256' --exclude dist \
    --key "python3 --version" --key "cargo --version" -- "$@"
}
step check scripts/check.sh "${check_args[@]}"
step tests python3 scripts/run-tests.py
if command -v cargo >/dev/null 2>&1; then
  step clippy bash -c 'cd crates/delegate && cargo clippy -- -D warnings'
  if [ -d "$REPO_ROOT/crates/delegate-tui" ]; then
    step tui bash -c 'cd crates/delegate-tui && cargo test && cargo clippy --all-targets -- -D warnings'
  fi
fi
