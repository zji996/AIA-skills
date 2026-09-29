---
name: openai-image-gen
description: 需要生成配图、Banner、图标、架构总览、产品插图或 UI/游戏素材并保存为本地文件时使用；先用便宜模型打草稿，确认构图后出一次正式图，按输出文件限定次数控制费用，只输出一行结果 JSON。Use to generate image assets, diagrams, illustrations, banners or icons to disk.
license: MIT
metadata:
  version: "2.0.0"
---

# OpenAI Image Generation (本地图像生成)

```bash
G=<本技能目录>/scripts/imagegen.py
$G draft -o docs/img/arch.webp -f arch.prompt.txt -s 2560x1440   # 草稿：flare · low · 长边 1536
$G final -o docs/img/arch.webp --keep-prompt                      # 正式图：sunburst · high，沿用草稿构图
$G edit  -o docs/img/arch.webp -f fix.txt                         # 局部修正
$G status -o docs/img/arch.webp
```

每次调用都收费。草稿用快速模型、low 质量和小尺寸，比正式图便宜得多，所以构图靠草稿来定，正式图只出一次。

## 流程

1. **写提示词**：写完整后自查一遍再调用，写法见 [references/prompting.md](references/prompting.md)。长提示词放文件，`-f -` 读 stdin。
2. **草稿**：只看构图、区块、箭头方向和文字位置。草稿里的字模糊或拼错是正常的。构图不对就改提示词，再出一张草稿。
3. **正式图**：默认以最新一张草稿作构图参考，并自动追加“保持构图、全质量渲染、文字照提示词”的指令。`--draft N` 选指定草稿；草稿里某个标签写错了，用 `-f` 传修正后的提示词；`--fresh` 不参考草稿，从头生成。
4. **审查**：裁剪放大后逐字核对文字，并核对每个箭头的起点和终点。
5. **编辑**：只修局部缺陷，提示词写清改什么，并注明其余保持不变。编辑会重绘整张图；结果不如原图时，用 JSON 里 `previous` 指向的原图恢复。
6. **交付**：给出文件路径和剩余瑕疵。用户说“差不多就行”时就停下。

## 预算与输出

- **次数上限**：每个输出路径最多草稿 3 张、正式图 1 张、编辑 1 次，只计成功的调用。
- **超出上限**：退出码为 3，输出 `"status":"refused"`，不会请求接口。只有用户明确同意多花钱，才加 `--allow-extra`。
- **`reset`**：只在同一路径要做一张新图时使用，不能用来绕过上限。
- **结果**：一行 JSON，包含 `file`、`budget`，以及给出下一步的 `next`。草稿和状态存放在 `$XDG_STATE_HOME/openai-image-gen/` 下，不进项目目录。
- **失败**：退出码为 1，错误写在 stderr，已有的目标文件保持不动，也不计次数。

## 要点

- **尺寸**：
  - 图标 `1024x1024`；
  - Banner `1536x1024`；
  - 海报 `1024x1536`；
  - 架构总览 `2560x1440`。
  - 两边都须是 16 的倍数，长短边比不超过 3:1，最大 `3840x2160`。
- **格式**：由扩展名决定，`.webp`/`.jpg` 可配 `--compression`。文档配图用 webp。
- **质量**：
  - 正式图用默认的 `high` 即可；`-q xhigh`/`max` 只在 `high` 确实达不到要求时用。
  - 正式图耗时 40–60 秒，给调用设的超时不低于 300 秒。
- **图不适合的场景**：精确的数据图、需要逐条核对的结构图，用 SVG 或 HTML 画；生成图适合做总览和题图。
- **凭据**，依次尝试：
  1. `-k`/`-b` 参数；
  2. `OPENAI_API_KEY`/`OPENAI_BASE_URL`；
  3. Codex 当前选中的 provider，它的 key 只发往它自己的地址；
  4. Codex `auth.json`。

  只依赖 Python 3.11+ 标准库，其余参数见 `--help`。
