## L0

### 1. 落点
- [crates/delegate/src/main.rs](crates/delegate/src/main.rs) :: `apply` (L2785-2797) — 移除 `not_truncated` 对合入后全清的阻塞。
- [crates/delegate/src/main.rs](crates/delegate/src/main.rs) :: `clean` (L1373-1389) — `clean --finished` 处 `.truncated` 阻塞清理的条件与保留提示。
- [crates/delegate/src/gc.rs](crates/delegate/src/gc.rs) :: `worker_command` (L297-371) — `_gc-worker` 主循环：接入已合入/已 drop 立即回收及整盘水位触发的未合入终态 LRU 回收。
- [crates/delegate/src/gc.rs](crates/delegate/src/gc.rs) :: `auto_clean_committed_applied_runs` (L373-424) — 移除 L382 `.truncated` 阻塞，扩展识别已合入（含未写 `.applied-cleaned` 但已进 HEAD）与已 drop 的 run。
- [crates/delegate/src/runs.rs](crates/delegate/src/runs.rs) :: `prune` (L1467-1512) — 移除 L1481 `.truncated` 对自动过期清理的阻塞。
- [crates/delegate/src/work.rs](crates/delegate/src/work.rs) :: `drop_cmd` (L8137-8238) — `work drop` 写入 `closed_kind = "dropped"` 后目前未触发 `gc::spawn_worker()`。
- [crates/delegate/src/disk.rs](crates/delegate/src/disk.rs) :: `run_last_progress_epoch` / `probe_all_busy` (L2222-2240, L1365-1385) — 供 `gc.rs` 复用整盘水位、在用三信号检测与最久未动时间戳（`run_last_progress_epoch` 当前为私有 `fn`）。
- [skills/delegate/SKILL.md](skills/delegate/SKILL.md) :: 全文 (L1-82) — 首屏瘦身至 5 条命令 + 3 句规则 + 按需索引，保留 `git prune` 禁令（L80）与 frontmatter（L1-11）。
- [skills/delegate/references/commands.md](skills/delegate/references/commands.md) :: 全文 (L1-51) — 承接 `SKILL.md` 外移的高级子命令与 `reply` 选项细节。
- [skills/delegate/references/model-routing.md](skills/delegate/references/model-routing.md) :: 全文 (L1-23) — 承接 `SKILL.md` 外移的档位、thinking 优先级与适配卡细节。
- [tests/test_delegate.py](tests/test_delegate.py) :: `test_clean_keeps_answers_shown_truncated_until_read_in_full` (L1967-1979) — 既有测试断言 `.truncated` 阻塞 `clean --finished` 与 `prune()`，需同步调整；另新增 `tests/test_delegate_auto_gc.py`。
- [CHANGELOG.md](CHANGELOG.md) :: `## Unreleased` (L3-9) — 追加一条 `- \`delegate\`：...` 改动记录。

### 2. 跨文件关系与测试命令
- 触发链：`main.rs::clean` (L1544) 与 `main.rs::apply` (L2859) 调 `gc::spawn_worker()` 起后台 `_gc-worker` → `gc::worker_command` 持 `<state>/.gc-worker.lock` → 排空 trash jobs → 调 `auto_clean_committed_applied_runs` → `runs::auto_clean_applied_run` 调 `archive::preserve` 后 `gc::stage_clean_to_trash` → 再次排空 jobs → 刷新 `index` 与 `archive/.usage.json`。
- 数据来源：run 目录来自 `index::discover_snapshot_repos` + `runs::all_runs_in`；已合入检测用 `worktree::detect_applied` / `is_applied_committed_in_head`；已 drop 工作项来自 `work::load_readonly_in(&state)` 的 `WorkItem { state == "done", closed_kind == "dropped", runs }`；整盘水位与在用检测来自 `disk::load_config` / `disk::probe_disk_space` / `disk::probe_all_busy`。
- 相关测试命令：
  - `python3 -m unittest tests.test_delegate_633 tests.test_delegate_disk_health tests.test_delegate_auto_gc`
  - `python3 -m unittest tests.test_delegate.DelegateTests.test_clean_keeps_answers_shown_truncated_until_read_in_full`
  - `./scripts/check.sh --strict`（校验 `SKILL.md` 链接与 `.repo-governance.json` token 预算）
  - `./scripts/verify.sh`（全量门禁：`check.sh` + `scripts/run-tests.py` + `cargo clippy -- -D warnings` + `delegate-tui`）

### 3. 与说明不符之处
- `references/commands.md` 与 `references/model-routing.md` 已存在（并非新建），需合并外移内容并注意 `skills/*/references/*.md` 的 3000 token 预算。
- `.truncated` 阻塞清理有 4 处（`main.rs:2785`、`gc.rs:382`、`main.rs:1375`、`runs.rs:1481`），不止规格所列 2 处，且 `tests/test_delegate.py:1967` 显式断言旧行为。
- `scripts/check.sh` 只检查 Markdown 文件路径链接，不校验正文中的子命令名（如 `land`），写 `land` 不会导致 `check.sh` 失败。
- 补充决定已覆盖规格里的固定 TTL/容量上限，`config.rs` 无需新增 `gc.worktreeTtlDays` / `gc.worktreeMaxGib`。
- `work drop` 与 run 终态收尾（`supervise.rs`）当前均未调用 `gc::spawn_worker()`。

### 4. 未核实项
- `adopt` 命令从归档重建 worktree 的具体实现（已确认 `launch.rs:1355` 支持 `--in` 从归档重建，未查 `main.rs` 中 `adopt` 子命令入口）。
- `skills/delegate/references/cleanup.md` 与 `runtime.md` 在外移内容后是否超 3000 token 预算（需跑 `./scripts/check.sh --strict` 实测）。
- `supervise.rs` 终态收尾是否需要一并 `gc::spawn_worker()` 以驱动磁盘水位自动回收（需查 `supervise.rs::finish_run` L2185-2300）。

---

## L1

### 完整调用表与数据结构位置

#### 1. GC Worker 与清理链路
- [crates/delegate/src/gc.rs](crates/delegate/src/gc.rs) :: `stage_worktree_only_to_trash` (L94-148) — `pub fn stage_worktree_only_to_trash(run: &Path, meta: &Value) -> Res<Option<PathBuf>>`：O(1) 原子 rename 仅将 worktree 移入 trash（池槽调 `pool::release`），保留轻量 run 目录，写 `<cache>/trash/jobs/<id>.json`。
- [crates/delegate/src/gc.rs](crates/delegate/src/gc.rs) :: `stage_clean_to_trash` (L152-222) — `pub fn stage_clean_to_trash(run: &Path, meta: &Value, remove_worktree: bool) -> Res<PathBuf>`：原子移出 run 目录及非共享非池化 worktree，并直删 `.git/worktrees/<name>`。
- [crates/delegate/src/gc.rs](crates/delegate/src/gc.rs) :: `execute_job_inner` (L229-279) — 消费 job JSON：清理 compose 容器、`remote::clean`、`rm -rf` trash worktree/run、`git worktree prune`。
- [crates/delegate/src/gc.rs](crates/delegate/src/gc.rs) :: `spawn_worker` (L282-291) — `pub fn spawn_worker()`：后台拉起脱离进程组的 `delegate _gc-worker`。
- [crates/delegate/src/gc.rs](crates/delegate/src/gc.rs) :: `worker_command` (L297-371) — 单实例（`locks::try_acquire(LockKind::Worker, ".gc-worker.lock", true)`），依次排空 jobs、清孤儿容器、跑 `work::check_stale_and_expired` / `refs::backfill_all` / `history::ensure_backfilled`、调 `auto_clean_committed_applied_runs(&state)`、二次排空 jobs、按需重建 index、刷新 `archive/.usage.json`。
- [crates/delegate/src/gc.rs](crates/delegate/src/gc.rs) :: `auto_clean_committed_applied_runs` (L373-424) — 遍历 `index::discover_snapshot_repos(state, &[], None)` 与 `runs::all_runs_in(&runs_dir)`；当前仅处理同时存在 `.applied-cleaned` 与 `.applied` 的 run，且跳过 `.generate-pending` / `.truncated` (L382) / `pending_reply::status` / `active` / `agent_alive` / `!clean_on_apply` / `conflictMarkers` / `has_active_child`，命中 `is_applied_committed_in_head` 后计算 `shared` 并调 `runs::auto_clean_applied_run(run, !shared)`。

#### 2. Run 生命周期、清理与归档
- [crates/delegate/src/runs.rs](crates/delegate/src/runs.rs) :: `state` / `active` (L441-505) — `active` 判定 `"running" | "starting" | "waiting" | "scouting"`；终态由 `exit_code` + `summary.json["state"]` 决定（或无活 supervisor 判 `"crashed"`）。
- [crates/delegate/src/runs.rs](crates/delegate/src/runs.rs) :: `unmerged` (L1241-1247) — 判定 write 模式、worktree 存在且缺 `.applied` 或有 `.generate-pending`。
- [crates/delegate/src/runs.rs](crates/delegate/src/runs.rs) :: `remove_after_apply` (L1271-1368) — 轻量清理：先 `archive::preserve` 再 `gc::stage_worktree_only_to_trash`，释放池槽（将同槽非活跃 run 的 `meta.worktree.path` 改为 `<path>#released`），写 `.applied-cleaned`。
- [crates/delegate/src/runs.rs](crates/delegate/src/runs.rs) :: `auto_clean_applied_run` (L1373-1417) — `pub fn auto_clean_applied_run(run: &Path, remove_worktree: bool) -> Res<Option<PathBuf>>`：检查非活跃 → `archive::preserve` → `work::on_run_cleaned` → `refs::delete_run` → `cleanup::reclaim_on_clean(run, false)` → 释放池槽引用 → `gc::stage_clean_to_trash` → `history::record_clean` → `index::remove_run` → `board::refresh_for_meta` → `events::emit("cleaned", ...)`。
- [crates/delegate/src/runs.rs](crates/delegate/src/runs.rs) :: `prune` (L1467-1512) — 由 `supervise.rs:1514` 启动阶段调用，按 `KEEP_DAYS`（默认 7）清理 `.delivered && !pending_reply && !.truncated && exit_code && !unmerged` 的 run。
- [crates/delegate/src/archive.rs](crates/delegate/src/archive.rs) :: `preserve` (L351-534) — 无损保存 `meta.json`、`summary.json`、`prompt.md`、`result.md`、`result-original.md`（L450）、`changes.json`、`changes.patch`、`applied.patch`、`applied-files.txt`、`chain-files.txt`、各轮结果文件与 `events.jsonl.gz`；失败时返回 `Err` 阻止删除 worktree。

#### 3. 整盘水位、在用检测与已合入/已 Drop 判定
- [crates/delegate/src/disk.rs](crates/delegate/src/disk.rs) :: `DiskConfig` & `DiskSpace` (L41-266) — 整盘水位配置（默认 `high_pct = 85`、`min_free_gib = 50`、`target_pct = 70`）及判定方法：`primary_is_high(&sp)` (L113)、`primary_bytes_to_free(&sp)` (L119)、`overflow_is_high(&sp)` (L138)、`overflow_bytes_to_free(&sp)` (L145)。
- [crates/delegate/src/disk.rs](crates/delegate/src/disk.rs) :: `load_config` / `probe_disk_space` (L477-483, L525-534) — 读取用户 `config.json` 的 `disk` 节；`probe_disk_space` 支持测试环境变量 `DELEGATE_DISK_PRIMARY_FAKE_SPACE` / `DELEGATE_DISK_OVERFLOW_FAKE_SPACE`（格式 `"total:avail"` 字节）覆盖 `statvfs`。
- [crates/delegate/src/disk.rs](crates/delegate/src/disk.rs) :: `ManagedTarget` & 三信号在用检测 (L1129-1394) — `probe_cargo_flock` (L1143)、`probe_proc_refs_in` (L1201，扫 `/proc/[pid]/{cwd,exe,fd}`)、`probe_lane_tickets_in` (L1292，查 `<state>/lane/*.ticket` 持锁且 `running: true` 的票)、`probe_all_busy` (L1365)、`is_slot_busy` (L1387)。
- [crates/delegate/src/disk.rs](crates/delegate/src/disk.rs) :: `run_last_progress_epoch` (L2222-2240) — 综合 `meta.startedEpoch`、`summary.json["finishedAt"]` 与 `summary.json`/`events.jsonl`/`agent.log`/`accept.log`/`exit_code` mtime 取最新活动时间戳；目前是模块私有函数。
- [crates/delegate/src/disk.rs](crates/delegate/src/disk.rs) :: `prune_with` (L2523-2720) — 现有整盘水位淘汰参考实现：先测盘空间 → 发现目标 + `probe_all_busy` → L0 清理 → 若 `primary_bytes_to_free > 0` 按 LRU 淘汰至目标水位。
- [crates/delegate/src/worktree.rs](crates/delegate/src/worktree.rs) :: `clean_on_apply` (L102)、`is_applied_in_repo_readonly` (L964-1012)、`is_applied_committed_in_head` (L1017-1080)、`detect_applied` (L1084-1120) — 写路径调用 `detect_applied` 可补写 `.applied`；`is_applied_committed_in_head` 严格校验 HEAD 已前进且改动条目全部匹配 HEAD。
- [crates/delegate/src/work.rs](crates/delegate/src/work.rs) :: `WorkItem.closed_kind` (L354)、`load_readonly_in` (L3134-3141)、`drop_cmd` (L8137-8238) — `work drop <Wxx> --reason "..."` 将 `state` 置 `"done"`、`closed_kind` 置 `"dropped"`，关联 run ID 存在 `WorkItem.runs`。

#### 4. `SKILL.md`、References 与校验门禁
- [skills/delegate/SKILL.md](skills/delegate/SKILL.md) (L1-82) — frontmatter (L1-11，含 `metadata.version: "6.52.0"` 不改动)；L29-46 大表、L54-67 档位/适配卡、L70/76/79 为瘦身外移/删减对象；L80 `git prune` 禁令按补充决定暂留。
- [skills/delegate/references/commands.md](skills/delegate/references/commands.md) (L1-51) & [skills/delegate/references/model-routing.md](skills/delegate/references/model-routing.md) (L1-23) — 已存在的承接文档。
- [skills/delegate/references/runtime.md](skills/delegate/references/runtime.md) (L1-48) & [skills/delegate/references/cleanup.md](skills/delegate/references/cleanup.md) (L1-67) — 可承接 `DELEGATE_REMOTE`、相似历史提示及自动回收说明。
- [.repo-governance.json](.repo-governance.json) (L1-9) — 预算上限：`skills/*/SKILL.md` 4000 tokens，`skills/*/references/*.md` 3000 tokens。
- [scripts/check.sh](scripts/check.sh) (L40-120, L186-265) — 校验 frontmatter 中英文触发词（`时(?:主动)?使用` 与 `use (when|to|for)`）、禁止 `./skills/`、校验所有 Markdown 相对路径链接、调 `audit-context.py` 测预算、以及测试文件 AST 门禁（禁止裸 `time.sleep`、手写 `time.time()` 轮询、`os.kill(pid, 0)`、紧墙钟断言）。

### 可参照的同类实现
- **安全回收与共享 worktree 保护**：`gc.rs::auto_clean_committed_applied_runs` (L373-424) 与 `main.rs::apply` (L2760-2860) 的 `wt_is_shared_active` 逻辑——回收 worktree 前必须检查同仓其余 run 是否指向同一 `worktree.path` 且仍活跃/未合入，以及是否有未合入的子 run（`parent == run_id`）。
- **整盘水位驱动的 LRU 淘汰**：`disk.rs::prune_with` (L2523-2618) 展示了用 `DiskConfig::primary_bytes_to_free` 计算待释放字节数、跳过 `busy` 目标、并在每释放一项后 `sp.after_freed(freed)` 递减直到脱离高水位的完整模式。
- **只读 `--in` 从归档重建**：`launch.rs::reconstruct_in_snapshot` (L1355-1550) 展示了 `archive::preserve` 归档后仍能通过 `applied.patch` / `changes.patch` 重建快照的契约。

### 测试夹具的现成写法
- **同步触发 `_gc-worker`**：[tests/test_delegate_633.py](tests/test_delegate_633.py) :: `Delegate633Tests.run_gc_worker_sync` (L48-58) — 先对 `<state>/.gc-worker.lock` 做阻塞 `fcntl.flock` 等前序后台 worker 退出，再同步跑 `self.cli("_gc-worker", cwd=repo)`。
- **模拟整盘水位**：[tests/test_delegate_disk_health.py](tests/test_delegate_disk_health.py) (L19-80) — 复用 `fixtures.DelegateTests` 的 `setUp`/`fake_pi`/`cli`/`outcome`/`repo`，写 `XDG_CONFIG_HOME/delegate/config.json` 的 `disk` 节，并通过环境变量 `DELEGATE_DISK_PRIMARY_FAKE_SPACE="<total>:<avail>"` 确定性触发高水位/低水位。
- **触发 `.truncated` 场景**：[tests/test_delegate.py](tests/test_delegate.py) (L1967-1979) — 设 `self.env["DELEGATE_RESULT_CHARS"] = "10"` 配合长答复 `answer("x" * 50)` 生成 `.truncated` 标记。

### 看过但无关的文件
- [crates/delegate/src/cleanup.rs](crates/delegate/src/cleanup.rs) — 负责进程组、systemd scope 与 Docker compose 容器回收，被 `gc.rs`/`runs.rs` 直接复用，无需改动。
- [crates/delegate/src/config.rs](crates/delegate/src/config.rs) — 已将 `disk` 校验委托给 `disk::validate_config` (L74)，且按补充决定不新增 `gc.*` 配置项，无需改动。
- [tests/test_delegate_622.py](tests/test_delegate_622.py) — L167-172 仅测 `wait --any` 写 `.truncated` 与 `result` 清 `.truncated`，不涉及清理阻塞，无需改动。
- [scripts/verify.sh](scripts/verify.sh) — 门禁驱动脚本，只读确认验证流程，无需改动。
