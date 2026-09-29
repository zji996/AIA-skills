---
name: openai-image-gen
description: 需要生成配图、Banner、图标、产品插图或 UI/游戏素材并直接保存为本地文件时使用；调用 OpenAI Image API，只输出一行结果 JSON，不把图片数据灌入上下文。Use to generate image assets, illustrations, banners or icons to disk.
license: MIT
metadata:
  version: "1.3.0"
---

# OpenAI Image Generation (本地图像生成)

```bash
G=<本技能目录>/scripts/generate-image.sh

$G --prompt "Isometric 3D hero illustration of a SaaS analytics dashboard, soft blue palette, clean white background" \
   --output assets/hero.png

$G -p "Wide cinematic banner of glowing neural network nodes in deep blue space" \
   -o public/images/banner.png -s 1536x1024

$G -f docs/img/arch.prompt.txt -o docs/img/arch.webp -s 2560x1440 -q xhigh   # 长提示词从文件读，`-f -` 读 stdin；扩展名决定格式
$G -f fix.txt -i docs/img/arch.webp -o docs/img/arch-v2.webp -s 2560x1440     # 在已有图上局部修改（edits 接口）
```

成功时输出一行 JSON：`{"status":"ok","file":...,"model":...,"size":...,"quality":...,"mode":"generate|edit","seconds":...}`。失败时不改动已有的目标文件，也不输出成功状态。

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
- **模型与质量**：默认 `gpt-image-2.5-sunburst`，是 2.5 代的高质量模型；`gpt-image-2.5-flare` 更快，质量约等于 gpt-image-2。质量档 `low`/`medium`/`high`/`xhigh`/`max`，默认 `high`；标签密集的信息图可以用 `xhigh`，实测 2560x1440 耗时与 `high` 相当（40–60 秒），细节与质感更好。官方建议 `xhigh`/`max` 只在 `high` 达不到要求时用，更高档不保证更好。
- **费用**：每次调用都收费，大图和高档更贵。先把提示词写完整、自查一遍再调用，别靠反复重生成试错；草图用 `low`/`medium` 或小尺寸，定稿一次到位。用户说“差不多就行”时停下，交付当前最好的一版并说明剩余瑕疵。
- **尺寸**：除三个标准尺寸外，可以用任意 `宽x高`，两边都是 16 的倍数、长短边比不超过 3:1、最大 `3840x2160`；超过 2560x1440 的像素数属实验性。海报与架构总览用 `2560x1440`。
- **格式**：输出扩展名为 `.webp`/`.jpg` 时自动请求对应格式，`--compression` 调压缩率；文档配图优先 webp，体积通常只有 PNG 的十分之一。
- **看结果再迭代**：生成后用图片查看工具检查，再根据问题改提示词，每次只改一两个变量。带文字的图逐字核对每个文字串（大图先裁剪放大再看），再核对箭头与归属等语义；不满意的版本先另存，避免覆盖已经可用的一版。
- **修改已有图**：`-i <图>` 走 edits 接口，适合修重影、背景、单个标签等局部问题，布局基本保持。它每次都会重绘整张图，连续编辑会让别处的字慢慢走样，所以只编辑一两次；同一个错误改不掉时，改提示词重新生成。
- **箭头与位置**：箭头方向常跟着元素的相对位置走，而不是跟着描述走。要让 A 指向 B，先把 B 排在 A 附近（例如正下方），再写明起点和终点。
- **耗时**：`high` 质量、`1536x1024` 一次约 45 秒，排队时更久。给调用设的超时不要低于 300 秒。

## 参数

| 参数 | 缩写 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `--prompt` | `-p` | 与 `-f` 二选一 | 提示词 |
| `--prompt-file` | `-f` | 与 `-p` 二选一 | 从文件读提示词，`-` 表示 stdin |
| `--output` | `-o` | 必填 | 本地保存路径，目录不存在时会自动创建 |
| `--model` | `-m` | `gpt-image-2.5-sunburst` | 图片模型 |
| `--size` | `-s` | `1024x1024` | 分辨率 |
| `--quality` | `-q` | `high` | `auto`/`low`/`medium`/`high`/`xhigh`/`max`，取决于模型 |
| `--image` | `-i` | 无 | 在这张图上修改（可重复），走 `/v1/images/edits` |
| `--format` |  | 按扩展名，否则 `png` | `png`/`webp`/`jpeg` |
| `--compression` |  | 无 | webp/jpeg 压缩率 0–100 |
| `--base-url` | `-b` | `https://api.openai.com` | 可含末尾 `/v1` |
| `--api-key` | `-k` | 自动推断 | 会暴露在进程参数中，优先用环境变量 |

## 凭据来源

依次尝试：命令行参数 → `OPENAI_API_KEY`/`OPENAI_BASE_URL` → Codex `config.toml` 里当前选中 provider 的 `base_url` 与 Bearer key（两者需同时存在）→ Codex `auth.json` 中的 API key，发往 Codex 同样会发往的地址：选中 provider 设了 `requires_openai_auth` 时用它的 `base_url`，否则用官方 API。Codex 的 OAuth 登录态不能当作 API key 使用；只有登录态时，需要另外提供 key。依赖 `curl`、`jq`、`python3`。
