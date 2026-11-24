# agy 多账号管理与从 agy-swap 迁移

`delegate` 内置本地 Antigravity CLI（`agy`）多账号管理，替代闭源且带自更新的 `agy-swap`：纯手工切换、全程文件锁、不做自动轮换、不调任何 Google 内部 HTTP 接口（真实额度仅经官方 `agy -p /usage </dev/null` 获取）、永不打印令牌。

## 目录布局与权限

配置根目录遵循 `${XDG_CONFIG_HOME:-$HOME/.config}/delegate/agy-accounts/`（目录 `0700`，元数据与凭据文件 `0600`，写入均用 `O_CREAT|O_EXCL` 临时文件 `0600` → `sync_data` → `rename`）：

- `accounts.json`（`0600`）：槽位索引、当前活跃槽位、各账号最后使用/回存时间（仅存邮箱等元数据，不存令牌）。
- `.lock`（`0600`）：所有写操作持排他 `flock` 串行化。
- `<id>/`（`0700`）：只存 `agy` 本身读写的 6 份凭据文件（存在则保存，均 `0600`）：
  1. `oauth_creds.json` ↔ `~/.gemini/oauth_creds.json`
  2. `google_accounts.json` ↔ `~/.gemini/google_accounts.json`
  3. `jetski-standalone-oauth-token` ↔ `~/.gemini/jetski-standalone-oauth-token`
  4. `antigravity-cli-oauth-token` ↔ `~/.gemini/antigravity-cli/antigravity-oauth-token`
  5. `antigravity-oauth-token` ↔ `~/.gemini/antigravity/antigravity-oauth-token`
  6. `credential-user-scope` ↔ `~/.config/antigravity/credential-user-scope`

## CLI 与 TUI 用法

| 命令 | 作用 |
|---|---|
| `delegate agy account list [--quota] [--json]` | 列出槽位、活跃标记（`*`）、邮箱、文件数、近 5h/7d 用量与真实额度（`--quota` 实时刷新） |
| `delegate agy account status [--quota] [--json]` | 显示活跃槽位、实盘邮箱（含漂移）、近 1h/5h/7d 用量、真实额度与年龄、手动快照、在跑 `agy` 进程与本地令牌到期时间 |
| `delegate agy account add [<id>] [--label TEXT] [--overwrite]` | 在临时 `0700` HOME 中交互启动 `agy` 登录，退出后把新凭据收进新槽位并销毁临时 HOME；不影响当前活跃账号 |
| `delegate agy account use <id> [--force] [--json]` | 排他锁内：校验无在跑 `agy` → 回存当前活跃账号被 `agy` 刷新过的最新凭据 → 原子替换为目标槽位（清理目标槽位不存在的残留文件如旧 `credential-user-scope`） |
| `delegate agy account note <id> [--weekly PCT] [--five-hour PCT] [--clear] [--json]` | 手动登记「周剩余 %」「五小时剩余 %」快照，结合本地用量推算「每 1% 约多少 token」与按当前速度还能跑多久（仅作提示，不自动换号） |
| `delegate agy account remove <id> [--force]` | 删除槽位（不碰实盘 `~/.gemini`；删除活跃槽位需 `--force`） |
| `delegate agy account import-agy-swap [--dry-run] [--source DIR] [--overwrite]` | 从 `~/.gemini/agy-swap/vault.json` 与 `accounts.json` 导入槽位（支持 `go-keyring-base64:` 与纯 JSON），绝不打印令牌，不改原目录 |
| `delegate agy account restore-entry [--dry-run] [--mode wrapper\|symlink]` | 备份原 `~/.local/bin/agy` 到 `~/.local/bin/agy.bak-agy-swap`，替换为直连 `~/.local/libexec/agy` 的最小 `exec` 包装（默认）或符号链接 |
| `delegate usage [--since 24h\|7d\|all] [--by account\|day\|run\|repo] [--account ID] [--repo PATH] [--all] [--json]` | 按账号槽位 / 日 / run / 仓库汇总本地 `agy` 用量（作者与审查员分列，运行中实时计入或单列「运行中未计」，绝不记邮箱） |

**真实额度、用量与 TUI**：
- **真实额度**：仅调用 `agy -p /usage </dev/null`（解析「额度池 × 窗口 → 剩余 % + 重置时刻」，未知行原样保留）。活跃且未漂移槽位在真实 `HOME` 跑；非活跃槽位在临时 `0700` HOME 跑，凭据若刷新原子写回（`0600`）槽位后删临时目录。缓存于 `<state>/agy-quota.json` 并显数据年龄；TUI 打开总览/账号页及之后每 10 分钟刷新（关闭不轮询），也可用 `--quota` 或 `Q` 手动刷新。
- **用量与推算**：作者/审查员/返工每轮记录增量 tokens 与活跃槽位 ID（不记邮箱），落 `summary.json` 与 `<state>/agy-usage.jsonl`（运行中实时写 `live-usage.json` 或标「运行中未计」）。真实额度优先于手动 `note`，按近 1h 速率推算每 1% token 与可用时长（仅提醒不自动换号）。
- **TUI**：`0`/`O` 总览页看额度 `Gauge`、按账号 24h `Sparkline`、用途/模型 `BarChart`（`v` 切换）与近 7 天合入耗时/一轮通过率（80 列降级纯文本）；`6`/`A` 账号页看详情与在跑进程，`Enter`/`u` 切换，`Q` 刷新。

## 设计取舍：有在跑 `agy` 时拒切（不做 `--next-start`）

`use` 选择**检测到同 `HOME` 有在跑 `agy` 进程时直接拒绝切换并列出 PID 与 delegate run**，而不提供 `--next-start` 延迟切换，原因：

1. **避免重新引入隐式包装钩子**：`--next-start` 需要入口脚本或 `start` 在每次启动前改写 `~/.gemini/*`，重新引入 `agy-swap` 式黑盒包装与多进程竞态。
2. **防止令牌刷新撕裂**：运行中的 `agy` 会在内存中持有旧账号并回写刷新后的令牌；只要仍有旧进程活着，延迟或热切换都可能导致 A 的刷新令牌覆盖 B 或 `credential-user-scope` 串号。
3. **失败关闭**：无进程时全量回存 + 原子替换保证磁盘凭据与 `accounts.json` 始终一致，按 `/proc/<pid>/environ` 的 `HOME` 精确隔离，不误伤其他 `HOME` 或 `add` 的临时目录。

## `lan-agent-sync` 调整参考

不再推送 `agy-swap`，补齐全部 6 份 `agy` 凭据路径，并同步最小 `~/.local/bin/agy` 入口与 `~/.local/libexec/agy` 真二进制（用默认 `wrapper` 模式时 `scp` 不会把大二进制传两遍）：

```bash
#!/usr/bin/env bash
# Push this host's agy config, agy binary and delegate binary to the other LAN hosts.
# Only settings and credentials move; conversations, memories and caches stay per host.
set -euo pipefail
HOSTS=${LAN_AGENT_HOSTS:-"lan-8-6 lan-8-12 lan-8-18 lan-8-24"}
cd ~
CONFIG=".config/antigravity .gemini/settings.json .gemini/google_accounts.json .gemini/oauth_creds.json .gemini/jetski-standalone-oauth-token .gemini/GEMINI.md .gemini/config .gemini/antigravity-cli/settings.json .gemini/antigravity-cli/antigravity-oauth-token .gemini/antigravity/antigravity-oauth-token .gemini/antigravity/user_settings.pb"
AGY=.local/bin/agy
AGY_REAL=.local/libexec/agy
DEL=.local/share/aia-skills/skills/delegate/bin/delegate
agy_sum=$(sha256sum $AGY | cut -c1-16); del_sum=$(sha256sum $DEL | cut -c1-16)
pairs="$AGY:$agy_sum $DEL:$del_sum"
[ -f "$AGY_REAL" ] && pairs="$AGY_REAL:$(sha256sum $AGY_REAL | cut -c1-16) $pairs"
for h in $HOSTS; do
  ssh "$h" "cd ~ && mkdir -p .local/share/agy-config-backup .gemini/antigravity-cli .gemini/antigravity .config && tar czf .local/share/agy-config-backup/before-sync-\$(date +%Y%m%d-%H%M%S).tgz --ignore-failed-read $CONFIG 2>/dev/null; ls -1t .local/share/agy-config-backup/*.tgz | tail -n +6 | xargs -r rm -f"
  rsync -aR --ignore-missing-args $CONFIG "$h:"
  for pair in $pairs; do
    f=${pair%%:*}; want=${pair##*:}
    have=$(ssh "$h" "sha256sum ~/$f 2>/dev/null | cut -c1-16" || true)
    if [ "$have" != "$want" ]; then
      ssh "$h" "mkdir -p ~/$(dirname "$f")" && scp -q "$f" "$h:$f.new" && ssh "$h" "chmod 755 ~/$f.new && mv -f ~/$f.new ~/$f"
    fi
  done
  echo "$h: $(ssh "$h" 'bash -lc "agy --version"; ~/.claude/skills/delegate/bin/delegate --version; jq -r ".model + \" \" + .toolPermission" ~/.gemini/antigravity-cli/settings.json' | tr '\n' ' ')"
done
```

若后续希望在所有机器上移除 `agy-swap` 残留，可在验证稳定后清理各机 `~/.local/bin/agy-swap` 与 `~/.gemini/agy-swap/`。

## 真实环境迁移步骤清单（含回退）

> **前提**：先确认本机没有在跑的 `agy` 委派或交互进程。

1. **检查在跑进程与升级二进制**：
   ```bash
   delegate agy account status    # 确认 running agy processes: 0
   ```
   （先按发版流程编译/安装包含 `6.38.0` 的 `delegate` 与 `delegate-tui`。）
2. **演练导入（只读不写盘、不打印令牌）**：
   ```bash
   delegate agy account import-agy-swap --dry-run
   ```
   核对将生成的槽位 ID、邮箱、文件数及 `[matches live ~/.gemini]` 标记。
3. **执行导入并核验**：
   ```bash
   delegate agy account import-agy-swap
   delegate agy account list
   ls -ld ~/.config/delegate/agy-accounts ~/.config/delegate/agy-accounts/*
   ```
   确认目录为 `0700`、文件为 `0600`，且活跃槽位与当前实盘一致（`~/.gemini/agy-swap/` 保持原样未删）。
4. **演练并恢复 `~/.local/bin/agy` 入口**：
   ```bash
   delegate agy account restore-entry --dry-run
   delegate agy account restore-entry            # 默认 wrapper 模式，原脚本备份到 ~/.local/bin/agy.bak-agy-swap
   agy --version                                 # 验证直连 ~/.local/libexec/agy 正常
   ```
5. **验证手动切换与 TUI**：
   ```bash
   delegate agy account use <另一个槽位id>
   delegate agy account status
   delegate agy account use <原活跃槽位id>
   delegate tui                                  # 按 6 查看账号页并核对活跃标记
   ```
6. **停用 `agy-swap` 后台进程并更新 `lan-agent-sync`**：
   - 结束仍在运行的 `agy-swap` 进程；更新 `~/.local/bin/lan-agent-sync`（补齐 `jetski-standalone-oauth-token` 与 `antigravity/antigravity-oauth-token`），跑一次 `lan-agent-sync` 把新入口和凭据同步到其余 4 台机。
   - 观察稳定 1–2 天后再删除 `~/.local/bin/agy-swap`、`~/.local/bin/agy.bak-agy-swap` 与 `~/.gemini/agy-swap/`。

### 回退步骤（任一步出现异常时）

1. 恢复原 `~/.local/bin/agy` 包装脚本：
   ```bash
   mv -f ~/.local/bin/agy.bak-agy-swap ~/.local/bin/agy
   ```
2. `~/.gemini/agy-swap/` 在整个迁移过程中未被修改，原 `agy-swap` 可立即恢复接管；如需重新同步回 4 台机，重跑原 `lan-agent-sync`。
3. 如需清理新槽位：`rm -rf ~/.config/delegate/agy-accounts`（不影响当前 `~/.gemini/*` 实盘凭据）。
