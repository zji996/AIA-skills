# 合入员（`delegate merge`）与发散侦察（`delegate radar`）

## 一、合入员（`delegate merge`）

把多个已完成写入 run 在**独立集成 worktree** 中串行合入，产出提交链与一页报告 `merge-report.md`；合入员不 `git push`、不改 ADR、不碰受保护路径、不代解冲突、不改模型可见文字。

### 命令

```bash
delegate merge <run>... [--name NAME] [--json]   # 按指定顺序串行合入
delegate merge --ready [--name NAME] [--json]    # 取本仓所有已交付、验收通过、审查 PASS 或已签字的未合入 run（按时间 FIFO）
delegate merge list [--all] [--json]             # 查看合入批次与报告（--all 跨仓库）
delegate apply --keep-commits <merge-run>        # 主控确认后把提交链原样拿到源仓库，push 由主控执行
```

### 串行合入流程

1. 基于源仓库当前 `HEAD` 新建独立集成 worktree（保留 `worktree.copy`/`link`/`setup` 配置，托管路径不入提交）。
2. 逐个 3-way 合并每个 run 的净改动（复用 `apply` 的合并逻辑与 `appendOnly` 规则）。
3. 合并路径命中 `generated.inputs` 时，经 `lane` 重跑 `generated.command`。
4. 每合完一个 run，经 `lane` 跑一次仓库 `accept`。
5. 按主题提交：原样沿用同事首个提交标题，提交信息末尾附 `Delegate-Run: <run>` trailer。
6. 全部合完后，经 `lane` 跑一次 `mergeAccept`（`.delegate.json` 可选，默认等于 `accept`）。
7. 产出 `merge-report.md`（每 run 一行：状态、提交、验收、是否可自主合入、跳过原因）和结构化 `merge-report.json`，事件流记录 `merge_started` / `merge_item` / `merge_done`（TUI「3合入」标签页可读）。

### 跳过与回退条件

遇到下列任一情况，将集成 worktree 原子回退到上一个全绿提交，报告标 `SKIPPED` 并写明原因，接着处理下一个 run：

- 语义冲突（3-way 合并产生冲突或冲突标记）；
- 重新生成失败，或仓库验收测试变红；
- 命中 `defaults.protect`（含 ADR 如 `docs/decision/`）；
- 命中 `controllerReviewPaths` 但未签字，或 `delegate sign --check` 失效，或合入后受控路径 diff 哈希与签字不一致；
- 未交付、仍在运行、或审查为 `REWORK` / 未签字 `NEEDS_CONTROLLER`。

### 自主合入白名单（`autoMerge`）

仓库 `.delegate.json` 可配 `autoMerge`（默认空数组）。**全部提交文件均落在白名单内、审查为 `PASS`、且未命中受保护路径或 `controllerReviewPaths`** 时，报告标「可自主合入」，否则标「待主控点头」。

规则按顺序求值（类似 `.gitignore`，后者覆盖前者）：

- `<glob>`：匹配新增、修改、删除（支持 `*`、`?`、`**`、末尾 `/`）；
- `!<glob>`：排除匹配路径（如 `!docs/decision/**`）；
- `add:<glob>`：仅匹配**纯新增**文件，修改或删除已有文件不放行（如 `add:**/tests/**`、`add:**/*_test.rs`）；
- `!add:<glob>`：排除纯新增匹配路径。

---

## 二、发散侦察（`delegate radar`）

低成本夜间/周期性侦察：先跑确定性采集算指纹，无变化零 token 退出；有变化才派只读侦察员，Host 验锚点去重后落盘候选板，主控只挑不自动派活。

### 命令与配置

```json
{
  "radar": {
    "collect": [
      {"name": "recent-log", "command": "git log --since='24 hours ago' --oneline"},
      {"name": "gaps", "command": "cargo xtask capabilities --gaps"}
    ],
    "tier": "scout",
    "timeout": "5m"
  }
}
```

- `radar.collect`：必填，顺序执行的采集命令列表（字符串或 `{"name","command"}`）；可选 `tier`（仅限只读 `scout`/`review`，默认 `scout`）、`model`、`thinking`、`timeout`。
- `delegate radar [run] [--force] [--dry-run] [--json]`：顺序执行采集并算 SHA-256 指纹。
  - **指纹未变**：零 token 退出，仅写一条 `radar_unchanged` 事件。
  - **指纹变化**：派只读侦察员（不能写文件、不能派活，越权写入失败关闭），输入带行号（`collect#N:LN`）的采集输出与上一份候选板。
- **证据锚点与候选板**：每条候选必须含可核验锚点（`文件:行号`、`commit <短哈希>` 或采集输出行号 `collect#N:LN`），无锚点候选由 Host 自动丢弃；按指纹去重后由 Host 写入 `<state>/radar/<repo>/board.md`（与 `board.json`），每条含 ID、标题、证据、建议做法、粗估成本、能否直接写成任务说明，并发 `radar_done` 事件（TUI「4候选」标签页可读）。
- `delegate radar list|board [--all] [--json]`：查看候选板（`--all` 跨仓库）。
- `delegate radar pick <id> [--out PATH] [--json]`：把选中候选生成一份任务说明草稿，**不自动派活**。

### 定时运行安装说明（不自动安装）

默认推荐夜间窗口 **00:30–04:30 每两小时一次**（00:30、02:30、04:30）。也可用 `delegate radar schedule` 为当前仓库直接生成完整片段。

**方式一：systemd user timer**

1. 创建 `~/.config/systemd/user/delegate-radar-<repo>.service`：
```ini
[Unit]
Description=Delegate Radar scout for <repo>

[Service]
Type=oneshot
WorkingDirectory=/path/to/repo
ExecStart=/path/to/delegate radar
```

2. 创建 `~/.config/systemd/user/delegate-radar-<repo>.timer`：
```ini
[Unit]
Description=Run Delegate Radar every 2h between 00:30 and 04:30

[Timer]
OnCalendar=*-*-* 00,02,04:30:00
Persistent=true

[Install]
WantedBy=timers.target
```

3. 手动启用：`systemctl --user daemon-reload && systemctl --user enable --now delegate-radar-<repo>.timer`。

**方式二：crontab**

运行 `crontab -e` 添加：
```cron
30 0,2,4 * * * cd /path/to/repo && /path/to/delegate radar >> ~/.local/state/delegate/radar/<repo>/cron.log 2>&1
```
