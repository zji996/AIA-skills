# 用途、模型与思维长度

- `models` 别名可为 `"provider/id"` 或 `{"model":"provider/id","thinking":"medium","cost":"main"}`（`thinking`、`cost` 均可省略，`cost` 取值 `low|main|high`）；字符串写法不变，tier 引用别名也带默认思考档。
- `models` 别名表、`tiers` 用途表均按键合并，仓库覆盖用户同名键。模型全集**只来自 models**；Pi 列表可能有没凭据的内置模型，绝不作为可选来源。`--model` 精确别名或已登记完整名优先，否则仅在别名与其完整名中做不区分大小写的子串匹配；按完整模型去重，唯一才用，歧义列候选，未登记提示 `delegate tiers --alias name=provider/id`。Pi 列表只校验登记模型存在与读取 context/images/thinking。
- `tiers.scout`（只读侦察/摘要/文案）、`tiers.review`（只读严密审查/根因/第二意见）、`tiers.write`（改代码带验收）都必须可解析，否则 start/run/reply 退出 2 并给配置引导。不兼容旧档名：把 cheap/strong/default 改为三用途档，将模型放进 models，`:high` 等后缀拆到 thinking。`--model` 可与 `--tier` 同用：用途决定模式，model 固定模型；scout/review 自动只读。未给二者时写入推断 write、只读推断 review，stderr 明确提醒，meta `tierInferred: true`，status 标「推断」。普通 reply 沿用原模型与思维长度；带 `--tier`/`--model` 可换模型，`--thinking` 可独立改长度。
- 档位可为别名/已登记完整名，或规则列表：`"write": [{"model":"sol","days":"mon-fri","hours":"09:00-12:00,14:00-18:00","thinking":"high"},"ds"]`。按调用时本机星期/时间取首条命中；`days` 是 mon…sun 的列表/区间，`hours` 是逗号分隔 HH:MM-HH:MM（结束不含，可跨午夜）。`maxActive` 为正整数：启动锁内按解析后的完整模型统计本机活动任务，达到上限默认排队等原模型；仅条目显式 `"overflow":"next"` 时跳到下一候选，`--overflow next|queue` 覆盖本次选择，未知值在加载时报错。排队及等待上游的任务不计数。条件需全部命中，末项是无条件模型字符串；中间条目至少带一个条件或 thinking。启动与显式换档续做重新取值，普通续做不重选；瞬时失败自动换本档列表里当前模型之后第一个不同模型（每 run 最多一次），没有则不换；模型与思维长度候选记入 meta `tierCandidates`。同一完整模型在多个档声明上限时取最小值，`--model`、环境指定模型与普通 reply 也计数并受限，不因别名、用途或时段绕过该模型的上限。
- 思维长度独立字段：`DELEGATE_THINKING` > CLI `--thinking` > `--class`（`build`/`review` → `medium`，`hard` → `high`，启动回显 `thinking <档>〔类别 <类>〕`；未传 `--class` 保持原规则）> 规则条目 `thinking` > 别名默认 `thinking` > 顶层 `thinking` > 不传（Pi 默认），级别为 off/minimal/low/medium/high/xhigh。`DELEGATE_SCOUT_MODEL` / `DELEGATE_REVIEW_MODEL` / `DELEGATE_WRITE_MODEL` 覆盖对应整个档位，仍必须是已登记模型；模型名里的 thinking 后缀报错，CLI 用 `--thinking`，配置用 thinking 字段。
- `delegate tiers` 列各档规则（别名 → 完整名、thinking）和已登记别名的价位（`cost`，未设显示 `—`）及 context/images/thinking 能力，末行只写 Pi 中还有多少模型未登记，不列出它们。设置用 `--alias name=provider/id`（可重复）、`--scout/--review/--write <别名或已登记完整名>`、`--thinking <级别>`，校验后原子写入用户配置；`probe` 仅探测登记模型，按完整名去重。
- `--image` 从不影响选模型：最终模型在 Pi 列表里 images=no 时退出 2，提示在 Pi models.json 给该模型 input 加 image，或用 `--model` 换能看图的模型；Pi 列表为空时跳过此守卫。

用户级与仓库级配置示例见 [configuration.md](configuration.md)。

## 模型适配卡（fit）与价位

- 三类固定：`build` 按规格实现（后端/测试/批量化，含照主控设计稿做界面——设计稿与观感收尾归主控不委派）；`hard` 疑难改动（内核账本/并发时序/沙箱安全/迁移）；`review` 只读审查（代码/方案/风险审查）。写任务默认先只读探路出地图（极轻任务与 --no-scout 例外），再新会话派写手；地图与说明不符时先停下问主控。主控在说明里给齐要改的文件、符号与已知事实。
- 两步探路默认走 `tiers.scout` 档，Host 零模型调用校验 L0 落点与 blob sha 后将地图存为写任务 `scout-map.md` 再开新会话派写手（缺 L0 或坏锚点超 20% 弃图记 `scout_map_dropped` 不阻塞写入），可用 `--no-scout` 或 `.delegate.json` `"scoutFirst": false` 关闭。
- 首次遇到同类任务可把同份规格各派一路对比 → `delegate fit set <类别> <别名> first|ok|avoid "一句话" [--sample <规格文件>]` 记结论（每格仅留最新一句，上限 80 字符；设 `first` 时同类别原 `first` 自动降为 `ok`）。记录存放在用户配置目录的 `fit.json`（旧 `scout`/`design` 记录读取时忽略，下次写回时丢弃）。
- `delegate fit` 以表格展示三类质量与每个别名的价位（`alias〔cost〕`，未设显示 `—`）及未评项；`delegate fit --json` 输出完整 note、sample 与 `cost` 映射。配置新增别名会列为未评，用各类别记录的 sample 重跑即可。
- `start`/`reply` 启动回显在模型旁显示已配价位（如 `main〔main〕`），仅提示，不自动换模型、不拦截。

## agy 同事

别名映射到 `agy/<agy 模型 id>`（如 `{"neon":{"model":"agy/gemini-4-neon","thinking":"high"}}`，id 见 `agy models`）的任务由 Antigravity CLI 执行，其余仍是 Pi。和 Pi 一样全权限（`--dangerously-skip-permissions`，只读任务照旧在快照工作树里）；提示经 stdin 以 stream-json 传入，固定附言放在首条消息开头；`--thinking` 映射为 `--effort`，off/minimal 取 low（每次调用都思考）。reply 用 `--conversation` 续接同一会话；换到另一个同事（Pi↔agy）时开新会话。agy 的命令是它自己的子进程，进程范围、按 pid 结束和 bash 守卫不变；`probe` 也走 agy。
