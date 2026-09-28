#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"$REPO_ROOT/scripts/check.sh"
(cd "$REPO_ROOT" && python3 -m unittest discover -s tests)
if command -v cargo >/dev/null 2>&1; then
  (cd "$REPO_ROOT/crates/delegate" && cargo clippy -- -D warnings)
fi
