#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
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
step check scripts/check.sh
step tests python3 scripts/run-tests.py
if command -v cargo >/dev/null 2>&1; then
  step clippy bash -c 'cd crates/delegate && cargo clippy -- -D warnings'
fi
