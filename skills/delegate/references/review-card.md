# 合入审查卡（渐进式披露）与分层并行验收

合入仍由主控把关（不做 autoApply）；本能力通过 L0–L3 渐进式披露与分层并行验收让审查更快、更准。

## 渐进式披露（L0 → L3）

- **L0（一行）**：出现在 `status`、`wait` 与 `events` 交付通知中，由宿主从 `summary.json["card"]` 零 git 直读：包含验收结果（含跑/跳层）、审查结论、改动文件与增删行数、风险项计数（如 `风险 3：模型可见文字 2、迁移/Schema 1`）、快进状态（`快进 —`，预留 6.35 接线）与下一步命令 `delegate card <run>`。
- **L1（一屏审查卡）**：`delegate card [run] [--json] [--round]`
  1. **做了什么**：宿主截取 `result.md` 前 3 行并附原文路径与总行/字数（不修改原文），另附审查员一句结论与 `verdictFile` 路径。
  2. **风险项**：按严重度（严重 > 高 > 中 > 低）排序，逐条给出 `文件:行`、规则/来源与一句原因（只指出该看哪里，不做裁决；无则写 `无`）；并与审查员 `## 主控重点看` 清单按同 hunk / ±5 行去重合并（标明 `规则`、`审查员` 或 `规则+审查员`）。
  3. **测试与验收**：确定性统计新增/修改/删除测试数，以及验收各层执行结果与跳过/复用/兜底原因。
  4. **其余改动**：未命中风险项的文件按目录折叠成一行一组（目录 + 文件数 + `+A/-D`）。
  5. **下一步**：`delegate diff <run> --risky`、`delegate diff <run> --total`、`delegate apply <run>`（需签字时附 `delegate sign <run>`）。
- **L2（仅看风险 hunk）**：`delegate diff <run> --risky [--round] [-- paths...]` 只输出命中规则或审查员提示的 hunk，前后各带 5 行上下文及 `# 风险命中:` 标注。
- **L3（完整 diff）**：`delegate diff <run> [--total]` 保持原样。

## 风险规则（内置 + `.delegate.json` `riskRules`）

内置 9 类确定性规则（均由宿主基于 diff 计算）：
1. `model_visible`（模型可见文字，高）：沿用 `controllerReviewPaths`。
2. `migration_schema`（迁移/Schema，高）：`migrations/**`、`*.sql`、`schema.*`、`*.proto`、`*.graphql` 或 SQL DDL。
3. `weakened_test`（测试减弱，高）：删除测试文件/测试定义、净删减断言或新增 `#[ignore]`/`.skip`/`@pytest.mark.skip`。
4. `dependency`（依赖/Lockfile，中）：lockfile 变更或清单文件新增依赖。
5. `protected_path`（受保护路径，高）：命中 `protect` / `defaults.protect`（含 `--allow` 放开项）。
6. `config_constant`（配置/常量，中）：数字常量、配置限额或超时默认值变更。
7. `concurrency_unsafe`（锁/并发/unsafe，高/中）：`unsafe`、`Mutex`/`RwLock`/`flock`/原子或线程原语。
8. `secret_internal`（敏感信息/内网，严重）：私钥、常见 Token/硬编码凭据、内网 IP 或内网主机名。
9. `large_deletion`（大段删除，中）：单 hunk 删除 ≥40 行且净删明显，或整文件删除。

仓库可在 `.delegate.json` 配 `riskRules` 定制：

```json
{
  "riskRules": {
    "disable": ["config_constant"],
    "largeDeletionLines": 50,
    "extra": [
      {
        "id": "sandbox_policy",
        "label": "沙箱策略",
        "severity": "high",
        "paths": ["crates/sandbox/**"],
        "content": ["seccomp", "cgroup"],
        "reason": "涉及沙箱隔离策略改动"
      }
    ]
  }
}
```

## 分层验收（`acceptRules`）与并行审查

`.delegate.json` 支持可选 `acceptRules`（按 glob 匹配改动文件，支持 `!` 排除），与单条 `accept` 向后兼容：

```json
{
  "accept": "full-check-cmd",
  "acceptRules": [
    {"name": "rust-fast", "tier": "fast", "paths": ["crates/**/*.rs"], "command": "cargo fmt --all -- --check"},
    {"name": "rust-slow", "tier": "slow", "paths": ["crates/**/*.rs"], "command": "cargo clippy --workspace --all-targets -- -D warnings"},
    {"name": "web", "tier": "fast", "paths": ["apps/web/**"], "command": "pnpm check:web"}
  ]
}
```

- **Fail-closed 全量兜底**：若改动为空/未知、有任意改动文件未命中任何规则、或命中构建配置（`Cargo.*`、`xtask/**`、`package.json`、各类 lockfile、`tsconfig*.json`、`Makefile`、`.delegate.json` 等），自动回退跑全量 `accept`（未设 `accept` 则跑全部 `acceptRules`）。
- **快层 → 慢层与审查并行**：先串行跑命中的 `fast` 层（失败直接 `rejected`，不起慢层与审查员）；`fast` 通过后，命中的 `slow` 层与独立只读审查员（在隔离 `review-ro` 目录）并行执行。
- **任一失败即互相取消**：慢层失败立停审查员并标为 `cancelled`（绝不记成 `PASS`，状态为 `rejected`）；审查员判 `REWORK` 立停慢层进入返工；仅两者都通过才进入 `delivered`。
- **返工增量重跑**：返工后仅重跑本轮增量改动命中的层与尚未通过的层；未受影响且已通过的层标记 `reused` 直接复用并在 `accept.layers` 与审查卡里注明原因。
