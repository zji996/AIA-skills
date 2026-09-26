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
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
FORGEJO_API="${FORGEJO_API:-https://git.aiatechco.com:31443/api/v1/repos/zji996/AIA-skills}"
GITHUB_REPO="${GITHUB_REPO:-zji996/AIA-skills}"

usage() { echo "usage: release-binary.sh build|publish <skill>" >&2; exit 2; }
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

publish() {
  local file notes
  [ -d "$DIST" ] || { echo "Nothing built for $TAG: run $0 build $SKILL first" >&2; exit 1; }
  (cd "$DIST" && sha256sum -c --quiet "$DIR/bin.sha256") || { echo "dist/ does not match bin.sha256" >&2; exit 1; }
  git -C "$REPO_ROOT" diff --quiet HEAD -- "$DIR/bin.sha256" \
    || { echo "Commit skills/$SKILL/bin.sha256 before publishing" >&2; exit 1; }
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
  if [ -n "${FORGEJO_TOKEN:-}" ]; then
    local auth=(-H "Authorization: token $FORGEJO_TOKEN") id
    id="$(curl -fsS "${auth[@]}" "$FORGEJO_API/releases/tags/$TAG" 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])' 2>/dev/null)" || true
    if [ -z "$id" ]; then
      id="$(curl -fsS "${auth[@]}" -H "Content-Type: application/json" -X POST "$FORGEJO_API/releases" \
        -d "{\"tag_name\": \"$TAG\", \"name\": \"$NAME $VERSION\", \"body\": \"$notes\"}" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
    fi
    for file in "$DIST"/*; do
      curl -fsS "${auth[@]}" -X POST "$FORGEJO_API/releases/$id/assets?name=$(basename "$file")" \
        -F "attachment=@$file" >/dev/null || echo "  [Forgejo] upload failed (already there?): $(basename "$file")"
    done
    echo "  [Forgejo] ${FORGEJO_API%/api/v1/repos/*}/zji996/AIA-skills/releases/tag/$TAG"
  else
    echo "  [Forgejo] skipped: set FORGEJO_TOKEN to publish there too (optional; GitHub is the primary source)"
  fi
}

case "$ACTION" in
  build) build ;;
  publish) publish ;;
  *) usage ;;
esac
