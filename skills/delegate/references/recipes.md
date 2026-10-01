# 场景示例、任务说明模板与常见坑

按需阅读：SKILL.md 的速查不够用时再看这里。下文 `D=<本技能目录>/bin/delegate`。

## 任务说明模板

```text
目标：<要什么结果；一句话能说清>
背景与已定事实：<同事不该重新猜的东西：已排除的原因、已做的决定、相关文件>
范围：<能改哪些目录/文件；不能碰什么>
完成标准：<--accept 命令会自动附上；只读任务写清答复要包含什么，如"每条写 文件:行号、触发场景、后果，按严重程度排序">
约束：<风格、依赖、不要重构无关代码、有疑问写在答复里而不是自作主张>
失败语义：<新增旁路能力时写：哪些失败中止主路径，哪些只记诊断、在下个边界重试；取消必须向上传递。仓库入口文件已写明分级时可省略>
证据：<审查/调研类：每条关于现状的断言附 文件:行号；区分"现状事实"与"建议"；不用形容词，给表格加结论>
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

想边收边看就改成逐个收取：`$D wait --stream`（宿主能把每行输出变成通知时，每路完工一行）或后台 `$D wait --any`（先完工的先返回，处理完再放同一条）。写入任务边收边合时，结论行的 `sourceDrift.overlap` 列出你合入别的结果后与它重叠的文件。

Claude Code 的 Monitor 能把后台输出逐行转事件，优先 `wait --stream`；只有进程结束通知时重复后台 `wait --any`。直接 `--agent pi` 的结论显示 `pi/cheap`、`agentPinned: true`，固定同事不自动升档。

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
$D reply --wait fix-lock "worker.go 的超时路径也会走 release，一并处理并补测试"   # 返工并等结论；省略 --wait 可后台启动
$D apply fix-lock
```

真实模型评测作为证据单独收集，确定的检查仍作验收：

```bash
$D run --worktree --name model-fix --accept 'cargo test -p engine' \
  --evidence 'cargo xtask model-eval' --evidence-timeout 30m '修复模型调用的取消传递'
$D reply model-fix '补上并发取消路径'   # 沿用验收和证据
$D reply model-fix --no-evidence '只补文档中的说明'
```

验收通过后证据在 lane 运行，排队不计时；证据失败/超时不改 `delivered` 和退出码，读 `status --json` 的 `evidence` 与日志后判断。配置默认 `evidence` 仅用于写入。若已手动用 git 合入全部改动，`status`/`wait`/`clean` 检查源工作树内容后自动记“已合入（主干已含改动）”，可直接清理；只合入部分仍提示 apply。

## 3. 大任务：先定契约，再防"改测试过关"

迁移、重写这类任务，先把行为写成规格与黑盒测试，再委派；验收命令里禁止改动测试与规格，否则最省事的"通过"就是改测试：

```bash
$D run --worktree --tier strong --name port --timeout 3h --accept-timeout 30m \
  --protect tests/ --protect docs/spec.md \      # 改了测试或规格直接判 rejected，验收都不跑
  --accept 'make build && make test' --prompt-file task.md
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

容量有两层：整机默认 12 个任务/Codex 6，每仓库 8/Codex 4；git worktree 与主仓库同计。超出时提示层级、计数和相关任务，先 `wait` 收一批再放。

## 6. monorepo 的 worktree 配置

`.delegate.json` 放在仓库根：

```json
{
  "env": {"CUDA_VISIBLE_DEVICES": ""},
  "agentDeny": [{"argv": ["cargo", "xtask", "check"], "hint": "全量由主控合入后跑；改跑 cargo test -p <crate> 与 cargo clippy -p <crate>"}],
  "accept": "./scripts/check.sh && python3 -m unittest discover -s tests",
  "evidence": "cargo xtask model-eval",
  "generated": {"paths": ["src/generated/"], "command": "./scripts/generate.sh"},
  "worktree": {
    "copy": [".env", ".local/scan"],
    "link": ["third_party/some-submodule", "models/weights"],
    "setup": ["pnpm install --offline --frozen-lockfile"],
    "writeSetup": ["uv sync --frozen --offline"]
  }
}
```

- `copy` 将被 git 忽略的文件或目录独立复制进 worktree；源不存在时跳过并提示。
- `link` 适合只读的子模块与大目录（worktree 里子模块是空目录）；写入会落到原仓库。
- `setup` 在重任务队列里执行；先在一个临时 worktree 里手动跑一遍，确认离线能装齐。
- `writeSetup` 只在写入任务里、`setup` 之后执行：装起来慢、只有跑测试才用得上的环境（如含 torch 的 venv）放这里，只读侦察不必等。
- `env` 在原地运行时同样生效。
- `agentDeny` 按 argv 前缀拦截同事的全量检查（退出 77，打印 hint），同事通过 lane 调用也拒绝；验收与主控 lane 保持可用。同事跑相关包检查，主控合入一批后统一全量检查。
- 只禁无过滤的数据库全量检查时用 `{"argv":["cargo","xtask","infra-test"],"exact":true,"hint":"带过滤参数跑相关用例"}`；额外参数放行。用户/仓库去重和 `allow: true` 撤销都按 `(argv, exact)`，撤销时保留同样的 exact。
- 顶层 `accept` 只作用于写入任务；显式 `--accept` 覆盖，`--no-accept` 关闭。并行写入用 `--protect` 划分文件所有权；验收覆盖相关 ratchet、contracts、docs 门禁，合并后再跑全量。
- `generated.paths` 的匹配语义同 `--protect`（目录以 `/` 结尾）。这些生成文件仍出现在改动清单和 diff 中，但 `apply` 不合并或覆盖它们；合并其他文件后，在源仓库根通过 lane 运行 `sh -c` 执行 `generated.command`（设了 `generated.inputs` 时只在合并路径命中它时运行），输出写入 run 目录的 `generate.log`。`--dry-run` 只报告动作；生成失败时已合并文件保留，需查看日志后重试。见 `docs/delegate-spec.md` §6.4。
- 界面实现默认 `--tier cheap`（Pi 前端审美与交互明显好于 Codex），说明里要截图路径；强档只接状态、数据与接口逻辑。

## 常见坑

| 现象 | 原因与对策 |
|---|---|
| 同事交付 `delivered`，但改法有问题 | 验收只证明命令通过。看 `diff`；把漏掉的情形补成测试再 `reply` |
| 改动清单里出现 `submodule contents` | 5.1 起，同事为跑测试而初始化的子模块会被自动识别并忽略；仍然出现，说明它在子模块里改了文件或切了提交，要看 `diff` |
| worktree 里测试报找不到依赖或路径依赖 | 被忽略的依赖没带过去：在 `.delegate.json` 里 `link` 子模块、`setup` 装依赖 |
| 便宜档的审查只看到现状、说不出"改了什么" | 它在非 git 目录或 `--in-place` 下运行：只读 Pi 此时没有 shell，跑不了 `git log`/`git diff`。去掉 `--in-place`（默认的快照里它有全部工具），或先把 diff 导出成文件交给它 |
| 便宜档连续畸形 | 会话太长：`reply --fresh` 开新会话并写完整说明；或直接 `--tier strong` |
| 被拒绝：并发已满 / 内存不足 | 先 `wait` 收结果或 `stop` 不再需要的任务；不要调大上限绕过 |
| 强档名额被写入任务占满 | 写入默认强档；整机 Codex 6、每仓库 4。读拒绝中的层级与相关任务，先收取该层结果 |
| 强档把附带能力做成致命路径（记忆去重失败让会话封存失败） | 仓库规则写着“失败关闭”，`?` 又是最省事的写法；说明没给失败分级。说明里写失败语义，或先在仓库入口文件写清哪类失败关闭、哪类只作诊断 |
| 便宜档的实现 `catch {}` 静默吞错、答复称“全覆盖” | 只求验收通过 | 合并前 `rg 'catch \{' ` 扫一遍；仓库最好用 lint 禁掉空 catch。答复里的数字以 diff 为准 |
| 多路分别验收通过，合并后编译失败（主控补的 DTO 字段让另一路的测试夹具缺字段） | 各路验收的是自己的快照 | 合并后在主干跑全量；DTO 类改动尽量先合并再放出依赖它的任务 |
| 几路写入任务并行改同一个 app | 各开 `--worktree`，并用 `--protect` 把别路负责的文件划出去，冲突在 `apply` 前就暴露；合并后跑一次仓库级全量检查 |
| 验收通过，合并后仓库级检查失败 | 验收只覆盖了单个 app，漏了仓库级门禁（ratchet、contracts、docs 审计）：验收命令带上这些 gate，或在 `.delegate.json` 顶层 `accept` 声明默认验收 |
| 验收在高负载时超时 | 重检查已整机排队且排队不计时；仍超时说明命令本身慢，调大 `--accept-timeout` |
| 同事写到一半 timeout，返工预算已用完 | 复制结论的 `next` 续做，上一轮 timeout 的 reply 不计返工次数；其他失败仍按原规则 |
| 同事全量检查被拒绝（77） | 按 hint 改跑相关检查；结论的 denied 统计拦截次数，主控合入一批后统一跑全量 |
| 带过滤参数的 infra-test 也被拒绝 | 前缀规则包含所有后缀；只禁无参数版本应设 `exact: true` |
| 模型评测超时导致 rejected | 将不确定的评测放进 `--evidence`，让确定的门禁作 `--accept`；证据超时只记诊断 |
| 同事读到的代码和你当前的不一样 | 只读任务读的是启动时的快照；你之后的修改它看不到，需要时重新放一个 |
| 清理后找不到长报告的全文 | `wait` 显示答复头尾共 6000 字；只看过截断答复的任务 `clean --finished` 默认保留。要留存就先 `result <name> > 文件`，或调大 `DELEGATE_RESULT_CHARS` |
| 便宜档给的配置键、参数名不存在 | 仓库外知识会被编造：要求附出处，并对照 schema、源码或 `--help` 核实后再采纳 |
| 任务拆得太碎，开销比收益大 | 适合委派的粒度：有明确完成标准、约 10 分钟到 2 小时的工作量 |

## 什么时候别委派

- 改动只有几行，或写说明比自己做还费事。
- 下一步决策强依赖你脑中尚未写成文字的上下文。
- 需要反复试探、边做边改方向的探索（先自己摸清，再把确定的部分派出去）。
