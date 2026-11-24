#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
strict_budget=0
case "${AIA_SKILLS_STRICT_BUDGET:-0}" in
  1|true|TRUE|yes|YES) strict_budget=1 ;;
esac
for arg in "$@"; do
  case "$arg" in
    --strict|--strict-budget) strict_budget=1 ;;
    *)
      echo "Unknown option: $arg (supported: --strict)" >&2
      exit 2
      ;;
  esac
done
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
CHINESE_TRIGGER = re.compile(r"时(?:主动)?使用")
ENGLISH_TRIGGER = re.compile(r"\buse (when|to|for)\b", re.I)
KNOWN_AGENTS = {"pi", "codex", "cursor", "claude", "kilo"}

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
        if not CHINESE_TRIGGER.search(description) or not ENGLISH_TRIGGER.search(description):
            errors.append(f"{name}: description must say when to use the skill in Chinese and English (e.g. '…时使用' and 'Use when/to/for')")
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

audit="$REPO_ROOT/skills/repo-governance/scripts/audit-context.py"
if [ -f "$audit" ]; then
  if [ "$strict_budget" -eq 1 ]; then
    python3 "$audit" --repo "$REPO_ROOT" --only budget --fail-on budget
  else
    python3 "$audit" --repo "$REPO_ROOT" --only budget --fail-on none
  fi
fi

# Unresolved merge conflict markers (apply --merge leaves them in any text file).
if git -C "$REPO_ROOT" grep -n -I -E '^(<<<<<<< |>>>>>>> |\|\|\|\|\|\|\| )' -- . >/dev/null 2>&1; then
  git -C "$REPO_ROOT" grep -n -I -E '^(<<<<<<< |>>>>>>> |\|\|\|\|\|\|\| )' -- . | sed 's/^/  [FAIL] conflict marker: /' >&2
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

# A skill with a prebuilt binary must list a checksum for every released target, and have its source.
for skill_md in "$REPO_ROOT/skills"/*/SKILL.md; do
  dir="$(dirname "$skill_md")"
  primary="$(sed -n '/^---$/,/^---$/p' "$skill_md" | sed -n 's/^[[:space:]]*binary:[[:space:]]*//p' | head -1 | tr -d '"')"
  [ -n "$primary" ] || continue
  extras="$(sed -n '/^---$/,/^---$/p' "$skill_md" | sed -n 's/^[[:space:]]*extra-binaries:[[:space:]]*//p' | head -1 | tr -d '"' | tr ',[]' '   ')"
  version="$(sed -n '/^---$/,/^---$/p' "$skill_md" | sed -n 's/^[[:space:]]*version:[[:space:]]*//p' | head -1 | tr -d '"')"
  for binary in "$primary" $extras; do
    missing=""
    for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
      grep -qE "^[0-9a-f]{64}  $binary-$target\$" "$dir/bin.sha256" 2>/dev/null || missing+=" $target"
    done
    crate_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/crates/$binary/Cargo.toml" 2>/dev/null | head -1)"
    if [ -n "$missing" ] || [ ! -f "$REPO_ROOT/crates/$binary/Cargo.toml" ]; then
      echo "  [FAIL] Binary $binary: bin.sha256 lacks${missing:- nothing} or crates/$binary is missing" >&2
      errors=$((errors + 1))
    elif [ "$crate_version" != "$version" ]; then
      echo "  [FAIL] Binary $binary: crates/$binary is version $crate_version, the skill is $version" >&2
      errors=$((errors + 1))
    else
      echo "  [PASS] Binary checksums & source: $binary"
    fi
  done
done

if [ "$errors" -gt 0 ]; then
  echo "Validation FAILED with $errors script errors." >&2
  exit 1
fi

# Static AST gate for timing-flaky test antipatterns (see tests/README.md).
if ! python3 - "$REPO_ROOT" <<'PY'
import ast, re, sys
from pathlib import Path

root = Path(sys.argv[1])
test_files = sorted(list((root / "tests").glob("test_*.py")) + list(root.glob("crates/*/tests/test_*.py")))
TIMING_RE = re.compile(r"(?:perf_counter|monotonic|elapsed|wall|p99|latency|_ms\b|\bms\b|took|duration)", re.I)
MS_RE = re.compile(r"(?:_ms\b|\bms\b|p99)", re.I)
errors = []

def has_ok(lines, node, tag):
    pat = re.compile(rf"#\s*{tag}-ok:\s*\S")
    lo, hi = max(1, node.lineno), min(len(lines), getattr(node, "end_lineno", node.lineno))
    return any(pat.search(lines[i - 1]) for i in range(lo, hi + 1))

for path in test_files:
    rel = path.relative_to(root)
    src = path.read_text(encoding="utf-8")
    lines = src.splitlines()
    tree = ast.parse(src, str(rel))
    time_mods = {a.asname or a.name for n in ast.walk(tree) if isinstance(n, ast.Import) for a in n.names if a.name == "time"}
    imp_sleep = {a.asname or a.name for n in ast.walk(tree) if isinstance(n, ast.ImportFrom) and n.module == "time" for a in n.names if a.name == "sleep"}
    imp_time = {a.asname or a.name for n in ast.walk(tree) if isinstance(n, ast.ImportFrom) and n.module == "time" for a in n.names if a.name == "time"}

    def is_time_call(call, attr, direct):
        f = call.func
        return (isinstance(f, ast.Attribute) and f.attr == attr and isinstance(f.value, ast.Name) and f.value.id in time_mods) or (isinstance(f, ast.Name) and f.id in direct)

    class Checker(ast.NodeVisitor):
        def __init__(self):
            self.in_test = 0
        def visit_FunctionDef(self, node):
            hit = node.name.startswith("test_")
            self.in_test += hit
            self.generic_visit(node)
            self.in_test -= hit
        def visit_While(self, node):
            if any(is_time_call(c, "time", imp_time) for c in ast.walk(node) if isinstance(c, ast.Call)):
                if not has_ok(lines, node, "poll"):
                    errors.append(f"{rel}:{node.lineno}: hand-written time.time() polling loop; use wait_until() (or # poll-ok: <reason>)")
            self.generic_visit(node)
        def _wall(self, node, lhs, rhs):
            if not (isinstance(rhs, ast.Constant) and isinstance(rhs.value, (int, float)) and not isinstance(rhs.value, bool)):
                return
            lhs_src = ast.unparse(lhs)
            if TIMING_RE.search(lhs_src) and float(rhs.value) < (1000.0 if MS_RE.search(lhs_src) else 1.0) and not has_ok(lines, node, "wall-clock"):
                errors.append(f"{rel}:{node.lineno}: tight wall-clock assertion (< {rhs.value}); use gates/invariants/probes (or # wall-clock-ok: <reason>)")
        def visit_Assert(self, node):
            if isinstance(node.test, ast.Compare) and len(node.test.ops) == 1 and isinstance(node.test.ops[0], (ast.Lt, ast.LtE)):
                self._wall(node, node.test.left, node.test.comparators[0])
            self.generic_visit(node)
        def visit_Call(self, node):
            if self.in_test:
                if is_time_call(node, "sleep", imp_sleep) and not has_ok(lines, node, "sleep"):
                    errors.append(f"{rel}:{node.lineno}: bare time.sleep() inside test; use gates or wait_until() (or # sleep-ok: <reason>)")
                if isinstance(node.func, ast.Attribute) and node.func.attr == "kill" and isinstance(node.func.value, ast.Name) and node.func.value.id == "os":
                    if len(node.args) >= 2 and isinstance(node.args[1], ast.Constant) and node.args[1].value == 0 and not has_ok(lines, node, "kill"):
                        errors.append(f"{rel}:{node.lineno}: bare os.kill(pid, 0); use wait_gone() (or # kill-ok: <reason>)")
            fn = node.func.attr if isinstance(node.func, ast.Attribute) else (node.func.id if isinstance(node.func, ast.Name) else "")
            if fn in ("assertLess", "assertLessEqual") and len(node.args) >= 2:
                self._wall(node, node.args[0], node.args[1])
            elif fn == "assertTrue" and node.args and isinstance(node.args[0], ast.Compare) and len(node.args[0].ops) == 1 and isinstance(node.args[0].ops[0], (ast.Lt, ast.LtE)):
                self._wall(node, node.args[0].left, node.args[0].comparators[0])
            elif fn in ("until", "wait_until") and node.args:
                if any(isinstance(c, ast.Call) and isinstance(c.func, ast.Attribute) and c.func.attr == "read_text" for c in ast.walk(node.args[0])):
                    if not has_ok(lines, node, "until"):
                        errors.append(f"{rel}:{node.lineno}: until(...read_text()); use wait_text()/wait_json()/wait_gone() (or # until-ok: <reason>)")
            self.generic_visit(node)

    Checker().visit(tree)

for err in errors:
    print(f"  [FAIL] {err}", file=sys.stderr)
if not errors:
    print(f"  [PASS] Test timing AST gate ({len(test_files)} files)")
sys.exit(bool(errors))
PY
then
  exit 1
fi
echo "All checks passed successfully."
