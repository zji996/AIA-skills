# 统一工作表与适时提示

全机唯一协调层保存在 `<state_dir>/work.jsonl`（每行一个 JSON 工作项，ID `W001…` 单调不复用），以「一件事从待派到已生效闭环」为粒度，跨多 run、多条续做链与多仓库统一跟踪；核心路径纯确定性、零模型 token。

## 1. 7 态状态机与预留字段

- **静默三态（不打扰）**：`blocked`（等 `wait_on`）、`active`（运行/排队中，无卡住告警）、`deferred`（显式暂缓且未到 `deferUntil`）。
- **行动三态（按优先级排序）**：`attention`（提问/失败/NEEDS_CONTROLLER/待合入/卡住/未读答复）> `ready`（前置满足待派）> `verify`（已合入或已产出，但 `doneWhen`、`artifacts[]` 或未完成 `followups[]` 尚未闭环）。
- **终态**：`done`（显式 `close --evidence`、`drop --reason`、或无未闭环项的简单项自动关闭）。
- 预留字段：`artifacts[]` 与 `followups[]` 在 6.31 已落盘并支持手动登记与 `note` 映射，供后续自动捕获直接写入。

## 2. 四层渐进式披露

1. **第 0 层**：`delegate work` 首行输出 `工作表：需处理 … · 就绪 … · 待闭环 … · 推进中 … · 等待 … · 暂缓 …`。
2. **第 1 层**：6 列紧凑表（`id` / `repo` / `title` / `state` / `focus` / `next`，单行 ≤100 字）；未绑定工作项的活跃/待处理 run 自动按续做链折叠合表为 `R:<短id>`，不强制先建工作项；默认折叠 `blocked`/`deferred` 计数，`--all` 展开，`--state done` 查已关闭。
3. **第 2 层**：`delegate work show <ID>` 展示完整卡片（完成条件、前置逐条 `✓/⏳`、关联运行链、产物、待办、暂缓/关闭依据）与宿主确定性写出的第 3 层原文指针。
4. **第 3 层**：按卡片指针按需读取 `delegate result <run>`、`delegate diff <run> --total`、任务说明文件或 `docs/owner/README.md`。

## 3. 命令速查

- `delegate work [--owner] [--all] [--state <态>] [--repo <名>] [--rebuild] [--json]`：查看工作表（`--owner` 按 动手/点头/看卡/方向 与 已否 纯文本展示所有者事项；`--rebuild` 强制重算派生状态与迁移）。
- `delegate work show <Wxx|Qxx|Dxxx|R:id> [--json]`：查看工作项或所有者事项卡片。
- `delegate work add "<标题>" [--done-when "<条件>"] [--brief <文件>] [--after-applied <run>] [--after-delivered <run>] [--after-work <Wxx>] [--wait-owner <Qxx>] [--after-time <4h|YYYY-MM-DD|ISO>] [--wait-manual "<说明>"] [--artifact <路径>] [--owner-kind act|nod|card|direction --ask "<问题>" --qid <Qxx|Dxxx> --recommendation "<建议>" --if-undecided "<不定会怎样>" --cost "<成本>"] [--auto -- <start参数>]`：立项或新建所有者事项（`Q`/`D` 编号，卡片路径用 `--brief`）。
- `delegate work wait <Wxx> [同上前置选项] [--clear]`：给已有工作项挂载或更新前置（含 `--after-applied` 与 `--auto` 自动派发）。
- `delegate start --work <Wxx> …`：将 run 绑定到工作项；`delegate reply` 自动继承父 run 的 `workId`。
- `delegate work close <Wxx|Qxx|Dxxx> --evidence "<SHA/测试/所有者结论>"` / `delegate apply --closes <Wxx> <run>`：带证据关闭（关闭所有者事项自动解锁等它的 `owner:Qxx` 下游）。
- `delegate work drop <Wxx|Qxx|Dxxx> --reason "<放弃/否决理由>"`：留痕放弃/否决（不自动解锁下游，TUI 按 `x` 可查「已否」）。
- `delegate work defer <Wxx|run> --reason "<原因>" [--until 4h|24h|YYYY-MM-DD|ISO]`：暂缓工作项或将临时 run 升格为暂缓项。
- `delegate work import-md <文件> [--dry-run] [--include-stale] [--repo <目录>] [--json]`：解析 Markdown 待办并比对 `history.jsonl` 标出疑似已合入/重复项，每条附源文件行号指针。

## 4. 前置条件与只读边界

- `run_applied:<run>`（`--after-applied`）：检查 run 目录 `.applied`/`.applied-cleaned`、`history.jsonl` 与 `events.jsonl`，兼容直接 `apply` 与经 `delegate merge` 后 `apply --keep-commits`。`delegate queue add` 同样支持 `--after-applied`。
- `run_delivered:<run>`、`work_closed:<Wxx>`（含环检测）、`after_time:<ISO>`、`manual:<说明>`。
- `owner:<Qxx>`：优先指向工作表中的所有者事项（`Q`/`D` 关闭后自动解锁下游，否决不自动解锁）；未迁入时回退只读扫描 `docs/owner/README.md`。
- **TUI 工作视图**：`delegate tui` 按 `w`/`5` 切到工作表（L0 计数、所有者事项置顶、依赖链缩进、Enter 看工作卡与 Markdown 卡片正文、`t`/`→` 跳关联 run、`o`/`x` 过滤、`<70` 列手机双行紧凑布局）。

## 5. 适时提示、Stop Hook 分级与迁移

- **尾部追加（零前缀变动）**：所有提示仅追加在命令 stdout 尾部并附宿主写出的原文位置指针：
  - `wait` / `wait --since`：附所属工作项 1 行及受影响下游（≤3 行）。
  - `apply`：立即求值 `run_applied` 与 `work_closed`，打印被解锁的可执行 `start --work` 命令或自动派发结果，并说明本项留在 `verify` 的原因。
  - `merge`：提示本次批次合入落盘后将解锁哪些下游项。
  - `clean`：对未关闭工作项印 1 行保留确认，不阻断清理、不写 Stop hook 拦截事件。
- **Stop Hook 分级**：`hooks/claude-code-stop.py` 只拦当下阻断项（未读告警事件含 `work_ready`/`work_defer_expired`/`work_stale` 经游标消费一次、提问、无 waiter 在跑、未暂缓的已交付未合入）；不再因 `blocked`、`deferred`、未超期 `verify`（含 24h 内合入后待办）或积压 `ready` 反复拦。`verify` 超 24h 或 `ready` 超 48h 按单项 24h 冷却发低频 `work_stale` 事件。
- **自动迁移**：首次读写 `work.jsonl` 时自动幂等迁入已登记仓库未完成的 `notes.json`（入 `verify`）与未过期的 `deferred.tsv`（入 `deferred`）；`delegate note` 保留为薄壳读写 `work.jsonl`，不再写入 `notes.json`。
