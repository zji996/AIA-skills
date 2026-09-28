#!/usr/bin/env bash
# Put a skill's prebuilt binary at skills/<skill>/bin/<name>.
#
# A skill that needs one declares `binary: <name>` in its SKILL.md metadata. The release for its version
# (tag <name>-v<version>) carries one static musl build per architecture; the checksums committed in
# skills/<skill>/bin.sha256 decide what is accepted, so trust stays rooted in this repository.
# Without a download, a checkout that has cargo and crates/<name> builds it instead (--build forces that).
set -euo pipefail

# Test fixtures may override the checkout; normal installs use this script's repository.
REPO_ROOT="${AIA_SKILLS_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
# GitHub releases are the primary source; the Forgejo instance is tried second when it has them too.
PRIMARY_RELEASES="${AIA_SKILLS_RELEASES:-https://github.com/zji996/AIA-skills/releases/download}"
MIRROR_RELEASES="${AIA_SKILLS_MIRROR_RELEASES:-https://git.aiatechco.com:31443/zji996/AIA-skills/releases/download}"

usage() {
  echo "usage: fetch-binary.sh [--build] <skill>...   download (or build) the binaries these skills declare" >&2
  exit 2
}

BUILD=0
SKILLS=()
for arg in "$@"; do
  case "$arg" in
    --build) BUILD=1 ;;
    -h|--help) usage ;;
    -*) usage ;;
    *) SKILLS+=("$arg") ;;
  esac
done
(( ${#SKILLS[@]} )) || usage

frontmatter_value() {
  sed -n '/^---$/,/^---$/p' "$1" | sed -n "s/^[[:space:]]*$2:[[:space:]]*//p" | head -1 | tr -d '"'
}

target_triple() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) echo x86_64-unknown-linux-musl ;;
    Linux-aarch64|Linux-arm64) echo aarch64-unknown-linux-musl ;;
    *) echo "unsupported platform: $(uname -s) $(uname -m) (Linux x86_64 or aarch64 only)" >&2; return 1 ;;
  esac
}

sha256_of() { sha256sum "$1" | cut -d' ' -f1; }

download() {
  local url=$1 out=$2
  if command -v curl >/dev/null; then
    curl -fsSL --retry 2 --connect-timeout 10 -o "$out" "$url"
  else
    wget -q -O "$out" "$url"
  fi
}

build_from_source() {
  local name=$1 dest=$2 crate="$REPO_ROOT/crates/$1"
  [ -f "$crate/Cargo.toml" ] && command -v cargo >/dev/null || return 1
  echo "  [Build] $name from $crate" >&2
  (cd "$crate" && cargo build --release --locked -q) || return 1
  install -m 755 "$crate/target/release/$name" "$dest"
}

fetch_skill() {
  local skill=$1 dir="$REPO_ROOT/skills/$1" name version target asset expected tmp bin marker url actual
  name="$(frontmatter_value "$dir/SKILL.md" binary)"
  [ -n "$name" ] || return 0
  version="$(frontmatter_value "$dir/SKILL.md" version)"
  bin="$dir/bin/$name"
  marker="$dir/bin/.$name.installed"
  mkdir -p "$dir/bin"
  target="$(target_triple)"
  asset="$name-$target"
  expected="$(awk -v a="$asset" '$2 == a { print $1 }' "$dir/bin.sha256" 2>/dev/null)"
  if (( BUILD )); then
    install_source "$name" "$bin" "$marker" "$expected" "$version" || {
      echo "Cannot build or verify $name from $REPO_ROOT/crates/$name" >&2
      return 1
    }
    return 0
  fi
  if [ -x "$bin" ] && [ -f "$marker" ] &&
      { [ "$(cat "$marker")" = "$version $expected" ] ||
        [ "$(cat "$marker")" = "source-verified $version $expected" ]; } \
      && [ "$(sha256_of "$bin")" = "$expected" ]; then
    echo "  [Up to date] $bin ($name $version, $target)"
    return 0
  fi
  if [ -n "$expected" ]; then
    tmp="$(mktemp "$dir/bin/.$name.XXXXXX")"
    for url in "$PRIMARY_RELEASES/$name-v$version/$asset" "$MIRROR_RELEASES/$name-v$version/$asset"; do
      if download "$url" "$tmp" 2>/dev/null; then
        if [ "$(sha256_of "$tmp")" = "$expected" ]; then
          chmod 755 "$tmp"
          mv -f "$tmp" "$bin"
          echo "$version $expected" > "$marker"
          echo "  [Downloaded] $bin ($name $version, $target)"
          return 0
        fi
        echo "  [Rejected] $url: checksum does not match skills/$skill/bin.sha256" >&2
      fi
    done
    rm -f "$tmp"
  else
    echo "  [No release] skills/$skill/bin.sha256 lists no $asset" >&2
  fi
  if install_source "$name" "$bin" "$marker" "$expected" "$version"; then
    return 0
  fi
  echo "Cannot install $name $version for $target: download failed and local build is unavailable or unverified." >&2
  echo "  Retry later, or install Rust (https://rustup.rs) and run: $0 --build $skill" >&2
  return 1
}

install_source() {
  local name=$1 bin=$2 marker=$3 expected=$4 version=$5 tmp actual
  tmp="$(mktemp "$(dirname "$bin")/.$name.build.XXXXXX")" || return 1
  if ! build_from_source "$name" "$tmp"; then
    rm -f -- "$tmp"
    return 1
  fi
  actual="$(sha256_of "$tmp")"
  if [ -z "$expected" ] || [ "$actual" != "$expected" ]; then
    echo "  [Warning] 本机构建，未经发布校验: $name ($actual)" >&2
    if [ "${AIA_SKILLS_REQUIRE_VERIFIED:-}" = 1 ]; then
      rm -f -- "$tmp"
      return 1
    fi
    mv -f -- "$tmp" "$bin"
    printf 'source-unverified %s\n' "$actual" > "$marker"
  else
    mv -f -- "$tmp" "$bin"
    printf 'source-verified %s %s\n' "$version" "$expected" > "$marker"
  fi
  echo "  [Built] $bin (local source, checksum ${actual})"
}

status=0
for skill in "${SKILLS[@]}"; do
  [ -f "$REPO_ROOT/skills/$skill/SKILL.md" ] || { echo "Invalid or missing skill: $skill" >&2; exit 2; }
  fetch_skill "$skill" || status=1
done
exit $status
