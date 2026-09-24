#!/usr/bin/env bash
# One-line installer: clone (or update) AIA-skills, then run scripts/install.sh.
#   curl -fsSL https://git.aiatechco.com:31443/zji996/AIA-skills/raw/branch/main/scripts/bootstrap.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/zji996/AIA-skills/main/scripts/bootstrap.sh | bash
set -euo pipefail

PRIMARY_URL="${AIA_SKILLS_PRIMARY:-https://git.aiatechco.com:31443/zji996/AIA-skills.git}"
MIRROR_URL="${AIA_SKILLS_MIRROR:-https://github.com/zji996/AIA-skills.git}"

usage() {
  cat >&2 <<'EOF'
usage: bootstrap.sh [--dir <path>] [--ref <branch|tag>] [--github | --source <git-url>]
                    [--with-pi | --with-pi-sync] [install options...]

  --dir           checkout location (default: $AIA_SKILLS_DIR or ~/.local/share/aia-skills)
  --ref           branch or tag to install (default: main)
  --github        clone from GitHub instead of the primary repository
  --source        clone from a specific git URL
  --with-pi       also install or update Pi via the bundled pi-kit, keeping existing
                  Pi settings and packages (pi-kit --additive); needed by pi-delegation
  --with-pi-sync  same, but apply pi-kit's full declarative setup (pi-kit --sync)
Remaining arguments go to scripts/install.sh, e.g. --copy, --force or skill names.
Set PI_KIT_MIRROR=cn to force npm mirrors in Mainland China.
Re-running the same command updates an existing checkout.
EOF
  exit 2
}

log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'bootstrap: %s\n' "$*" >&2; exit 1; }

# pi-kit is a submodule with a URL relative to the checkout's origin, so it
# comes from the same host the skills were cloned from.
install_pi() {
  local dir=$1 mode=$2 tool missing=()
  log "Installing Pi via pi-kit ($mode)"
  git -C "$dir" submodule sync --quiet -- third_party/pi-kit
  GIT_TERMINAL_PROMPT=0 git -C "$dir" submodule update --init --quiet -- third_party/pi-kit ||
    die "could not fetch third_party/pi-kit; skills are installed, re-run to retry Pi"
  sh "$dir/third_party/pi-kit/install.sh" "--$mode" </dev/null || die "pi-kit failed; skills are installed, re-run to retry Pi"
  for tool in jq setsid timeout; do
    command -v "$tool" >/dev/null || missing+=("$tool")
  done
  if (( ${#missing[@]} )); then
    log "pi-delegation also needs: ${missing[*]} (e.g. sudo apt install jq util-linux coreutils)"
  fi
}

main() {
  local dir="${AIA_SKILLS_DIR:-$HOME/.local/share/aia-skills}" ref=main source="" pi_mode=""
  local install_args=()
  while (( $# )); do
    case "$1" in
      --dir) [[ $# -ge 2 ]] || usage; dir=$2; shift 2 ;;
      --ref) [[ $# -ge 2 ]] || usage; ref=$2; shift 2 ;;
      --github) source=$MIRROR_URL; shift ;;
      --source) [[ $# -ge 2 ]] || usage; source=$2; shift 2 ;;
      --with-pi) pi_mode=additive; shift ;;
      --with-pi-sync) pi_mode=sync; shift ;;
      -h|--help) usage ;;
      *) install_args+=("$1"); shift ;;
    esac
  done

  command -v git >/dev/null || die "git is required"
  (( BASH_VERSINFO[0] >= 4 )) || die "bash 4+ is required (on macOS: brew install bash)"

  if [[ -d "$dir/.git" ]]; then
    log "Updating $dir to $ref"
    git -C "$dir" fetch --quiet --tags origin
    git -C "$dir" checkout --quiet "$ref"
    if git -C "$dir" symbolic-ref -q HEAD >/dev/null; then
      git -C "$dir" merge --quiet --ff-only "origin/$ref" ||
        die "local changes in $dir prevent a fast-forward update; resolve them and re-run"
    fi
  elif [[ -e "$dir" ]]; then
    die "$dir exists and is not a git checkout; pass --dir or remove it"
  else
    mkdir -p "$(dirname "$dir")"
    local urls=("$PRIMARY_URL" "$MIRROR_URL") url cloned=0
    [[ -n "$source" ]] && urls=("$source")
    for url in "${urls[@]}"; do
      log "Cloning $url ($ref)"
      if GIT_TERMINAL_PROMPT=0 git clone --quiet --branch "$ref" "$url" "$dir"; then
        cloned=1
        break
      fi
      rm -rf -- "$dir"
      log "Clone failed: $url"
    done
    (( cloned )) || die "could not clone AIA-skills"
  fi

  "$dir/scripts/install.sh" ${install_args[@]+"${install_args[@]}"}
  [[ -z "$pi_mode" ]] || install_pi "$dir" "$pi_mode"
  log "Installed from $dir ($(git -C "$dir" describe --tags --always 2>/dev/null)). Re-run this command to update."
}

main "$@"
