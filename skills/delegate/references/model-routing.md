# 用途、模型与思维长度

- `models` 别名可为 `"provider/id"` 或 `{"model":"provider/id","thinking":"medium"}`（默认档可省略）；字符串写法不变，tier 引用别名也带默认思考档。
- `models` 别名表、`tiers` 用途表均按键合并，仓库覆盖用户同名键。模型全集**只来自 models**；Pi 列表可能有没凭据的内置模型，绝不作为可选来源。`--model` 精确别名或已登记完整名优先，否则仅在别名与其完整名中做不区分大小写的子串匹配；按完整模型去重，唯一才用，歧义列候选，未登记提示 `delegate tiers --alias name=provider/id`。Pi 列表只校验登记模型存在与读取 context/images/thinking。
- `tiers.scout`（只读侦察/摘要/文案）、`tiers.review`（只读严密审查/根因/第二意见）、`tiers.write`（改代码带验收）都必须可解析，否则 start/run/reply 退出 2 并给配置引导。不兼容旧档名：把 cheap/strong/default 改为三用途档，将模型放进 models，`:high` 等后缀拆到 thinking。`--model` 可与 `--tier` 同用：用途决定模式，model 固定模型；scout/review 自动只读。未给二者时写入推断 write、只读推断 review，stderr 明确提醒，meta `tierInferred: true`，status 标「推断」。普通 reply 沿用原模型与思维长度；带 `--tier`/`--model` 可换模型，`--thinking` 可独立改长度。
- 档位可为别名/已登记完整名，或规则列表：`"write": [{"model":"sol","days":"mon-fri","hours":"09:00-12:00,14:00-18:00","thinking":"high"},"ds"]`。按调用时本机星期/时间取首条命中；`days` 是 mon…sun 的列表/区间，`hours` 是逗号分隔 HH:MM-HH:MM（结束不含，可跨午夜）。`maxActive` 为正整数：启动锁内按解析后的完整模型统计本机活动任务，达到上限默认排队等原模型；仅条目显式 `"overflow":"next"` 时跳到下一候选，`--overflow next|queue` 覆盖本次选择，未知值在加载时报错。排队及等待上游的任务不计数。条件需全部命中，末项是无条件模型字符串；中间条目至少带一个条件或 thinking。启动与显式换档续做重新取值，普通续做不重选；瞬时失败自动换本档列表里当前模型之后第一个不同模型（每 run 最多一次），没有则不换；模型与思维长度候选记入 meta `tierCandidates`。同一完整模型在多个档声明上限时取最小值，`--model`、环境指定模型与普通 reply 也计数并受限，不因别名、用途或时段绕过该模型的上限。
- 思维长度独立字段：`DELEGATE_THINKING` > CLI `--thinking` > 规则条目 `thinking` > 别名默认 `thinking` > 顶层 `thinking` > 不传（Pi 默认），级别为 off/minimal/low/medium/high/xhigh。`DELEGATE_SCOUT_MODEL` / `DELEGATE_REVIEW_MODEL` / `DELEGATE_WRITE_MODEL` 覆盖对应整个档位，仍必须是已登记模型；模型名里的 thinking 后缀报错，CLI 用 `--thinking`，配置用 thinking 字段。
- `delegate tiers` 列各档规则（别名 → 完整名、thinking）和已登记别名的 context/images/thinking 能力，末行只写 Pi 中还有多少模型未登记，不列出它们。设置用 `--alias name=provider/id`（可重复）、`--scout/--review/--write <别名或已登记完整名>`、`--thinking <级别>`，校验后原子写入用户配置；`probe` 仅探测登记模型，按完整名去重。
- `--image` 从不影响选模型：最终模型在 Pi 列表里 images=no 时退出 2，提示在 Pi models.json 给该模型 input 加 image，或用 `--model` 换能看图的模型；Pi 列表为空时跳过此守卫。

用户级与仓库级配置示例见 [configuration.md](configuration.md)。
