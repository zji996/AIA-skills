#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
echo "=== Checking AIA-skills repository ==="

if ! python3 - "$REPO_ROOT" <<'PY'
import re
import sys
from pathlib import Path
from urllib.parse import unquote

try:
    import yaml
except ImportError:
    sys.exit("[FAIL] PyYAML is required (python3 -m pip install PyYAML)")

root = Path(sys.argv[1])
skills = root / "skills"
readme = root / "README.md"
errors = []
skill_names = set()
TRIGGER = re.compile(r"时使用|\buse (when|to|for)\b", re.I)
KNOWN_AGENTS = {"pi", "codex", "cursor", "claude", "kilo"}
MAX_SKILL_LINES = 80

for directory in sorted(skills.iterdir()):
    if not directory.is_dir():
        continue
    name = directory.name
    skill_names.add(name)
    skill_file = directory / "SKILL.md"
    if not skill_file.is_file():
        errors.append(f"{name}: missing SKILL.md")
        continue
    content = skill_file.read_text(encoding="utf-8")
    match = re.match(r"\A---\s*\n(.*?)\n---\s*\n", content, re.S)
    if not match:
        errors.append(f"{name}: invalid or missing YAML frontmatter")
        continue
    try:
        metadata = yaml.safe_load(match.group(1))
    except yaml.YAMLError as exc:
        errors.append(f"{name}: invalid YAML: {exc}")
        continue
    if not isinstance(metadata, dict) or metadata.get("name") != name:
        errors.append(f"{name}: frontmatter name must exactly match directory")
    description = metadata.get("description") if isinstance(metadata, dict) else None
    if not isinstance(description, str) or not description.strip():
        errors.append(f"{name}: description must be nonempty text")
    else:
        if len(description) > 1024:
            errors.append(f"{name}: description exceeds 1024 characters")
        if not TRIGGER.search(description):
            errors.append(f"{name}: description must say when to use the skill (e.g. '…时使用' or 'Use when/to/for')")
    extra = metadata.get("metadata") if isinstance(metadata, dict) else None
    version = str((extra or {}).get("version", "")) if isinstance(extra, dict) else ""
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        errors.append(f"{name}: metadata.version must be semantic (e.g. \"1.0.0\")")
    excluded = str((extra or {}).get("exclude-agents", "")).replace(",", " ").split() if isinstance(extra, dict) else []
    for agent in excluded:
        if agent not in KNOWN_AGENTS:
            errors.append(f"{name}: unknown agent in exclude-agents: {agent}")
    if "./skills/" in content:
        errors.append(f"{name}: SKILL.md uses a repo-relative './skills/' path; use <本技能目录>/... instead")
    if not (root / "evals" / f"{name}.md").is_file():
        errors.append(f"{name}: missing evals/{name}.md trigger examples")
    lines = content.count("\n") + 1
    if lines > MAX_SKILL_LINES:
        print(f"  [WARN] {name}: SKILL.md has {lines} lines (> {MAX_SKILL_LINES}); consider moving details to references/")
    if not any(error.startswith(f"{name}:") for error in errors):
        print(f"  [PASS] Skill metadata: {name}")

readme_text = readme.read_text(encoding="utf-8")
index = re.findall(r"^\|\s*\*\*`([^`]+)`\*\*\s*\|\s*`skills/([^/]+)/`\s*\|", readme_text, re.M)
indexed_names = set()
for label, directory in index:
    if label != directory or label in indexed_names:
        errors.append(f"README.md: mismatched or duplicate index entry: {label} -> {directory}")
    indexed_names.add(directory)
for name in sorted(skill_names - indexed_names):
    errors.append(f"README.md: missing skill index entry: {name}")
for name in sorted(indexed_names - skill_names):
    errors.append(f"README.md: index points to missing skill: {name}")

for doc in [readme, *sorted(skills.rglob("*.md"))]:
    content = doc.read_text(encoding="utf-8")
    for target in re.findall(r"!?\[[^\]]+\]\(([^\s)]+)(?:\s+[^)]*)?\)", content):
        if target.startswith(("https://", "http://", "mailto:", "#")):
            continue
        path = unquote(target.split("#", 1)[0])
        if path and not (doc.parent / path).exists():
            errors.append(f"{doc.relative_to(root)}: broken link: {target}")

for error in errors:
    print(f"  [FAIL] {error}", file=sys.stderr)
sys.exit(bool(errors))
PY
then
  exit 1
fi

errors=0
for script in "$REPO_ROOT/scripts"/*.sh "$REPO_ROOT/skills"/*/scripts/*.sh; do
  [ -f "$script" ] || continue
  if [ ! -x "$script" ]; then
    echo "  [FAIL] Script not executable: $script" >&2
    errors=$((errors + 1))
  elif ! bash -n "$script"; then
    echo "  [FAIL] Script syntax: $script" >&2
    errors=$((errors + 1))
  else
    echo "  [PASS] Script syntax & executable: $(basename "$script")"
  fi
done

for script in "$REPO_ROOT/skills"/*/scripts/*.py; do
  [ -f "$script" ] || continue
  if [ ! -x "$script" ]; then
    echo "  [FAIL] Script not executable: $script" >&2
    errors=$((errors + 1))
  elif ! python3 -c 'import ast, sys; ast.parse(open(sys.argv[1], encoding="utf-8").read(), sys.argv[1])' "$script"; then
    echo "  [FAIL] Script syntax: $script" >&2
    errors=$((errors + 1))
  else
    echo "  [PASS] Script syntax & executable: $(basename "$script")"
  fi
done

if [ "$errors" -gt 0 ]; then
  echo "Validation FAILED with $errors script errors." >&2
  exit 1
fi
echo "All checks passed successfully."
