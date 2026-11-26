# 磁盘健康

池槽构建缓存不设单槽硬上限（不再按单槽容量淘汰热缓存），也不设硬时间保护；空间由整盘总水位、按在用判活的全局 LRU 与可选第二块盘溢出统一管理。

## 用户配置（`${XDG_CONFIG_HOME}/delegate/config.json`）

主机相关的构建根目录、溢出根与水位均只在用户级配置 `disk` 节声明（仓库 `.delegate.json` 禁止设置 `disk`）；路径支持 `~/` 展开。配置严格校验（失败关闭）：遇到 JSON 语法错、未知字段、越界或倒挂水位（如 `targetPct >= highPct`）以及仓库级 `disk` 时，`delegate disk` 与 `config::load` 报错指出文件与字段并退出 2，不静默回退默认值。

```json
{
  "disk": {
    "primaryRoot": "/path/to/build-disk/delegate-pool",
    "overflowRoot": "/path/to/overflow-disk/delegate-pool-overflow",
    "extraTargets": ["/path/to/repo/target"],
    "extraOverflowTargets": ["/path/to/overflow-disk/other-target"],
    "warnPct": 75,
    "highPct": 85,
    "minFreeGib": 50,
    "targetPct": 70,
    "migratePct": 90,
    "returnPct": 60,
    "overflowHighPct": 90,
    "overflowMinFreeGib": 100,
    "overflowTargetPct": 80,
    "overflowTargetFreeGib": 150,
    "staleRunHours": 24,
    "staleRunGib": 10
  }
}
```

- `primaryRoot` / `overflowRoot`：池槽 `target/` 软链的主构建根与溢出构建根；环境变量 `DELEGATE_POOL_TARGET_ROOT`、`DELEGATE_POOL_TARGET_OVERFLOW_ROOT` 优先于配置文件。
- `extraTargets` / `extraOverflowTargets`：除池槽外一并纳入清理的构建目录（如主仓 `target`）。
- 主盘默认水位：注意 `warnPct = 75`，高水位 `highPct = 85` 或剩余 `< minFreeGib`（默认 `50` GiB），全局 LRU 目标 `targetPct = 70`，回迁阈值 `returnPct = 60`（滞回防抖），极端冷槽迁移阈值 `migratePct = 90`。
- 溢出盘默认水位：高水位 `overflowHighPct = 90` 或剩余 `< overflowMinFreeGib`（默认 `100` GiB），全局 LRU 目标 `overflowTargetPct = 80` 且剩余 `>= overflowTargetFreeGib`（默认 `150` GiB）。
- 长期占槽提醒：某未合入写任务超过 `staleRunHours`（默认 `24` 小时）无进展且占槽超过 `staleRunGib`（默认 `10` GiB）时仅提醒主控检查 `apply`/`clean`，**绝不自动删除 run**（设 `0` 可关闭）。

## 清理、判活与溢出（`delegate disk prune` / `delegate disk status`）

- **三信号在用判活（零硬时间保护）**：按每个 `target` 独立检查：① `flock` 持有的 Cargo 锁（`.cargo-lock` / `.cargo-build-lock`）；② `/proc/*/fd|cwd|exe` 指向该 `target` 的进程；③ `<state>/lane/*.ticket` 中 `running: true` 指向该槽位/仓库的活跃票据。任一成立即视为在用并跳过，不靠文件修改时间宽限。
- **零代价清理（每次必做）**：对非在用构建根清理被取代的旧增量 session（保留最新已完成 `s-*`、`s-*-working` 与持锁会话）、同前缀旧哈希的自有测试可执行文件与自有 `.rlib`/`.rmeta`（不删多版本第三方依赖），以及已释放空闲槽位中的测试可执行文件（保留库产物供复用）。
- **整盘水位全局 LRU**：主盘超 `highPct` 或余 `< minFreeGib` 时，跨所有非在用主盘构建根按层（测试可执行文件 → `incremental` → 自有库 → 第三方/其余构建单元）与文件 `atime` 最旧优先淘汰到 `targetPct`，只删可重建字节；溢出盘超线时同样只清非在用字节。每次运行原子写入 `<state>/disk-health.json` 诊断（删除项、释放字节、前后水位与跳过原因）；`--dry-run` 仅预演不删除也不落盘，`--json` 输出结构化结果。
- **满载溢出与回迁**：主盘已在高水位且无可淘汰非在用字节、溢出盘仍有余量时，新占用的空槽 `<slot>/target` 直接软链到 `<overflowRoot>/<slot>/target`（对 Cargo 透明）；槽位释放或下次复用且主盘回落到 `<= returnPct`（默认 `60%`）时自动切回主盘并删除溢出目录。默认不热迁在用槽；仅当主盘 `>= migratePct`（默认 `90%`）且已无可淘汰非在用字节时，才将最冷的非在用空闲槽迁至溢出根。
- **可见性**：`delegate status`、`delegate-tui` 总览与 Stop 钩子只读展示主盘/溢出盘水位、溢出槽数与超过 24 小时且占槽超 10 GiB 的 run 提醒。
