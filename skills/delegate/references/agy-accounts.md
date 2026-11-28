# agy 多账号管理与从 agy-swap 迁移

`delegate` 内置本地 Antigravity CLI（`agy`）多账号管理，替代闭源且带自更新的 `agy-swap`：账号槽位包含全部实号外加 1 个 `auto` 档（默认固定当前号，`use auto` 进入新会话自动选号）、全程文件锁、不调任何 Google 内部 HTTP 接口（真实额度仅经官方 `agy -p /usage </dev/null` 获取）、永不打印令牌。

## 目录布局与权限

配置根目录遵循 `${XDG_CONFIG_HOME:-$HOME/.config}/delegate/agy-accounts/`（目录 `0700`，元数据与凭据文件 `0600`，写入均用 `O_CREAT|O_EXCL` 临时文件 `0600` → `sync_data` → `rename`）：

- `accounts.json`（`0600`）：槽位索引、实盘活跃槽位（`active`）、选号模式（`mode`：`fixed` 固定号或 `auto` 自动选号，旧文件缺省该字段视为 `fixed`）、各账号最后使用/回存时间（仅存邮箱等元数据，不存令牌）。
- `.lock`（`0600`）：所有写操作持排他 `flock` 串行化。
- `<id>/`（`0700`）：只存 `agy` 本身读写的凭据文件（存在则保存，均 `0600`，最少需要 `antigravity-cli` 令牌；新版 `agy` 首次登录通常只写 `antigravity-cli-oauth-token` 与 `credential-user-scope` 2 份即可正常工作）：
  1. `oauth_creds.json` ↔ `~/.gemini/oauth_creds.json`
  2. `google_accounts.json` ↔ `~/.gemini/google_accounts.json`
  3. `jetski-standalone-oauth-token` ↔ `~/.gemini/jetski-standalone-oauth-token`
  4. `antigravity-cli-oauth-token` ↔ `~/.gemini/antigravity-cli/antigravity-oauth-token`
  5. `antigravity-oauth-token` ↔ `~/.gemini/antigravity/antigravity-oauth-token`
  6. `credential-user-scope` ↔ `~/.config/antigravity/credential-user-scope`

## CLI 与 TUI 用法

| 命令 | 作用 |
|---|---|
| `delegate agy account list [--quota\|--quota-if-stale] [--json]` | 列出全部实号槽位与 `auto` 档、当前选中标记（`*`）、邮箱、文件数、近 5h/7d 用量与真实额度（`--quota` 强制刷新；`--quota-if-stale` 按 ≥15 分钟单飞节流刷新） |
| `delegate agy account status [--quota\|--quota-if-stale] [--json]` | 显示当前选中项（固定号或 `auto`）、实盘槽位与邮箱（含漂移）、各池当前指针与档位、近 1h/5h/7d 用量、真实额度与年龄、手动快照、在跑 `agy` 进程与本地令牌到期时间 |
| `delegate agy account pick [--model <别名>] [--json]` | 只读干跑（`auto` 与固定号模式均可用）：不建任务、不查询额度（只读缓存），打印此刻新会话会选哪个号、各号各池档位与有效 5h/7d、缓存时刻与理由（固定号模式直接报固定号） |
| `delegate agy account add [<id>] [--label TEXT] [--overwrite]`<br>别名：`delegate agy login [<id>]` | 在临时 `0700` HOME 中交互启动 `agy` 登录；登录期间检测到令牌文件落盘后会在终端醒目提示一次「已登录，按两次 Ctrl+D 退出 agy 即保存」，退出后把存在的凭据收进新槽位并销毁临时 HOME，打印槽位名、识别出的邮箱、文件数及「其他机器跑 `lan-agent-sync`」；不影响当前活跃账号（`auto` 为保留名不可用作槽位 ID） |
| `delegate agy account use <id\|auto> [--force] [--json]` | 选 `auto` 时进入新会话自动选号且不改动实盘 `~/.gemini`（实盘保持选 `auto` 之前的号）；选 `<id>` 时排他锁内回存当前实盘账号被 `agy` 刷新过的最新凭据 → 原子替换为目标槽位、切回固定号模式并同步把各额度池指针设为 `<id>`；有在跑 `agy` 时也允许切换（在跑进程与已开始的会话继续用原账号，新 run 与 `--fresh` 使用新选择） |
| `delegate agy account note <id> [--weekly PCT] [--five-hour PCT] [--clear] [--json]` | 手动登记「周剩余 %」「五小时剩余 %」备注快照（仅作展示记录，不参与额度耗尽预测） |
| `delegate agy account remove <id> [--force]` | 删除槽位（不碰实盘 `~/.gemini`；删除活跃槽位需 `--force`） |
| `delegate agy account import-agy-swap [--dry-run] [--source DIR] [--overwrite]` | 从 `~/.gemini/agy-swap/vault.json` 与 `accounts.json` 导入槽位（支持 `go-keyring-base64:` 与纯 JSON），绝不打印令牌，不改原目录 |
| `delegate agy account restore-entry [--dry-run] [--mode wrapper\|symlink]` | 备份原 `~/.local/bin/agy` 到 `~/.local/bin/agy.bak-agy-swap`，替换为直连 `~/.local/libexec/agy` 的最小 `exec` 包装（默认）或符号链接 |
| `delegate usage [--since 24h\|7d\|all] [--by account\|day\|run\|repo] [--account ID] [--repo PATH] [--all] [--json]` | 按账号槽位 / 日 / run / 仓库汇总本地 `agy` 用量（作者与审查员分列，运行中实时计入或单列「运行中未计」，绝不记邮箱） |

**真实额度、用量与 TUI**：
- **真实额度**：仅调用 `agy -p /usage </dev/null`（解析「额度池 × 窗口 → 剩余 % + 重置时刻」，未知行原样保留）。全部槽位均在独立临时 `0700` HOME 装载各自槽位凭据运行（不再在真实 `HOME` 跑，避免被常驻交互 `agy` 每小时刷新令牌写回实盘串号），校验实际身份匹配目标槽位后才落盘并写回刷新凭据（不符则拒存并标「身份不符，未更新」；两槽位条目含 `resetAt` 完全相同时在 `list`/`status` 警告）。缓存于 `<state>/agy-quota.json` 并显数据年龄；CLI 新会话与 TUI 定时刷新共用基于 `fetchedEpoch` ≥15 分钟与跨进程单飞锁（`<state>/agy-quota-refresh.lock`）的入口，仅手动 `--quota` 或 TUI 按 `Q` 强制穿透刷新。
- **用量与耗尽预测**：作者/审查员/返工每轮记录增量 tokens 与活跃槽位 ID（不记邮箱），落 `summary.json` 与 `<state>/agy-usage.jsonl` 仅用于 Token 统计与图表。耗尽预测与 `auto` 选号共用 `PoolQuotaState`，只用 `<state>/agy-quota.json` 同一 `resetAt` 周期内 `/usage` 剩余 `%` 历史（`history` 附 `fiveHourResetEp`/`weeklyResetEp`，上限 700）做近窗斜率区间与数据不足/空闲门控，结果不超过 `resetAt` 且仅对当前选用账号与池告警（`auto` 同池有绿档可接管时静默）。
- **TUI**：`0`/`O` 总览页看额度 `Gauge`、按账号 24h `Sparkline`、用途/模型 `BarChart`（`v` 切换）与近 7 天合入耗时/一轮通过率（80 列降级纯文本）；额度/账号页列出全部实号槽位及 `auto` 行（展示各池当前指针、档位与理由）与在跑进程，按 `u` 切号或选中 `auto`、`Enter` 聚焦详情、`Q` 强制刷新。

## 设计取舍：运行中可切换、会话粘滞与按进程凭据覆盖层

`use` **允许在有 `agy` 运行时直接切换**：正在跑的进程与已开始的会话继续用原账号（避免同一会话跨账号导致缓存前缀失效），新 run 与 `--fresh` 开的新会话生效新账号。实现依赖三点：

1. **会话粘滞与新会话边界**：每个 run（含其审查员会话）在第一轮启动时把所用槽位 ID 记进 `meta.json`（`agyAccount` / `reviewAgyAccount`，不记邮箱）；后续同一会话的每一轮（常规 `reply`、追加、返工、审查续审、`--sync` 续做）严格沿用钉住的槽位起覆盖层，即使缓存额度为 0% 也不静默换号续原会话（仅原槽位被删或本地凭据/身份损坏时回退活跃号）。`start`、`reply --fresh`、模型 `escalate`、探路、非续接审查与额外审查员均作为新会话选号。
2. **按进程私有凭据覆盖层（防止旧进程令牌刷新覆盖新账号）**：实测 `agy` 在 OAuth 访问令牌到期刷新时会直接截断回写 `~/.gemini/antigravity-cli/antigravity-oauth-token`。因此 `delegate` 启动每个 `agy` 进程前，在 `.lock` 内确定目标槽位（钉住槽位或新会话选定槽位）并建一个 `0700` 临时 `HOME` 覆盖层：只把该槽位中已保存的凭据文件（存在则拷贝，最少含 `antigravity-cli` 令牌）以 `0600` 独立拷贝进去，`$HOME`、`~/.gemini`（含会话 `conversations/`、`brain/`、`settings.json`、`log/`）与 `~/.config` 的其余条目全部软链回真实目录。进程退出后，在 `.lock` 内把刷新过的有效凭据写回**它自己的槽位**（仅当该账号仍为活跃且未漂移时同步回实盘 `~/.gemini`；非活跃槽位绝不碰实盘 `~/.gemini`），随后销毁临时目录；即使运行期间切了号，旧进程绝不会覆盖新账号的实盘凭据。
3. **只读探活不触发自动选号**：`probe` 探活走 `prepare_active()` 仅测当前活跃号（`AuthRefresh` 重探则测该 run 钉住的槽位），绝不触发自动选号或改池指针。

## 新会话自动选号（`auto` 档）

是否启用自动选号**只由当前选中项是否为 `auto` 决定**（存在 `accounts.json` 的 `mode` 字段，旧文件无此字段视为固定号模式；不再使用 `agy.autoSwitch.enabled`，若在配置中填写 `enabled` 将直接报错）：

- `delegate agy account use auto`：进入自动选号模式，**不改动实盘 `~/.gemini`**（实盘保持选 `auto` 之前的账号）；仅当本机存在 ≥2 个可用槽位时生效，单槽位时沿用唯一槽位。
- `delegate agy account use <id>`：切回固定号模式并把实盘切换为 `<id>`，新会话始终使用 `<id>` 不自动切（同时把各池指针同步为 `<id>`，便于下次再选 `auto` 时从此号起步）。
- TUI 额度/账号页同样多一行 `auto`，展示各模型池当前指针、档位与理由，可像普通槽位一样按 `u` 选中；`account list` / `account status` 也会列出 `auto` 行并以 `*` 标出当前选中项。

阈值、刷新间隔与并发预扣参数仅可在用户配置 `~/.config/delegate/config.json` 中调整（仓库 `.delegate.json` 出现 `agy.autoSwitch` 直接报错）：

```json
{
  "agy": {
    "autoSwitch": {
      "fiveHourMinPct": 30,
      "weeklyMinPct": 30,
      "refreshMinutes": 15,
      "perRunReservePct": 5
    }
  }
}
```

- **agy 当主控时统一用 `delegate agy account use auto`**：正在跑的 `agy` 主控进程直接使用实盘 `~/.gemini`（不走覆盖层），而 `auto` 模式下委派同事全部走进程私有覆盖层按池自动选号、绝不改动实盘 `~/.gemini`。**运行期间切勿执行 `delegate agy account use <固定号>`**：这会把实盘 `~/.gemini` 切到新号 B，但仍在运行的原主控 A 在 OAuth 令牌刷新时会把 A 的新令牌回写实盘，静默覆盖刚切好的 B。
- **不改实盘凭据，按池独立指针**：`auto` 模式下仅决定每次新会话的凭据覆盖层，绝不改动 `~/.gemini` 实盘凭据。每个模型池（`gemini` / `claude_gpt`）在 `<state>/agy-auto-switch.json` 持久化各自当前号指针（初值 = 实盘活跃号）；手动执行 `delegate agy account use <id>` 会切回固定号并把各池指针同步为 `<id>`。未知模型沿用实盘活跃号（避开 0% 槽位）且不覆写池指针。
- **三档严格升档滞回 + 并发预扣**：
  - 有效 5h = `缓存 5h − perRunReservePct × 该号该池当前在跑写手数`（防 15 分钟盲窗内多路并发挤爆余量）。
  - **绿档**（`有效 5h ≥ fiveHourMinPct && 7d ≥ weeklyMinPct`，或无有效缓存/已重置未知）> **黄档**（`5h ≥ 5% && 7d ≥ 2%` 且无被动耗尽标记）> **红档**（其余）。
  - **仅当另一健康号档位严格更高时才切换池指针**；同档（含双绿、双黄）绝不切换，避免来回乒乓；双红时仅在另一号满足 `5h > 0` 且 `min(5h, 7d)` 严格更高时切换。
  - 过期很久的数据（5h `> 2h`、7d `> 24h`，非被动标记）降级为未知以维持当前号；`resetAt` 加 60 秒时钟偏差缓冲后视作未知（不硬赋 100%）；401 登录失效与 `identityMismatch` 槽位自动剔除候选，直至凭据更新或 `/usage` 恢复。
- **15 分钟单飞刷新与被动耗尽标记**：`auto` 模式下新会话仅在当前池指针缓存过期或非绿档时通过 `<state>/agy-quota-refresh.lock` 单飞刷新过期槽位（查询失败保留旧缓存与旧 `fetchedEpoch` 并进入 5 分钟退避）。运行期收到明确额度耗尽报错（不含瞬时 `429` / RPM 限流）时，直接向缓存软标该号该池 5h 为 `0%`（`source: "passive_error"`，无已知 `resetAt` 时合成 `now + 15m`，不覆盖 `weekly` 与历史）。
- **中途耗尽兜底（按角色，仅 `auto` 模式）**：
  - **写手**（每 run 至多 1 次）：同进程内以全新会话换到另一健康号继续（交接提示包含原 `prompt.md`、已消费的 `appends.json` 追加消息、返工轮审查意见及已改文件索引，嘱先看 `git status` / `git diff` 续改，不内联大 diff；`summary.json["tokens"]` 按会话分段累计不丢前半程用量）。
  - **只读任务（探路等）**：以原 `prompt.md` 换号重跑 1 次。
  - **审查员**：不走写手接力；主审遇到明确额度耗尽且另一号非红档时，在触发 `review.fallback` 前用 `fork_target = None` 完整审查模板换号重审 1 次。
- **验证与可见性**：用 `delegate agy account pick [--model <别名>] [--json]` 只读查看当前模式下新会话选号结果与各号各池档位（固定号模式直接报固定号）；换指针写 `agy_account_auto` 事件、中途接力/重审写 `agy_account_handoff` 事件，在 `status` / 卡片 / TUI 各渲染一行（均不进 Stop hook、不提醒所有者、不打印邮箱）。

## `lan-agent-sync` 调整参考

不再推送 `agy-swap`，按存在同步上述 `agy` 凭据路径（`--ignore-missing-args`，最少含 `antigravity-cli` 令牌），并同步最小 `~/.local/bin/agy` 入口与 `~/.local/libexec/agy` 真二进制（用默认 `wrapper` 模式时 `scp` 不会把大二进制传两遍）：

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
  echo "$h: $(ssh "$h" 'bash -lc "agy --version"; ~/'"$DEL"' --version; jq -r ".model + \" \" + .toolPermission" ~/.gemini/antigravity-cli/settings.json' | tr '\n' ' ')"
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
