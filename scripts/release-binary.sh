#!/usr/bin/env bash
# Release a skill's binary: static musl builds for x86_64 and aarch64, checksums committed in the skill,
# assets on the GitHub release tagged <name>-v<version> (and on Forgejo when a token is given).
#
#   release-binary.sh bump <skill> <ver|patch|minor|major> [--allow-empty]
#                                       bump SKILL.md + crates/*/Cargo.{toml,lock} and turn CHANGELOG.md
#                                       ## Unreleased entries into a versioned section
#   release-binary.sh build <skill>     build into dist/ and write skills/<skill>/bin.sha256
#   (commit bin.sha256 with the version bump and push)
#   release-binary.sh publish <skill>   fetch origin, verify HEAD is on origin/main and tag matches HEAD,
#                                       then tag that commit and upload dist/ to the releases
#   release-binary.sh release <skill> <ver|patch|minor|major> [--allow-empty] [--from <step>]
#   release-binary.sh release <skill> --from <step>
#                                       one-step release: bump -> build -> commit -> push -> publish -> install -> sync
#                                       stops immediately on any failure and prints the --from <step> resume command
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

usage() {
  echo "usage: release-binary.sh bump <skill> <X.Y.Z|patch|minor|major> [--allow-empty]" >&2
  echo "       release-binary.sh build|verify|publish <skill>" >&2
  echo "       release-binary.sh release <skill> <X.Y.Z|patch|minor|major> [--allow-empty] [--from <step>]" >&2
  echo "       release-binary.sh release <skill> --from <step>" >&2
  exit 2
}
(( $# >= 2 )) || usage
ACTION=$1 SKILL=$2
DIR="$REPO_ROOT/skills/$SKILL"
[ -f "$DIR/SKILL.md" ] || { echo "Invalid or missing skill: $SKILL" >&2; exit 2; }
frontmatter_value() {
  sed -n '/^---$/,/^---$/p' "$1" | sed -n "s/^[[:space:]]*$2:[[:space:]]*//p" | head -1 | tr -d '"'
}
refresh_meta() {
  NAME="$(frontmatter_value "$DIR/SKILL.md" binary)"
  EXTRA_BINARIES="$(frontmatter_value "$DIR/SKILL.md" extra-binaries | tr ',[]' '   ')"
  VERSION="$(frontmatter_value "$DIR/SKILL.md" version)"
  BINARIES=()
  if [ -n "$NAME" ]; then
    BINARIES=("$NAME")
    local extra
    for extra in $EXTRA_BINARIES; do
      BINARIES+=("$extra")
    done
    TAG="$NAME-v$VERSION"
    DIST="$REPO_ROOT/dist/$TAG"
  else
    TAG=""
    DIST=""
  fi
}
refresh_meta

bump() {
  local target_spec="${1:-}" allow_empty="0"
  shift || true
  while (( $# > 0 )); do
    case "$1" in
      --allow-empty) allow_empty="1" ;;
      *) usage ;;
    esac
    shift
  done
  [ -n "$target_spec" ] || usage
  local bins=()
  if [ -n "$NAME" ]; then
    bins=("$NAME")
    for extra in $EXTRA_BINARIES; do
      bins+=("$extra")
    done
  fi
  python3 - "$REPO_ROOT" "$SKILL" "$VERSION" "$target_spec" "$allow_empty" "${bins[@]}" <<'PY'
import datetime
import re
import sys
from pathlib import Path

repo = Path(sys.argv[1])
skill = sys.argv[2]
old_ver = sys.argv[3]
spec = sys.argv[4]
allow_empty = sys.argv[5] == "1"
bins = sys.argv[6:]

m = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", old_ver)
if not m:
    sys.exit(f"Invalid current version in skills/{skill}/SKILL.md: {old_ver!r}")
maj, minor, patch = map(int, m.groups())
if spec == "patch":
    new_ver = f"{maj}.{minor}.{patch + 1}"
elif spec == "minor":
    new_ver = f"{maj}.{minor + 1}.0"
elif spec == "major":
    new_ver = f"{maj + 1}.0.0"
elif re.fullmatch(r"\d+\.\d+\.\d+", spec):
    new_ver = spec
else:
    sys.exit(f"Invalid version specifier: {spec!r} (expected X.Y.Z, patch, minor, or major)")

if new_ver == old_ver:
    sys.exit(f"Target version {new_ver} equals current version {old_ver}")

changelog = repo / "CHANGELOG.md"
new_changelog_text = None
moved_count = 0
if changelog.is_file():
    raw = changelog.read_text(encoding="utf-8")
    lines = raw.splitlines(keepends=True)
    unrel_start = None
    for idx, line in enumerate(lines):
        if re.fullmatch(r"##\s+Unreleased\s*", line.strip()):
            unrel_start = idx
            break
    if unrel_start is None:
        if not allow_empty:
            sys.exit("CHANGELOG.md has no '## Unreleased' section (add one or pass --allow-empty)")
    else:
        unrel_end = len(lines)
        for idx in range(unrel_start + 1, len(lines)):
            if re.match(r"^##\s+", lines[idx]):
                unrel_end = idx
                break
        body = lines[unrel_start + 1 : unrel_end]
        # Group lines into top-level bullet entries (and non-bullet preamble lines).
        known_skills = {p.name for p in (repo / "skills").iterdir() if p.is_dir()} if (repo / "skills").is_dir() else set()
        entries = []
        preamble = []
        current = None
        for line in body:
            if line.startswith("- "):
                if current is not None:
                    entries.append(current)
                current = [line]
            elif current is not None:
                if line.strip() == "":
                    # Keep blank separator only if followed by indented continuation; flush later
                    current.append(line)
                elif line.startswith((" ", "\t")):
                    current.append(line)
                else:
                    entries.append(current)
                    current = None
                    preamble.append(line)
            else:
                if line.strip():
                    preamble.append(line)
        if current is not None:
            entries.append(current)

        moved = []
        kept = []
        for ent in entries:
            # Strip trailing blank lines from an entry
            while ent and ent[-1].strip() == "":
                ent.pop()
            first = ent[0]
            sm = re.match(r"^-\s+`([^`]+)`", first)
            if sm and sm.group(1) in known_skills and sm.group(1) != skill:
                kept.append(ent)
            else:
                # Stamp version into leading `- `<skill>`：` if it has no version yet
                ent[0] = re.sub(
                    rf"^(-\s+`{re.escape(skill)}`)(\s*[：:])",
                    rf"\1 {new_ver}\2",
                    first,
                )
                moved.append(ent)
        moved_count = len(moved)
        if moved_count == 0 and not allow_empty:
            sys.exit(
                f"No unreleased entries for {skill} in CHANGELOG.md '## Unreleased' "
                f"(add bullets there or pass --allow-empty)"
            )
        if moved_count > 0:
            today = datetime.date.today().isoformat()
            rebuilt = []
            rebuilt.extend(lines[: unrel_start + 1])
            rebuilt.append("\n")
            if preamble:
                rebuilt.extend(preamble)
                rebuilt.append("\n")
            if kept:
                for ent in kept:
                    rebuilt.extend(ent)
                rebuilt.append("\n")
            rebuilt.append(f"## {skill} {new_ver} - {today}\n\n")
            for ent in moved:
                rebuilt.extend(ent)
            rebuilt.append("\n")
            rebuilt.extend(lines[unrel_end:])
            new_changelog_text = "".join(rebuilt)

# Validate all crate files before writing any file.
skill_md = repo / "skills" / skill / "SKILL.md"
skill_text = skill_md.read_text(encoding="utf-8")
fm_match = re.match(r"\A(---\s*\n)(.*?)(\n---\s*\n)", skill_text, re.S)
if not fm_match:
    sys.exit(f"Invalid frontmatter in {skill_md}")
fm_body = fm_match.group(2)
new_fm_body, n_sub = re.subn(
    r'^([ \t]*version:[ \t]*")' + re.escape(old_ver) + r'("[ \t]*)$',
    rf"\g<1>{new_ver}\g<2>",
    fm_body,
    count=1,
    flags=re.M,
)
if n_sub != 1:
    new_fm_body, n_sub = re.subn(
        r"^([ \t]*version:[ \t]*)" + re.escape(old_ver) + r"([ \t]*)$",
        rf"\g<1>{new_ver}\g<2>",
        fm_body,
        count=1,
        flags=re.M,
    )
if n_sub != 1:
    sys.exit(f"Could not update version {old_ver} in {skill_md}")
new_skill_text = fm_match.group(1) + new_fm_body + fm_match.group(3) + skill_text[fm_match.end():]

crate_updates = []
for b in bins:
    cargo_toml = repo / "crates" / b / "Cargo.toml"
    if not cargo_toml.is_file():
        sys.exit(f"Missing {cargo_toml}")
    ct_text = cargo_toml.read_text(encoding="utf-8")
    new_ct, n_ct = re.subn(
        r'(?m)^(\s*version\s*=\s*")' + re.escape(old_ver) + r'(")',
        rf"\g<1>{new_ver}\g<2>",
        ct_text,
        count=1,
    )
    if n_ct != 1:
        sys.exit(f"Could not update version {old_ver} in {cargo_toml}")
    crate_updates.append((cargo_toml, new_ct))
    cargo_lock = repo / "crates" / b / "Cargo.lock"
    if cargo_lock.is_file():
        cl_text = cargo_lock.read_text(encoding="utf-8")
        pattern = re.compile(
            r'(\[\[package\]\]\s*\nname\s*=\s*"' + re.escape(b) + r'"\s*\nversion\s*=\s*")'
            + re.escape(old_ver)
            + r'(")',
            re.M,
        )
        new_cl, n_cl = pattern.subn(rf"\g<1>{new_ver}\g<2>", cl_text, count=1)
        if n_cl == 1:
            crate_updates.append((cargo_lock, new_cl))

# Apply updates atomically in order.
skill_md.write_text(new_skill_text, encoding="utf-8")
for path, content in crate_updates:
    path.write_text(content, encoding="utf-8")
if new_changelog_text is not None:
    changelog.write_text(new_changelog_text, encoding="utf-8")

print(f"Bumped {skill}: {old_ver} -> {new_ver} (CHANGELOG entries stamped: {moved_count})")
if bins:
    print(f"Next: ./scripts/release-binary.sh build {skill}, commit, push, then: ./scripts/release-binary.sh publish {skill}")
else:
    print("Next: run ./scripts/verify.sh, commit and push")
PY
}

if [ "$ACTION" = "bump" ]; then
  shift 2
  bump "$@"
  exit 0
fi

[ -n "$NAME" ] || { echo "skills/$SKILL declares no binary" >&2; exit 2; }

build() {
  local bin crate target
  rm -rf "$DIST" && mkdir -p "$DIST"
  for bin in "${BINARIES[@]}"; do
    crate="$REPO_ROOT/crates/$bin"
    for target in "${TARGETS[@]}"; do
      rustup target add "$target" >/dev/null
      # Pure Rust with no C dependencies: rust-lld links a static musl binary for any target.
      (cd "$crate" && env "CARGO_TARGET_$(echo "$target" | tr 'a-z-' 'A-Z_')_LINKER=rust-lld" \
        cargo build --release --locked -q --target "$target")
      install -m 755 "$crate/target/$target/release/$bin" "$DIST/$bin-$target"
    done
  done
  (cd "$DIST" && LC_ALL=C sha256sum ./* | sed 's| \./| |') > "$DIR/bin.sha256"
  echo "Built $TAG into ${DIST#"$REPO_ROOT"/}:"
  cat "$DIR/bin.sha256"
  echo "Next: commit skills/$SKILL/bin.sha256 with the version bump, push, then: $0 publish $SKILL"
}

check_tag_points_to_head() {
  local head tag_commit
  head="$(git -C "$REPO_ROOT" rev-parse HEAD)" || return 1
  if git -C "$REPO_ROOT" show-ref --verify --quiet "refs/tags/$TAG"; then
    tag_commit="$(git -C "$REPO_ROOT" rev-parse "refs/tags/$TAG^{}")" || return 1
    if [ "$tag_commit" != "$head" ]; then
      echo "Tag $TAG does not point to HEAD (tag: $tag_commit, HEAD: $head)" >&2
      return 1
    fi
  fi
}

check_head_on_origin_main() {
  local head origin_main
  git -C "$REPO_ROOT" fetch -q origin || {
    echo "Failed to fetch origin; cannot verify whether HEAD is on origin/main" >&2
    return 1
  }
  head="$(git -C "$REPO_ROOT" rev-parse HEAD)" || return 1
  if ! origin_main="$(git -C "$REPO_ROOT" rev-parse -q --verify refs/remotes/origin/main 2>/dev/null)"; then
    echo "当前提交还没推到 origin/main，先推送再 publish（缺少 origin/main）" >&2
    return 1
  fi
  if ! git -C "$REPO_ROOT" merge-base --is-ancestor "$head" "$origin_main"; then
    echo "当前提交还没推到 origin/main，先推送再 publish (HEAD=${head:0:7}, origin/main=${origin_main:0:7})" >&2
    return 1
  fi
}

verify() {
  local bin crate cargo_version head
  local dirty_paths=("skills/$SKILL")
  local tracked_files=("skills/$SKILL/bin.sha256")
  for bin in "${BINARIES[@]}"; do
    crate="$REPO_ROOT/crates/$bin"
    [ -f "$crate/Cargo.toml" ] && [ -f "$crate/Cargo.lock" ] || {
      echo "Missing crates/$bin/Cargo.toml or Cargo.lock" >&2; return 1;
    }
    cargo_version="$(python3 - "$crate/Cargo.toml" <<'PY'
import sys
import tomllib
with open(sys.argv[1], 'rb') as source:
    print(tomllib.load(source)['package']['version'])
PY
)" || return 1
    [ "$VERSION" = "$cargo_version" ] || {
      echo "SKILL.md version $VERSION differs from crates/$bin/Cargo.toml $cargo_version" >&2; return 1;
    }
    dirty_paths+=("crates/$bin")
    tracked_files+=("crates/$bin/Cargo.lock")
  done
  [ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=all -- "${dirty_paths[@]}")" ] || {
    echo "Commit all changes in ${dirty_paths[*]} before publishing" >&2; return 1;
  }
  git -C "$REPO_ROOT" ls-files --error-unmatch -- "${tracked_files[@]}" >/dev/null || {
    echo "Commit bin.sha256 and Cargo.lock before publishing" >&2; return 1;
  }
  [ -d "$DIST" ] || { echo "Nothing built for $TAG: run $0 build $SKILL first" >&2; return 1; }
  (cd "$DIST" && sha256sum -c --quiet "$DIR/bin.sha256") || {
    echo "dist/ does not match committed bin.sha256" >&2; return 1;
  }
  head="$(git -C "$REPO_ROOT" rev-parse HEAD)" || return 1
  check_tag_points_to_head || return 1
  echo "Verified $TAG at $head"
}

publish() {
  local file notes
  verify || return $?
  check_head_on_origin_main || return $?
  # Re-check after fetch in case origin already carried a tag pointing elsewhere.
  check_tag_points_to_head || return $?
  if ! git -C "$REPO_ROOT" show-ref --verify --quiet "refs/tags/$TAG"; then
    git -C "$REPO_ROOT" tag -a "$TAG" -m "$NAME $VERSION" || return $?
  fi
  git -C "$REPO_ROOT" push -q origin "$TAG" || return $?
  # Forgejo's push mirror runs on branch pushes only; a tag pushed afterwards waits for the periodic
  # sync, so the GitHub wait below would time out. Ask Forgejo to mirror now.
  forgejo_token_over_ssh
  if [ -n "${FORGEJO_TOKEN:-}" ]; then
    curl -fsS -o /dev/null -X POST -H "Authorization: token $FORGEJO_TOKEN" "$FORGEJO_API/push_mirrors-sync" \
      || echo "  [Forgejo] push mirror sync request failed; waiting for the periodic sync" >&2
  fi
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

commit_release() {
  local bin
  local files=("skills/$SKILL/SKILL.md" "skills/$SKILL/bin.sha256")
  for bin in "${BINARIES[@]}"; do
    files+=("crates/$bin/Cargo.toml")
    if [ -f "$REPO_ROOT/crates/$bin/Cargo.lock" ]; then
      files+=("crates/$bin/Cargo.lock")
    fi
  done
  if [ -f "$REPO_ROOT/CHANGELOG.md" ] && git -C "$REPO_ROOT" ls-files --error-unmatch CHANGELOG.md >/dev/null 2>&1; then
    files+=("CHANGELOG.md")
  fi
  git -C "$REPO_ROOT" add -- "${files[@]}"
  git -C "$REPO_ROOT" commit -m "release($SKILL): $VERSION" -- "${files[@]}"
}

push_release() {
  git -C "$REPO_ROOT" push origin HEAD:main
}

install_release() {
  "$REPO_ROOT/scripts/install.sh"
}

sync_release() {
  "$REPO_ROOT/scripts/sync-hosts.sh"
}

RELEASE_STEPS=(bump build commit push publish install sync)

release_step_index() {
  local want="$1" i
  for i in "${!RELEASE_STEPS[@]}"; do
    if [ "${RELEASE_STEPS[$i]}" = "$want" ]; then
      echo "$i"
      return 0
    fi
  done
  return 1
}

release() {
  local target_spec="" allow_empty="0" from_step="bump"
  while (( $# > 0 )); do
    case "$1" in
      --allow-empty) allow_empty="1"; shift ;;
      --from)
        (( $# >= 2 )) || usage
        from_step="$2"
        shift 2
        ;;
      --from=*)
        from_step="${1#--from=}"
        shift
        ;;
      -*) usage ;;
      *)
        if [ -n "$target_spec" ]; then
          usage
        fi
        target_spec="$1"
        shift
        ;;
    esac
  done

  local start_idx
  if ! start_idx="$(release_step_index "$from_step")"; then
    echo "Invalid --from step: $from_step (valid: ${RELEASE_STEPS[*]})" >&2
    exit 2
  fi
  if (( start_idx == 0 )) && [ -z "$target_spec" ]; then
    usage
  fi

  local idx step rc resume_cmd
  for (( idx = start_idx; idx < ${#RELEASE_STEPS[@]}; idx++ )); do
    step="${RELEASE_STEPS[$idx]}"
    echo "==> [release:$SKILL] step: $step"
    rc=0
    set +e
    case "$step" in
      bump)
        if [ "$allow_empty" = "1" ]; then
          ( set -euo pipefail; bump "$target_spec" --allow-empty )
        else
          ( set -euo pipefail; bump "$target_spec" )
        fi
        rc=$?
        ;;
      build)   ( set -euo pipefail; build ); rc=$? ;;
      commit)  ( set -euo pipefail; commit_release ); rc=$? ;;
      push)    ( set -euo pipefail; push_release ); rc=$? ;;
      publish) ( set -euo pipefail; publish ); rc=$? ;;
      install) ( set -euo pipefail; install_release ); rc=$? ;;
      sync)    ( set -euo pipefail; sync_release ); rc=$? ;;
    esac
    set -e
    if (( rc != 0 )); then
      if [ "$step" = "bump" ]; then
        resume_cmd="$0 release $SKILL $target_spec"
        [ "$allow_empty" = "1" ] && resume_cmd+=" --allow-empty"
      else
        resume_cmd="$0 release $SKILL --from $step"
      fi
      echo "[release] failed at step '$step' (exit $rc)." >&2
      echo "[release] resume with: $resume_cmd" >&2
      return "$rc"
    fi
    if [ "$step" = "bump" ]; then
      refresh_meta
    fi
  done
  echo "==> [release:$SKILL] completed $TAG"
}

case "$ACTION" in
  build)   (( $# == 2 )) || usage; build ;;
  verify)  (( $# == 2 )) || usage; verify ;;
  publish) (( $# == 2 )) || usage; publish ;;
  release) shift 2; release "$@" ;;
  *) usage ;;
esac
