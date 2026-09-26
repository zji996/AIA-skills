#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SKILLS_SRC="$REPO_ROOT/skills"
MARKER=".aia-skills-install"

# Shared directories and the agents that scan each of them.
PRIMARY_DIRS=(
  "$HOME/.agents/skills"
  "$HOME/.claude/skills"
)
declare -A READERS=(
  ["$HOME/.agents/skills"]="pi codex cursor kilo"
  ["$HOME/.claude/skills"]="claude"
)
# Agent-specific directories, used only when a skill must skip a shared directory.
declare -A OWN_DIR=(
  [codex]="$HOME/.codex/skills"
  [cursor]="$HOME/.cursor/skills"
  [kilo]="$HOME/.kilo/skills"
)
MANAGED_DIRS=("${PRIMARY_DIRS[@]}" "$HOME/.codex/skills" "$HOME/.cursor/skills" "$HOME/.kilo/skills")

usage() {
  cat >&2 <<'EOF'
usage: install.sh [--copy] [--force] [<skill>...]   install (default: all skills, as symlinks)
       install.sh --uninstall [<skill>...]         remove entries installed from this repository
       install.sh --status                         list installed entries and outdated copies

  --copy   copy skill directories instead of linking them (for machines without this checkout)
  --force  replace same-named symlinks that point to another source
EOF
  exit 2
}

ACTION=install
MODE=link
FORCE=0
SELECTED_SKILLS=()
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    --copy) MODE=copy ;;
    --uninstall) ACTION=uninstall ;;
    --status) ACTION=status ;;
    -h|--help) usage ;;
    -*) echo "Unknown option: $arg" >&2; usage ;;
    *) SELECTED_SKILLS+=("$arg") ;;
  esac
done

frontmatter_value() {
  awk -v key="$2" '/^---[[:space:]]*$/ { if (++fence == 2) exit; next }
       fence == 1 && $0 ~ "^[[:space:]]*" key ":" {
         sub(/^[^:]*:[[:space:]]*/, ""); gsub(/[][,"]/, " "); gsub(/\047/, " "); print; exit }' "$1"
}

source_commit() {
  local commit
  commit="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null)" || { echo unknown; return; }
  if [ -n "$(git -C "$REPO_ROOT" status --porcelain -- "skills/$1" 2>/dev/null)" ]; then
    commit="$commit-dirty"
  fi
  echo "$commit"
}

# An entry is ours if it links into this repository or is a copy carrying our marker.
owned() {
  local entry=$1
  if [ -L "$entry" ]; then
    [[ "$(readlink "$entry")" == "$SKILLS_SRC/"* ]]
  else
    [ -f "$entry/$MARKER" ] && grep -qxF "source=$SKILLS_SRC" "$entry/$MARKER"
  fi
}

remove_entry() {
  if [ -L "$1" ]; then rm -f -- "$1"; else rm -rf -- "$1"; fi
}

marker_field() {
  sed -n "s/^$2=//p" "$1/$MARKER" | head -n 1
}

owned_entries() {
  local dir entry
  for dir in "${MANAGED_DIRS[@]}"; do
    [ -d "$dir" ] || continue
    for entry in "$dir"/* "$dir"/.[!.]*; do
      { [ -L "$entry" ] || [ -d "$entry" ]; } && owned "$entry" && printf '%s\n' "$entry"
    done
  done
  return 0
}

if [ "$ACTION" = status ]; then
  found=0
  while IFS= read -r entry; do
    found=1
    name="$(basename "$entry")"
    if [ -L "$entry" ]; then
      state="link"
      [ -e "$entry" ] || state="link (broken)"
    elif [ ! -f "$SKILLS_SRC/$name/SKILL.md" ]; then
      state="copy (source removed)"
    else
      installed="$(marker_field "$entry" version) @ $(marker_field "$entry" commit)"
      current="$(frontmatter_value "$SKILLS_SRC/$name/SKILL.md" version | xargs) @ $(source_commit "$name")"
      state="copy $installed"
      [ "$installed" = "$current" ] || state="$state (outdated; source is $current)"
    fi
    echo "$entry: $state"
  done < <(owned_entries)
  [ "$found" -eq 1 ] || echo "No entries installed from $SKILLS_SRC"
  exit 0
fi

if [ "$ACTION" = uninstall ]; then
  removed=0
  while IFS= read -r entry; do
    name="$(basename "$entry")"
    if [ ${#SELECTED_SKILLS[@]} -gt 0 ] && [[ " ${SELECTED_SKILLS[*]} " != *" $name "* ]]; then
      continue
    fi
    remove_entry "$entry"
    removed=$((removed + 1))
    echo "  [Removed] $entry"
  done < <(owned_entries)
  echo "Done! Removed $removed entries."
  exit 0
fi

if [ ${#SELECTED_SKILLS[@]} -eq 0 ]; then
  for skill_path in "$SKILLS_SRC"/*; do
    [ -f "$skill_path/SKILL.md" ] && SELECTED_SKILLS+=("$(basename "$skill_path")")
  done
fi
if [ ${#SELECTED_SKILLS[@]} -eq 0 ]; then
  echo "No skills found in $SKILLS_SRC" >&2
  exit 1
fi
for skill in "${SELECTED_SKILLS[@]}"; do
  if [[ ! "$skill" =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]] || [ ! -f "$SKILLS_SRC/$skill/SKILL.md" ]; then
    echo "Invalid or missing skill: $skill" >&2
    exit 2
  fi
done

# Fills PLAN_INSTALL with the directories that should contain the skill.
plan_skill() {
  local skill=$1 dir agent excluded blocked
  PLAN_INSTALL=()
  excluded=" $(frontmatter_value "$SKILLS_SRC/$skill/SKILL.md" exclude-agents) "
  for dir in "${PRIMARY_DIRS[@]}"; do
    blocked=0
    for agent in ${READERS[$dir]}; do
      [[ "$excluded" == *" $agent "* ]] && blocked=1
    done
    if (( ! blocked )); then
      PLAN_INSTALL+=("$dir")
      continue
    fi
    for agent in ${READERS[$dir]}; do
      if [[ "$excluded" != *" $agent "* && -n "${OWN_DIR[$agent]:-}" ]]; then
        PLAN_INSTALL+=("${OWN_DIR[$agent]}")
      fi
    done
  done
}

# Validate every destination before touching anything.
for skill in "${SELECTED_SKILLS[@]}"; do
  plan_skill "$skill"
  for dir in "${PLAN_INSTALL[@]}"; do
    dest="$dir/$skill"
    if owned "$dest"; then
      continue
    elif [ -L "$dest" ]; then
      if [ "$FORCE" -ne 1 ]; then
        echo "Existing link belongs to another source: $dest (use --force to replace it)" >&2
        exit 1
      fi
    elif [ -e "$dest" ]; then
      echo "Existing entry was not installed from this repository: $dest" >&2
      exit 1
    fi
  done
done

install_copy() {
  local skill=$1 src=$2 dest=$3 tmp
  tmp="$(dirname "$dest")/.$skill.tmp.$$"
  rm -rf -- "$tmp"
  cp -R -- "$src" "$tmp"
  find "$tmp" -name __pycache__ -type d -prune -exec rm -rf {} +
  printf 'source=%s\nversion=%s\ncommit=%s\n' "$SKILLS_SRC" \
    "$(frontmatter_value "$src/SKILL.md" version | xargs)" "$(source_commit "$skill")" > "$tmp/$MARKER"
  if [ -L "$dest" ] || [ -e "$dest" ]; then remove_entry "$dest"; fi
  mv -- "$tmp" "$dest"
}

echo "=== Installing AIA-skills ($MODE) ==="
# A skill that declares `binary:` gets its prebuilt binary first, so a copy carries it too.
for skill in "${SELECTED_SKILLS[@]}"; do
  if [ -n "$(frontmatter_value "$SKILLS_SRC/$skill/SKILL.md" binary)" ]; then
    "$REPO_ROOT/scripts/fetch-binary.sh" "$skill" || { echo "Skipping $skill: its binary is missing" >&2; exit 1; }
  fi
done
for skill in "${SELECTED_SKILLS[@]}"; do
  plan_skill "$skill"
  src="$SKILLS_SRC/$skill"
  for dir in "${MANAGED_DIRS[@]}"; do
    dest="$dir/$skill"
    if [[ " ${PLAN_INSTALL[*]} " != *" $dir "* ]]; then
      if { [ -L "$dest" ] || [ -d "$dest" ]; } && owned "$dest"; then
        remove_entry "$dest"
        echo "  [Removed] $dest (not needed for this skill)"
      fi
      continue
    fi
    mkdir -p "$dir"
    if [ "$MODE" = link ]; then
      if [ -L "$dest" ] && [ "$(readlink "$dest")" = "$src" ]; then
        echo "  [Already linked] $dest"
        continue
      fi
      if [ -e "$dest" ] && [ ! -L "$dest" ]; then remove_entry "$dest"; fi
      ln -sfnT "$src" "$dest"
      echo "  [Linked] $dest"
    else
      install_copy "$skill" "$src" "$dest"
      echo "  [Copied] $dest"
    fi
  done
done

while IFS= read -r entry; do
  if [ -L "$entry" ]; then
    [ -f "$entry/SKILL.md" ] && continue  # a renamed skill may leave a directory of caches behind
  elif [ -f "$SKILLS_SRC/$(basename "$entry")/SKILL.md" ]; then
    continue
  fi
  remove_entry "$entry"
  echo "  [Pruned] $entry (source skill no longer exists)"
done < <(owned_entries)

echo "Done! Installed ${#SELECTED_SKILLS[@]} skills."
