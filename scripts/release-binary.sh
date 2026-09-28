#!/usr/bin/env bash
# Release a skill's binary: static musl builds for x86_64 and aarch64, checksums committed in the skill,
# assets on the GitHub release tagged <name>-v<version> (and on Forgejo when a token is given).
#
#   release-binary.sh build <skill>     build into dist/ and write skills/<skill>/bin.sha256
#   (commit bin.sha256 with the version bump and push)
#   release-binary.sh publish <skill>   tag that commit and upload dist/ to the releases
#
# publish needs `gh` logged in for GitHub (the primary download source). FORGEJO_TOKEN (write:repository)
# also publishes to the Forgejo instance, which fetch-binary.sh tries second; without it that is skipped.
# Agent shells often do not inherit it from ~/.bashrc, so it is read from FORGEJO_TOKEN_FILE when unset.
# Keep the token on one host: elsewhere, FORGEJO_TOKEN_SSH=<user@host> (or the same text in
# ~/.config/aia-skills/forgejo-token-ssh) reads that host's FORGEJO_TOKEN_FILE path over SSH at publish time,
# so the token is never stored on the releasing machine.
set -euo pipefail

# Test fixtures may override the checkout; normal releases use this script's repository.
REPO_ROOT="${AIA_SKILLS_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
FORGEJO_API="${FORGEJO_API:-https://git.aiatechco.com:31443/api/v1/repos/zji996/AIA-skills}"
GITHUB_REPO="${GITHUB_REPO:-zji996/AIA-skills}"
FORGEJO_TOKEN_FILE="${FORGEJO_TOKEN_FILE:-$HOME/.config/edge-gateway/forgejo-repoctl-token}"
if [ -z "${FORGEJO_TOKEN:-}" ] && [ -r "$FORGEJO_TOKEN_FILE" ]; then
  FORGEJO_TOKEN="$(tr -d '\r\n' < "$FORGEJO_TOKEN_FILE")"
fi
FORGEJO_TOKEN_SSH_FILE="$HOME/.config/aia-skills/forgejo-token-ssh"
if [ -z "${FORGEJO_TOKEN_SSH:-}" ] && [ -r "$FORGEJO_TOKEN_SSH_FILE" ]; then
  FORGEJO_TOKEN_SSH="$(tr -d '\r\n' < "$FORGEJO_TOKEN_SSH_FILE")"
fi

# Called only by publish: builds never touch the network for the token.
forgejo_token_over_ssh() {
  [ -z "${FORGEJO_TOKEN:-}" ] && [ -n "${FORGEJO_TOKEN_SSH:-}" ] || return 0
  local remote_file="${FORGEJO_TOKEN_FILE#"$HOME"/}"
  # The remote shell expands ~; the path is the same default location relative to that host's home.
  # shellcheck disable=SC2029
  FORGEJO_TOKEN="$(ssh -o BatchMode=yes -o ConnectTimeout=5 "$FORGEJO_TOKEN_SSH" "tr -d '\r\n' < ~/$remote_file" 2>/dev/null)" \
    || { FORGEJO_TOKEN=""; echo "  [Forgejo] could not read the token from $FORGEJO_TOKEN_SSH" >&2; }
}

usage() { echo "usage: release-binary.sh build|verify|publish <skill>" >&2; exit 2; }
(( $# == 2 )) || usage
ACTION=$1 SKILL=$2
DIR="$REPO_ROOT/skills/$SKILL"
[ -f "$DIR/SKILL.md" ] || { echo "Invalid or missing skill: $SKILL" >&2; exit 2; }
frontmatter_value() {
  sed -n '/^---$/,/^---$/p' "$1" | sed -n "s/^[[:space:]]*$2:[[:space:]]*//p" | head -1 | tr -d '"'
}
NAME="$(frontmatter_value "$DIR/SKILL.md" binary)"
VERSION="$(frontmatter_value "$DIR/SKILL.md" version)"
[ -n "$NAME" ] || { echo "skills/$SKILL declares no binary" >&2; exit 2; }
TAG="$NAME-v$VERSION"
DIST="$REPO_ROOT/dist/$TAG"

build() {
  local crate="$REPO_ROOT/crates/$NAME" target
  rm -rf "$DIST" && mkdir -p "$DIST"
  for target in "${TARGETS[@]}"; do
    rustup target add "$target" >/dev/null
    # Pure Rust with no C dependencies: rust-lld links a static musl binary for any target.
    (cd "$crate" && env "CARGO_TARGET_$(echo "$target" | tr 'a-z-' 'A-Z_')_LINKER=rust-lld" \
      cargo build --release --locked -q --target "$target")
    install -m 755 "$crate/target/$target/release/$NAME" "$DIST/$NAME-$target"
  done
  (cd "$DIST" && sha256sum "$NAME"-*) > "$DIR/bin.sha256"
  echo "Built $TAG into ${DIST#"$REPO_ROOT"/}:"
  cat "$DIR/bin.sha256"
  echo "Next: commit skills/$SKILL/bin.sha256 with the version bump, push, then: $0 publish $SKILL"
}

verify() {
  local crate="$REPO_ROOT/crates/$NAME" cargo_version head tag_commit
  [ -f "$crate/Cargo.toml" ] && [ -f "$crate/Cargo.lock" ] || {
    echo "Missing crates/$NAME/Cargo.toml or Cargo.lock" >&2; return 1;
  }
  cargo_version="$(python3 - "$crate/Cargo.toml" <<'PY'
import sys
import tomllib
with open(sys.argv[1], 'rb') as source:
    print(tomllib.load(source)['package']['version'])
PY
)" || return 1
  [ "$VERSION" = "$cargo_version" ] || {
    echo "SKILL.md version $VERSION differs from crates/$NAME/Cargo.toml $cargo_version" >&2; return 1;
  }
  [ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=all -- "skills/$SKILL" "crates/$NAME")" ] || {
    echo "Commit all changes in skills/$SKILL and crates/$NAME before publishing" >&2; return 1;
  }
  git -C "$REPO_ROOT" ls-files --error-unmatch -- "skills/$SKILL/bin.sha256" "crates/$NAME/Cargo.lock" >/dev/null || {
    echo "Commit bin.sha256 and Cargo.lock before publishing" >&2; return 1;
  }
  [ -d "$DIST" ] || { echo "Nothing built for $TAG: run $0 build $SKILL first" >&2; return 1; }
  (cd "$DIST" && sha256sum -c --quiet "$DIR/bin.sha256") || {
    echo "dist/ does not match committed bin.sha256" >&2; return 1;
  }
  head="$(git -C "$REPO_ROOT" rev-parse HEAD)" || return 1
  if git -C "$REPO_ROOT" show-ref --verify --quiet "refs/tags/$TAG"; then
    tag_commit="$(git -C "$REPO_ROOT" rev-parse "refs/tags/$TAG^{}")" || return 1
    [ "$tag_commit" = "$head" ] || { echo "Tag $TAG does not point to HEAD" >&2; return 1; }
  fi
  echo "Verified $TAG at $head"
}

publish() {
  local file notes
  verify
  if ! git -C "$REPO_ROOT" rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    git -C "$REPO_ROOT" tag -a "$TAG" -m "$NAME $VERSION"
  fi
  git -C "$REPO_ROOT" push -q origin "$TAG"
  notes="$NAME $VERSION: static Linux builds (musl). Checksums: skills/$SKILL/bin.sha256 at $TAG."
  if gh auth status >/dev/null 2>&1; then
    # The GitHub mirror receives the tag from the primary repository; wait for it rather than race.
    for _ in $(seq 60); do
      gh api "repos/$GITHUB_REPO/git/ref/tags/$TAG" >/dev/null 2>&1 && break
      sleep 5
    done
    if gh release view "$TAG" -R "$GITHUB_REPO" >/dev/null 2>&1; then
      gh release upload "$TAG" "$DIST"/* -R "$GITHUB_REPO" --clobber
    else
      gh release create "$TAG" "$DIST"/* -R "$GITHUB_REPO" --title "$NAME $VERSION" --notes "$notes" --verify-tag
    fi
    echo "  [GitHub] https://github.com/$GITHUB_REPO/releases/tag/$TAG"
  else
    echo "  [GitHub] skipped: gh is not logged in"
  fi
  forgejo_token_over_ssh
  if [ -n "${FORGEJO_TOKEN:-}" ]; then
    local auth=(-H "Authorization: token $FORGEJO_TOKEN") id
    id="$(curl -fsS "${auth[@]}" "$FORGEJO_API/releases/tags/$TAG" 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])' 2>/dev/null)" || true
    if [ -z "$id" ]; then
      id="$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" -X POST "$FORGEJO_API/releases" \
        -d "{\"tag_name\": \"$TAG\", \"name\": \"$NAME $VERSION\", \"body\": \"$notes\"}" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
    fi
    # Forgejo keeps same-named attachments side by side: upload only what the release lacks, so publishing
    # again (e.g. after adding a token) is safe. Contents are pinned by bin.sha256, never replaced here.
    local attached
    attached="$(curl -fsS "${auth[@]}" "$FORGEJO_API/releases/$id" \
      | python3 -c 'import json,sys; print("\n".join(a["name"] for a in json.load(sys.stdin)["assets"]))')"
    for file in "$DIST"/*; do
      if grep -qxF "$(basename "$file")" <<<"$attached"; then
        echo "  [Forgejo] already attached: $(basename "$file")"
        continue
      fi
      curl -fsS "${auth[@]}" -X POST "$FORGEJO_API/releases/$id/assets?name=$(basename "$file")" \
        -F "attachment=@$file" >/dev/null || { echo "  [Forgejo] upload failed: $(basename "$file")" >&2; return 1; }
    done
    echo "  [Forgejo] ${FORGEJO_API%/api/v1/repos/*}/zji996/AIA-skills/releases/tag/$TAG"
  else
    echo "  [Forgejo] skipped: set FORGEJO_TOKEN, FORGEJO_TOKEN_FILE or FORGEJO_TOKEN_SSH to publish there too (optional; GitHub is the primary source)"
  fi
}

case "$ACTION" in
  build) build ;;
  verify) verify ;;
  publish) publish ;;
  *) usage ;;
esac
