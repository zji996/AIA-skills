---
name: openai-image-gen
description: 需要生成配图、Banner、图标、产品插图或 UI/游戏素材并直接保存为本地文件时使用；调用 OpenAI Image API，只输出一行结果 JSON，不把图片数据灌入上下文。Use to generate image assets, illustrations, banners or icons to disk.
license: MIT
metadata:
  version: "1.2.0"
---

# OpenAI Image Generation (本地图像生成)

```bash
G=<本技能目录>/scripts/generate-image.sh

$G --prompt "Isometric 3D hero illustration of a SaaS analytics dashboard, soft blue palette, clean white background" \
   --output assets/hero.png

$G -p "Wide cinematic banner of glowing neural network nodes in deep blue space" \
   -o public/images/banner.png -s 1536x1024

$G -f docs/img/overview.prompt.txt -o docs/img/overview.png -s 1536x1024   # 长提示词从文件读，`-f -` 读 stdin
```

成功时输出一行 JSON：`{"status":"ok","file":...,"model":...,"size":...}`。失败时不改动已有的目标文件，也不输出成功状态。

## 出图要点

- **提示词写法**：按"主体 → 风格/媒介 → 构图与视角 → 色彩与光线 → 用途约束"的顺序写，比如"留出左侧空白放标题"或"纯色背景便于抠图"。提示词用英文写，即使用户用中文交流；只有画面里要出现的文字保留原语言，逐字写出并加引号。一两句话的提示词只适合氛围图。
- **示意图、信息图、带文字的画面**：模型会按描述摆放元素、照抄引号里的字，但不会替你补结构，所以提示词要写成一份布局说明，按段落分节：
  - `LAYOUT`：按从上到下、从左到右逐区描述，每个区域写清形状、位置、包含什么、标签是什么；箭头写明**从哪个元素出发、到哪个元素**，否则方向常被画错。
  - `STYLE`：媒介（flat vector 等）、背景与强调色的 hex 值、线宽与圆角、光线；出现光晕或雾化时写明 "flat solid background, no bloom, no haze"。
  - `TEXT RULES`：列出画面上全部文字串，逐字加引号，控制在十来个短标签以内，并写 "no other text"，否则模型会补伪文字。
  - `AVOID`：不要的元素，例如 3D、人物、电路纹理、镜头光斑。
  - 精确的数据图、需要逐条可查的结构图用 SVG/HTML 画；生成图适合做总览或题图。
- **长提示词放文件**：用 `-f <文件>` 或 heredoc 接 `-f -`，避免 shell 引号把提示词里的引号截断；把它存成输出旁的 `<名字>.prompt.txt`，便于复现和迭代。
- **尺寸按用途选**：`1024x1024` 适合图标、头像、卡片；`1536x1024` 适合 Banner、文章头图；`1024x1536` 适合海报、手机屏。
- **质量与费用**：默认 `high`。草图或批量探索可以用 `--quality low`/`medium` 省钱，定稿再用 `high`。每次调用都会产生 API 费用，先确认提示词再批量生成。
- **看结果再迭代**：生成后用图片查看工具检查，再根据问题改提示词，每次只改一两个变量。带文字的图逐字核对每个文字串，再核对箭头与归属等语义；不满意的版本先另存，避免覆盖已经可用的一版。
- **耗时**：`high` 质量、`1536x1024` 一次约 45 秒，排队时更久。给调用设的超时不要低于 300 秒。

## 参数

| 参数 | 缩写 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `--prompt` | `-p` | 与 `-f` 二选一 | 提示词 |
| `--prompt-file` | `-f` | 与 `-p` 二选一 | 从文件读提示词，`-` 表示 stdin |
| `--output` | `-o` | 必填 | 本地保存路径，目录不存在时会自动创建 |
| `--model` | `-m` | `gpt-image-2.5-sunburst` | 图片模型 |
| `--size` | `-s` | `1024x1024` | 分辨率 |
| `--quality` | `-q` | `high` | `auto`/`low`/`medium`/`high` 等，取决于模型 |
| `--base-url` | `-b` | `https://api.openai.com` | 可含末尾 `/v1` |
| `--api-key` | `-k` | 自动推断 | 会暴露在进程参数中，优先用环境变量 |

## 凭据来源

依次尝试：命令行参数 → `OPENAI_API_KEY`/`OPENAI_BASE_URL` → Codex `config.toml` 里当前选中 provider 的 `base_url` 与 Bearer key（两者需同时存在）→ Codex `auth.json` 中的 API key，发往 Codex 同样会发往的地址：选中 provider 设了 `requires_openai_auth` 时用它的 `base_url`，否则用官方 API。Codex 的 OAuth 登录态不能当作 API key 使用；只有登录态时，需要另外提供 key。依赖 `curl`、`jq`、`python3`。
