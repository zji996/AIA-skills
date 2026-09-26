---
name: delegate
description: 把可独立验收的任务交给同事 Agent 在后台并行完成，你只收结果并把关。遇到这些情况时主动使用，不必等用户开口：要读很多文件但只需结论；改动前想要第二意见或独立审查；有两个以上互不依赖、能用命令验收的子任务；需要有人看截图或设计图给意见。同事分两档：便宜档 Pi（Gemini，读材料、摘要、文案、看图）与强档 Codex（GPT，写代码、严密审查），只读默认便宜档、写入默认强档，便宜档失败自动升档。脚本代跑验收命令，只给一行结论与下一步，过程不进主控上下文；整机并发与重检查有上限。Use to delegate or parallelize work proactively, and to get a second opinion, code review or image review from Gemini (Pi) or GPT (Codex), judged by results.
license: MIT
compatibility: Linux x86_64 或 aarch64；入口是安装时下载的静态二进制 bin/delegate，不需要 Python；需要所选同事的 CLI：pi 或 codex。
metadata:
  version: "5.1.1"
  binary: delegate
  exclude-agents: pi
---

# Delegate（按结果委派）

你是主控：把边界清楚的任务交给同事，自己继续推进，回来只看结果。**是否采纳、如何整合始终由你决定。** 同事不能再委派，所有结果都回到你这里。

## 三步用法

```bash
D=<本技能目录>/bin/delegate
$D start --read-only --name review-api "审查 apps/api 的错误处理，只列真实缺陷，写明文件:行号"   # 1. 放出（可连放多个）
$D wait                                                                                  # 2. 等：放后台，结束时会返回
#                                                                                        # 3. 按结论行的 next 处理
```

- **等待**：能后台运行并在结束时收到通知的环境，把一个 `wait`（或 `run`）放后台，不要用 `status` 轮询；单次调用有时长上限的环境加 `--max 4m`，返回 75 就稍后再 `wait`。`wait` 不带参数时等所有未结束、未读取的任务。
- **结论**：每个任务一行 JSON，随后是改动清单与答复。`next` 字段给出下一步（含可复制的命令），没有 `next` 就是答复本身即交付物。

## 场景速查

| 想要 | 命令 |
|---|---|
| 读材料、只要结论 | `$D start --read-only "…"` |
| 独立审查、第二意见 | `$D start --read-only --tier strong "…"`（多路并行时各起一个，按子系统拆开） |
| 看截图或设计图 | `$D start --read-only --image shot.png "…"` |
| 改代码，测试通过才算完 | `$D run --worktree --accept "make check" "…"`，满意后 `$D apply <name>`；不许同事碰的路径加 `--protect tests/ --protect docs/spec.md` |
| 接着上一轮追问或返工 | `$D reply <name> "…"`（同一会话、同一 worktree）；开新会话加 `--fresh` |
| 自己跑重检查 | `$D lane make check`（与同事的验收排队，一次一个） |
| 看改了什么 / 停掉 / 清理 | `$D diff <name>` / `$D stop <name>` / `$D clean --finished` |

多行说明用 `--prompt-file -` 加 heredoc；总加 `--name`，之后用它指代整段对话。更多完整示例、任务说明模板与常见坑见 [references/recipes.md](references/recipes.md)。

## 选档位

| `--tier` | 默认给 | 适合 | 不适合 |
|---|---|---|---|
| `cheap`（Pi） | 只读任务 | 广度侦察（在代码里找事实、列清单）、界面与前端审查和实现、文案、看图；重要审查时作为另一路视角 | 需要严密判断与风险评估的逻辑（量化边界、并发、事务、迁移） |
| `strong`（Codex） | 写入任务 | 写代码、疑难 bug、严密审查与风险评估、核实便宜档的结论 | 只图省事的批量杂活 |

便宜档答复畸形、出错、超时或验收失败时，只要它还没改过文件（或是只读任务），脚本自动用强档重跑一次，结论带 `escalatedFrom`。`--agent pi|codex` 可直接指定同事，此时不升档。

## 写好任务说明

同事不知道你的会话，说明需要自足：**目标**（要什么结果）、**边界**（能改哪、不能改哪）、**完成标准**（`--accept` 命令，或答复要包含什么）、**已知事实与决定**（别让它重新猜）。验收命令会自动附在说明末尾，并教同事用 `lane` 自检。

## 经验建议

以下来自实际使用，是建议而非规则，按具体情况取舍。

- **续接还是新开**：返工建立在它已有理解上（审查意见、补测试）时，`reply` 通常最省事，不必重读材料。会话很长、答复开始畸形或方向已变，`--fresh` 往往更好。worktree 是启动时的快照，你之后在自己工作区做的改动（比如新写的测试）它看不到，这种时候另起 `start` 更合适。
- **什么时候自己改**：根因已经定位、改动很小、你清楚该怎么改时，自己改常常更快——派发、等待、审查、合并有固定开销。同一个问题返工一两轮仍不见好转，也值得考虑接手。需要只有你知道的背景时，先自己做，等能写成自足的说明再派。
- **两档怎么搭配**：同题对比中，Pi 找到的具体代码事实更多、但偶有把现状说混；Codex 更准确、风险意识更强。分量重的审查可以两路并行、由你合并，效果通常好于任一方。
- **审查类说明**：给出重点，但注明不限于此；给待证伪的假设，而不是结论；请它为关于现状的断言附上 `文件:行号`，能让说错的地方当场暴露。
- **受保护路径**：重写、迁移这类任务，最省事的"通过"方式往往是改测试，可以用 `--protect tests/` 之类把它们保护起来。

更多示例、任务说明模板与常见坑见 [references/recipes.md](references/recipes.md)。

## 读结论并把关

| state | 含义 |
|---|---|
| `delivered` | 已答复且验收命令通过——只说明命令过了，改法是否合适仍要看 `diff` |
| `answered` | 已答复，未设验收；关键结论去代码里核实 |
| `rejected` | 验收命令失败，`accept.tail` 有输出末尾 |
| `malformed` / `failed` / `timeout` / `killed` / `crashed` / `stopped` | 答复畸形 / 出错（见 `error`） / 超时 / 进程异常 / 被终止 |

state 只描述答复。只读任务改了文件时仍是 `answered`，另带 `readOnlyViolation`（留在它自己的 worktree，不会合并）或 `workspaceChanged`（`--in-place` 时无法归属）。改动清单按运行前后的工作区快照计算：shell 改的也算，运行前已有的改动与验收副产物不算。退出码：`0` delivered/answered，`1` 其他结局，`2` 用法错误或被拒绝，`75` 仍在运行。

## 边界

1. **容量**：整机同时最多 8 个任务、其中强档 4 个，可用内存低于 4 GB 时拒绝启动；重检查（验收、setup、`lane`）整机一次一个，排队不计时。超出即拒绝并列出运行中的任务——先 `wait` 收一批。上限由用户设定（见 references），同事不要自行调整。
2. **只读**：git 仓库里默认读启动时的工作区快照（独立 worktree，含未提交改动），你可以同时改代码；要读实时工作区加 `--in-place`。在快照里两位同事都有全部工具（能看 git 历史、跑测试），只读靠约定与事后核对，写了也只留在它自己的 worktree；非 git 目录或 `--in-place` 时 Pi 只剩读文件、搜索和列目录。
3. **写入**：原地写入同一目录同时只能有一个，且你同时改的文件会算进它的改动；要并行或不想被打扰就加 `--worktree`。
4. **worktree 依赖**：git 忽略的依赖不会带过去，在仓库根 `.delegate.json` 声明 `copy` / `link` / `setup`；顶层 `env` 注入同事、验收和 setup，例如 `{"CUDA_VISIBLE_DEVICES": ""}` 让同事碰不到 GPU。别 link `node_modules`/`.venv`。
5. **超时**：`--timeout` 默认 Pi 15 分钟、Codex 30 分钟；到时若仍在执行命令会宽限最多 50%。任务太大就拆小。

命令与全部选项见 `$D --help`；run 目录、文件、环境变量与清理策略见 [references/output-and-files.md](references/output-and-files.md)。
