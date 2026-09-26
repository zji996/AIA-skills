#!/usr/bin/env bash
# Print a draft handoff ledger with the verifiable facts filled in.
set -euo pipefail

usage() {
  echo "usage: handoff-snapshot.sh [--repo <dir>] [--commits <n>]" >&2
  exit 2
}

repo=$PWD
commits=5
while (( $# )); do
  case "$1" in
    --repo) [[ $# -ge 2 ]] || usage; repo=$2; shift 2 ;;
    --commits) [[ $# -ge 2 && "$2" =~ ^[0-9]+$ ]] || usage; commits=$2; shift 2 ;;
    -h|--help) usage ;;
    *) usage ;;
  esac
done
[[ -d "$repo" ]] || { echo "not a directory: $repo" >&2; exit 2; }
cd "$repo"

in_git=0
if top="$(git rev-parse --show-toplevel 2>/dev/null)"; then
  in_git=1
  cd "$top"
fi

reads=()
for file in AGENTS.md CLAUDE.md docs/current.md; do
  [[ -f "$file" ]] && reads+=("\`$file\`")
done
shopt -s nullglob
for file in .local/plan/*.md; do
  reads+=("\`$file\`")
done
shopt -u nullglob

echo "继续 $(basename "$PWD")：<目标名称>。"
if (( ${#reads[@]} )); then
  echo "先读取 ${reads[*]}。"
fi
echo
echo "当前状态："
if (( in_git )); then
  echo "- 分支: $(git branch --show-current 2>/dev/null || echo '(detached)')"
  echo "- Git HEAD: $(git log -1 --format='%h %s' 2>/dev/null || echo '(no commits)')"
  dirty="$(git status --short --untracked-files=normal)"
  if [[ -z "$dirty" ]]; then
    echo "- 工作区: 干净"
  else
    total="$(wc -l <<<"$dirty")"
    echo "- 工作区: ${total} 项未提交 <说明保留原因>"
    head -n 20 <<<"$dirty" | sed 's/^/    /'
    (( total > 20 )) && echo "    ...（其余 $((total - 20)) 项见 git status）"
  fi
  if (( commits > 0 )) && git rev-parse -q --verify HEAD >/dev/null; then
    echo "- 最近提交:"
    git log -"$commits" --format='    %h %s (%cr)'
  fi
else
  echo "- 非 git 仓库: $PWD"
fi

runs=()
declare -A unmerged=()
shopt -s nullglob
for meta in .local/run/pi/*/meta.json; do
  dir="$(dirname "$meta")"
  if [[ ! -f "$dir/exit_code" ]]; then
    runs+=("$(basename "$dir") 可能仍在运行")
  elif [[ ! -f "$dir/.delivered" ]]; then
    runs+=("$(basename "$dir") 已结束，结果未读取（exit $(cat "$dir/exit_code")）")
  fi
  # --worktree 的改动留在独立 worktree，直到 `delegate apply`；同一对话只报最新一轮。
  # 只读任务的 worktree 只是供阅读的快照，没有可合并的改动。
  # 按 JSON 键匹配、容忍任意空白：delegate 的 Python 与 Rust 实现写出的格式不同。
  worktree="$(grep -oE '"path"[[:space:]]*:[[:space:]]*"[^"]*"' "$meta" | head -1 | sed -E 's/.*:[[:space:]]*"([^"]*)"$/\1/' || true)"
  if grep -qE '"mode"[[:space:]]*:[[:space:]]*"read-only"' "$meta"; then
    worktree=
  fi
  if [[ -f "$dir/exit_code" && -n "$worktree" && -d "$worktree" ]]; then
    if [[ -f "$dir/.applied" ]]; then
      unset "unmerged[$worktree]"
    else
      unmerged[$worktree]="$(basename "$dir")"
    fi
  fi
done
shopt -u nullglob
for worktree in "${!unmerged[@]}"; do
  runs+=("${unmerged[$worktree]} 的 worktree 改动未合并：$worktree（delegate apply 或 clean）")
done
if (( ${#runs[@]} )); then
  echo "- 委派任务:"
  printf '    %s\n' "${runs[@]}"
fi

cat <<'EOF'
- 已完成:
  - <结果：事实与达成标准>

验证状态：
- 通过的验证: <实际运行过的命令与结果>
- 未运行/待验证: <项目及原因>

卡点与风险：
- <需要决策或阻碍推进的事项；没有写"无">

下一步行动：
1. <立刻可执行的第一个动作>

关键约束：
- <最容易被遗忘或破坏的原则>
EOF
