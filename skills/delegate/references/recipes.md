# 场景示例、任务说明模板与常见坑

按需阅读：SKILL.md 的速查不够用时再看这里。下文 `D=<本技能目录>/bin/delegate`。

## 任务说明模板

```text
目标：<要什么结果；一句话能说清>
背景与已定事实：<同事不该重新猜的东西：已排除的原因、已做的决定、相关文件>
范围：<能改哪些目录/文件；不能碰什么>
完成标准：<--accept 命令会自动附上；只读任务写清答复要包含什么，如"每条写 文件:行号、触发场景、后果，按严重程度排序">
约束：<风格、依赖、不要重构无关代码、有疑问写在答复里而不是自作主张>
```

审查类任务再加一句"重点看 A、B，但不限于此"；需要它推翻你的判断时，写成"我怀疑 X，请证实或证伪"。

## 1. 分路独立审查

大范围审查按子系统拆开，各起一个只读任务，范围写清，一个后台 `wait` 收齐：

```bash
$D start --read-only --tier strong --name rv-runtime --prompt-file - <<'EOF'
审查 packages/runtime 最近 4 个提交（git log -4 --stat 可看范围）引入的改动，只列真实缺陷……
EOF
$D start --read-only --tier strong --name rv-api "审查 apps/api 里同一批提交的调度与重试逻辑……"
$D start --read-only --name rv-web "审查 apps/web 同一批提交的交互与文案，列最影响使用的问题"
$D wait            # 放后台；三路都结束才返回
```

审自己刚写的代码时优先用另一家模型（`--tier strong` 即 Codex）：作者自测再多也难看到自己的盲点。拿到结论后逐条去代码里核实，别整段转述。

## 2. 同事修，主控审方向

根因你已经定位、改法需要细心实现时：

```bash
$D run --worktree --tier strong --name fix-lock --accept "go test ./internal/queue/..." --prompt-file - <<'EOF'
目标：修复并发取消时重复释放锁。
已定事实：根因是 cancel() 与 worker 退出路径都调用 release()，两者之间没有同步（queue.go:210、worker.go:88）。
要求：release 幂等且不引入新锁；补一个并发取消的回归测试。不要改公开接口。
EOF
$D diff fix-lock --total      # 审方向：改法对不对、有没有同类问题没顾及
$D reply fix-lock "worker.go 的超时路径也会走 release，一并处理并补测试"   # 返工在同一会话里
$D apply fix-lock
```

## 3. 大任务：先定契约，再防"改测试过关"

迁移、重写这类任务，先把行为写成规格与黑盒测试，再委派；验收命令里禁止改动测试与规格，否则最省事的"通过"就是改测试：

```bash
$D run --worktree --tier strong --name port --timeout 3h --accept-timeout 30m --accept '
  set -e
  git diff --quiet HEAD -- tests/ docs/spec.md      # worktree 中 HEAD 即起点，改了测试或规格直接失败
  make build && make test' --prompt-file task.md
```

交付后仍要看 diff：测试没覆盖到的问题，验收也拦不住。发现漏洞时先补一条测试，再让同事修。

## 4. 看图

```bash
$D start --read-only --image before.png --image after.png "对比两张截图，列出小屏下布局退化的地方"
```

## 5. 批量文案与摘要

便宜档适合量大、可粗筛的活，多路并行，结果由你汇总：

```bash
for f in docs/guide/*.md; do
  $D start --read-only --name "sum-$(basename "$f" .md)" "用三句话概括 $f 的要点与过时之处"
done
$D wait
```

注意整机并发上限（默认 6）：超出会被拒绝，先 `wait` 收一批再放。

## 6. monorepo 的 worktree 配置

`.delegate.json` 放在仓库根：

```json
{
  "env": {"CUDA_VISIBLE_DEVICES": ""},
  "worktree": {
    "copy": [".env"],
    "link": ["third_party/some-submodule", "models/weights"],
    "setup": ["pnpm install --offline --frozen-lockfile", "uv sync --frozen --offline"]
  }
}
```

- `link` 适合只读的子模块与大目录（worktree 里子模块是空目录）；写入会落到原仓库。
- `setup` 在重任务队列里执行；先在一个临时 worktree 里手动跑一遍，确认离线能装齐。
- `env` 在原地运行时同样生效。

## 常见坑

| 现象 | 原因与对策 |
|---|---|
| 同事交付 `delivered`，但改法有问题 | 验收只证明命令通过。看 `diff`；把漏掉的情形补成测试再 `reply` |
| `apply` 报子模块冲突或出现意外的子模块改动 | 同事为跑测试初始化了子模块。合并前在其 worktree 里 `git submodule deinit -f <路径>` 撤掉 |
| worktree 里测试报找不到依赖或路径依赖 | 被忽略的依赖没带过去：在 `.delegate.json` 里 `link` 子模块、`setup` 装依赖 |
| 便宜档连续畸形 | 会话太长：`reply --fresh` 开新会话并写完整说明；或直接 `--tier strong` |
| 被拒绝：并发已满 / 内存不足 | 先 `wait` 收结果或 `stop` 不再需要的任务；不要调大上限绕过 |
| 验收在高负载时超时 | 重检查已整机排队且排队不计时；仍超时说明命令本身慢，调大 `--accept-timeout` |
| 同事读到的代码和你当前的不一样 | 只读任务读的是启动时的快照；你之后的修改它看不到，需要时重新放一个 |
| 任务拆得太碎，开销比收益大 | 适合委派的粒度：有明确完成标准、约 10 分钟到 2 小时的工作量 |

## 什么时候别委派

- 改动只有几行，或写说明比自己做还费事。
- 下一步决策强依赖你脑中尚未写成文字的上下文。
- 需要反复试探、边做边改方向的探索（先自己摸清，再把确定的部分派出去）。
