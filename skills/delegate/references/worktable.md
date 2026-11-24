# 统一工作表与适时提示

全机唯一协调层保存在 `<state_dir>/work.jsonl`（每行一个 JSON 工作项，ID `W001…` 单调不复用），以「一件事从待派到已生效闭环」为粒度，跨多 run、多条续做链与多仓库统一跟踪；核心路径纯确定性、零模型 token。

## 1. 7 态状态机与预留字段

- **静默三态（不打扰）**：`blocked`（等 `wait_on`）、`active`（运行/排队中，无卡住告警）、`deferred`（显式暂缓且未到 `deferUntil`）。
- **行动三态（按优先级排序）**：`attention`（提问/失败/NEEDS_CONTROLLER/待合入/卡住/未读答复）> `ready`（前置满足待派）> `verify`（已合入或已产出，但带 `--needs-verify`、`verify_on`、`artifacts[]`、未勾 `followups[]`、仍有活跃/提问子 run、合入被拒 run 或 `apply --keep-open` 尚未闭环）。
- **终态**：`done`（显式 `close --evidence`、`drop --reason`、或 `apply` 成功后默认自动关闭，文本 `doneWhen` 仅作验收说明不再阻断自动关项）。
- 预留字段：`artifacts[]` 与 `followups[]` 在 6.31 已落盘并支持手动登记与 `note` 映射，供后续自动捕获直接写入。

## 2. 四层渐进式披露

1. **第 0 层**：`delegate work` 首行输出 `工作表：需处理 … · 就绪 … · 待闭环 … · 推进中 … · 等待 … · 暂缓 …`。
2. **第 1 层**：6 列紧凑表（`id` / `repo` / `title` / `state` / `focus` / `next`，单行 ≤100 字）；未绑定工作项的活跃/待处理 run 自动按续做链折叠合表为 `R:<短id>`，不强制先建工作项；默认折叠 `blocked`/`deferred` 计数，`--all` 展开，`--state done` 查已关闭。
3. **第 2 层**：`delegate work show <ID>` 展示完整卡片（完成条件、前置逐条 `✓/⏳`、关联运行链、产物、待办、暂缓/关闭依据）与宿主确定性写出的第 3 层原文指针。
4. **第 3 层**：按卡片指针按需读取 `delegate result <run>`、`delegate diff <run> --total`、任务说明文件或 `docs/owner/README.md`。

## 3. 命令速查

- `delegate work [--owner] [--all] [--state <态>] [--repo <名>] [--rebuild] [--json]`：查看工作表（`--owner` 按 动手/点头/看卡/方向 与 已否 纯文本展示所有者事项，缺字段的既有 Q 项行尾标「缺建议」；`--rebuild` 强制重算派生状态与迁移）。
- `delegate work show <Wxx|Qxx|Dxxx|R:id> [--json]`：查看工作项或所有者事项卡片（Q 项卡片附「在等它的工作」列出挂 `owner:<Qxx>` 的 W 项）。
- `delegate work add "<标题>" [--class feat|unblock|infra|docs] [--files <前缀>] [--done-when "<条件>"] [--needs-verify] [--brief <文件>] [--needs-owner nod|act|card --ask "…" --recommendation "…" --if-undecided "…" --cost "…"] [--verify-on <条件>] [--after-applied <run|Wxx>] [--after-delivered <run>] [--after-work <Wxx>] [--wait-owner <Qxx>] [--after-time <4h|YYYY-MM-DD|ISO>] [--artifact <路径>] [--owner-kind act|nod|card|direction --ask "…" --qid <Qxx|Dxxx> --recommendation "…" --if-undecided "…" --cost "…"] [--auto -- <start参数>]`：立项或新建所有者事项（`Q`/`D` 编号，卡片路径用 `--brief`）。
  - `--class feat|unblock|infra|docs`（`add`/`wait`）：价值类别（功能/解锁/基建/文档）；缺类别不参与补位且 `work` 行尾标「缺类别」。
  - `--needs-owner <nod|act|card>`（`add`/`wait`）：建 `Qxx` 并挂 `owner:Qxx` 进 `blocked`；与 `--owner-kind <act|nod|card>` 均须填 `--ask`（非标题问句）、`--recommendation`、`--if-undecided`、`--cost`（`direction` 除外）；命中审批/对外/删除/不可逆等词且无前置仅警告。
  - `--needs-verify` / `--verify-on <COND>`（`add`/`wait`）：`apply` 默认自动关为 `done`（`closedKind: auto`）；仅显式 `--needs-verify`、未满足 `--verify-on`（`run_applied:`/`work_applied:`/`work_closed:`/`owner:`/`commit_exists:<sha>`，拒 `cmd:`）、`artifacts`、未勾 `followups`、仍有在跑/提问 run、合入未过 run、所有者项或 `apply --keep-open` 时留 `verify` 并说明原因。
  - 缺说明：`add`/`import-md` 没带 `--brief` 照常进 `ready`，Stop 钩子与 TUI 标 `缺说明`，用 `work wait <Wxx> --brief <文件>` 补齐。
- `delegate work wait <Wxx|Qxx> [--class <类>] [--files <前缀>] [同上前置] [--needs-verify|--no-needs-verify] [--needs-owner nod|act|card ...] [--ask … --recommendation … --if-undecided … --cost …] [--verify-on <条件>] [--brief <文件>] [--clear]`：更新类别、路径、前置、闭环策略或说明（同态不重置 `state_since`）；可补齐 `Qxx` 四字段消「缺建议」。
- `delegate work import-dag <FILE> [--dry-run] [--repo <目录>] [--json]`：读 `{"items":[{"key","title","class","brief","doneWhen","after":[key…],"files":[前缀…],"tier","model","thinking"}]}` 批量建项（`after` 转 `work_closed:`、`files` 存前缀并附 `--auto`）；`--dry-run` 仅预览；重 key、成环或 brief 缺失整批拒绝。
- `delegate fill stats [--since 24h] [--repo <目录>] [--all] [--json]`：按 `fill.target` 报冲突率（`>10%`）、返工率（`>28.3%`）、未合入峰值（`>5`）、lane 排队占比（`>15%`）、每 10 交付 agy 周额度与只读占比；越线仅提醒不降档，放开节奏见 [容量](capacity.md)。
- `delegate start --work <Wxx> …`：将 run 绑定到工作项；`delegate reply` 自动继承父 run 的 `workId`。
- `delegate work close <Wxx|Qxx|Dxxx> --evidence "<SHA/测试/所有者结论>"` / `delegate apply --closes <Wxx> <run>`：带证据关闭（关闭所有者事项自动解锁等它的 `owner:Qxx` 下游）。
- `delegate work drop <Wxx|Qxx|Dxxx> --reason "<放弃/否决理由>"`：留痕放弃/否决（不自动解锁下游，TUI 按 `x` 可查「已否」）。
- `delegate work defer <Wxx|run> --reason "<原因>" --until <4h|24h|YYYY-MM-DD|ISO>`：暂缓工作项或将临时 run 升格为暂缓项（必须提供理由与到期时间，取代旧 `deferred.tsv`）；同一项连续暂缓 3 次在工作表标「反复暂缓」（只显示不拦截）。
- `delegate hook stats [--weeks N] [--json]`：只读按周统计 `<state>/stop-hook.jsonl` 触发次数与处理去向（挂了 wait / apply 了 / defer 了 / close 了 / wait_on 了 / 被忽略了）。
- `delegate work import-md <文件> [--dry-run] [--include-stale] [--repo <目录>] [--json]`：解析 Markdown 待办并比对 `history.jsonl` 标出疑似已合入/重复项，每条附源文件行号指针。

## 4. 前置条件与只读边界

- `run_applied:<run>` / `work_applied:<Wxx>`（`--after-applied <run|Wxx>`）：给 run 时检查 `.applied`/`.applied-cleaned`、`history.jsonl` 与 `events.jsonl`（兼容直接 `apply` 与 `merge`）；给 `Wxx` 时合入即解锁（不等 `done`，含环检测，`drop` 不自动解锁）。`delegate queue add` 同样支持 `--after-applied`。
- `run_delivered:<run>`、`work_closed:<Wxx>`（含环检测）、`after_time:<ISO>`（外部等待只用 `--after-time` 到点转 `ready` 或 `work defer --until`，不提供自由文本 `--wait-manual` 或 `cmd:` 探针）。
- `owner:<Qxx>`：优先指向工作表中的所有者事项（`Q`/`D` 关闭后自动解锁下游，否决不自动解锁）；未迁入时回退只读扫描 `docs/owner/README.md`。
- **TUI 工作视图**：`delegate tui` 按 `w`/`5` 切到工作表（L0 计数并在有老化项时显示 `老化 N`、所有者事项置顶、依赖链缩进、老化项行尾标 `停 Nh`、缺 brief 的 `ready` 项行尾标 `缺说明`、缺字段的 Q 项行尾标 `缺建议`、Enter 看工作卡含 Q 项「在等它的工作」与 Markdown 卡片正文、`t`/`→` 跳关联 run、`o`/`x` 过滤、`<70` 列手机双行紧凑布局）。

## 5. 适时提示、Stop Hook 分级与迁移

- **尾部追加（零前缀变动）**：所有提示仅追加在命令 stdout 尾部并附宿主写出的原文位置指针：
  - `wait` / `wait --since`：附所属工作项 1 行及受影响下游（≤3 行，沿 `parent` 链把续做叶子归到根 run 并匹配 `run_applied:`/`work_applied:`/`work_closed:`）。
  - `apply`：立即求值 `run_applied`、`work_applied` 与 `work_closed`，打印被解锁的可执行 `start --work` 命令或自动派发结果、自动关项证据或留在 `verify` 的具体原因（`--keep-open` 强制留 `verify`）。
  - `merge`：提示本次批次合入落盘后将解锁哪些下游项。
  - `clean`：对未关闭工作项印 1 行保留确认，不阻断清理、不写 Stop hook 拦截事件。
- **Stop Hook 合一与分级**：`hooks/claude-code-stop.py` 为唯一 Stop 钩子，严格零锁、零 git 只读 `state/index.json` 与快照，保留按 caller 隔离游标；已合入、已 clean、已回复或已被任一会话读过结论（`resultReadAt`）的交付/提问类事件直接判旧放行，非本会话发起的 run 仅在原 caller 失效且超过无人认领阈值（默认 `DEFAULT_ORPHAN_TAKEOVER_HOURS=6h`，可由 `DELEGATE_ORPHAN_TAKEOVER_HOURS` 配置）时拦截接盘，否则只计数不拦；无更高优阻断且本仓库有空闲位时，工作项老化提醒只拦最近 `add`/`wait`/`start --work` 的负责会话（未记录或负责会话失效超时后才允许接手），`<state>/work-remind.json` 按「项 × 会话」维护 6h 冷却；延后统一走 `delegate work defer`，每次拦截写入 `<state>/stop-hook.jsonl` 供 `delegate hook stats` 统计。已 clean/归档的 run 不再影响工作项状态，零改动交付在工作表里显示为「已收取」。
- **自动迁移**：升级或读写 `work.jsonl` 时自动幂等迁入已登记仓库未完成的 `notes.json`（入 `verify`）与未过期的历史 `deferred.tsv`（入 `deferred`），遇旧 `manual:` 前置一次性去掉、原文按「原手动前置：…」追加进 `followup`、无其他未满足前置即转 `ready` 并写回与记一条 `work_migrated` 事件；Stop hook 不再读取 `deferred.tsv`；`delegate note` 保留为薄壳读写 `work.jsonl`。
