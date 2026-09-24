---
name: openai-image-gen
description: 需要生成配图、Banner、图标、产品插图或 UI/游戏素材并直接保存为本地文件时使用；调用 OpenAI Image API，只输出一行结果 JSON，不把图片数据灌入上下文。Use to generate image assets, illustrations, banners or icons to disk.
license: MIT
metadata:
  version: "1.1.0"
---

# OpenAI Image Generation (本地图像生成)

```bash
G=<本技能目录>/scripts/generate-image.sh

$G --prompt "Isometric 3D hero illustration of a SaaS analytics dashboard, soft blue palette, clean white background" \
   --output assets/hero.png

$G -p "Wide cinematic banner of glowing neural network nodes in deep blue space" \
   -o public/images/banner.png -s 1536x1024
```

成功时输出一行 JSON：`{"status":"ok","file":...,"model":...,"size":...}`。失败时不改动已有的目标文件，也不输出成功状态。

## 出图要点

- **提示词写法**：按"主体 → 风格/媒介 → 构图与视角 → 色彩与光线 → 用途约束"的顺序写，比如"留出左侧空白放标题"或"纯色背景便于抠图"。需要画面里出现的文字要逐字写出并加引号。英文提示词通常更稳定。
- **尺寸按用途选**：`1024x1024` 适合图标、头像、卡片；`1536x1024` 适合 Banner、文章头图；`1024x1536` 适合海报、手机屏。
- **质量与费用**：默认 `high`。草图或批量探索可以用 `--quality low`/`medium` 省钱，定稿再用 `high`。每次调用都会产生 API 费用，先确认提示词再批量生成。
- **看结果再迭代**：生成后用图片查看工具检查，再根据问题改提示词，每次只改一两个变量。

## 参数

| 参数 | 缩写 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `--prompt` | `-p` | 必填 | 提示词 |
| `--output` | `-o` | 必填 | 本地保存路径，目录不存在时会自动创建 |
| `--model` | `-m` | `gpt-image-2.5-sunburst` | 图片模型 |
| `--size` | `-s` | `1024x1024` | 分辨率 |
| `--quality` | `-q` | `high` | `auto`/`low`/`medium`/`high` 等，取决于模型 |
| `--base-url` | `-b` | `https://api.openai.com` | 可含末尾 `/v1` |
| `--api-key` | `-k` | 自动推断 | 会暴露在进程参数中，优先用环境变量 |

## 凭据来源

依次尝试：命令行参数 → `OPENAI_API_KEY`/`OPENAI_BASE_URL` → Codex `auth.json` 中的 API key → Codex `config.toml` 里当前选中 provider 的 `base_url` 与 Bearer key（两者需同时存在）。Codex 的 OAuth 登录态不能当作 API key 使用；只有登录态时，需要另外提供 key。依赖 `curl`、`jq`、`python3`。
