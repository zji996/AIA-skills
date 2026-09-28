#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
git -C "$REPO_ROOT" rev-parse --show-toplevel >/dev/null
git -C "$REPO_ROOT" config --local core.hooksPath .githooks
echo "Installed repository pre-push hook (.githooks)."
