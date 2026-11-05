---
name: openai-image-gen
description: 需要生成配图、Banner、图标、架构总览、产品插图、分镜画面或 UI/游戏素材并保存为本地文件时使用；可传参考图（角色设定图等）让多张图里的人物保持一致；一次调用直接出成图，明显不对时可局部编辑或改提示词重出，按输出文件限定次数控制费用，只输出一行结果 JSON。Use to generate image assets, diagrams, illustrations, storyboard frames, banners or icons to disk, optionally with reference images for consistent characters.
license: MIT
metadata:
  version: "3.0.0"
---

# OpenAI Image Generation (本地图像生成)

```bash
G=<本技能目录>/scripts/imagegen.py
$G image -o docs/img/arch.webp -f arch.prompt.txt -s 2560x1440 --keep-prompt   # 出图，直接写到输出文件
$G edit  -o docs/img/arch.webp -f fix.txt                                       # 改一处写明的细节
$G image -o art/scene.webp -f scene.txt -s 2560x1440 -r cast/he/sheet.webp -r cast/she/sheet.webp  # 带参考图
$G status -o docs/img/arch.webp
```

每次调用都收费，出图和编辑价格相同。默认 `gpt-image-2.6-sunburst`、medium；`-m gpt-image-2.6-flare` 更快、同价，`-q high` 用于小字和细节多的图。提示词写法与审图要点见 [references/prompting.md](references/prompting.md)。长提示词放文件，`-f -` 读 stdin。

## 能力

- **`image`**：按提示词出一张图写到 `-o`；网关返回的格式与扩展名不同时用 ffmpeg 转换。同一路径再次 `image` 会从头重出，上一张留在 JSON 的 `previous`。
- **`edit`**：把当前图连同修改说明发给编辑接口。编辑高保真保留原图、精确执行写明的改动，但会重绘整张图，连续编辑会让别处的字慢慢走样；没写改动时返回的就是原图，所以与出图提示词相同的编辑会被拒绝。编辑前的版本留在 `previous`。
- **参考图 `-r`**：多张图要出现同一个人物或物件时，先单独出一张设定图，之后每张图用 `-r` 传入，最多 8 张。脚本在提示词末尾注明每张参考图的编号和文件名，并说明构图、姿势和光线只按文字来；提示词里用编号或文件名点名，例如 "the boy from he/sheet.webp"。同一路径的重出和编辑自动沿用上次的参考图（`--no-refs` 去掉），文件改过会警告，删了会拒绝。
- **画幅写进提示词**：有的网关只按提示词文字决定横竖，脚本自动在末尾追加一句画幅说明，例如 "Portrait orientation, 2:3 aspect ratio, taller than wide."。

## 费用边界

- **次数上限**：每个输出路径出图 3 次、编辑 1 次，只计成功的调用。超出时退出码 3、`"status":"refused"`，不请求接口；只有用户明确同意多花钱才加 `--allow-extra`。
- **不重复花钱**：提示词、尺寸、参考图都与之前某次相同的出图，以退出码 3 拒绝。
- **`reset`**：只在同一路径要做一张新图时使用，不能用来绕过上限。

## 结果

- 一行 JSON：`file`、`returned`（实际格式与尺寸）、`budget`、`previous`、`next`；历次成图和状态存放在 `$XDG_STATE_HOME/openai-image-gen/` 下，不进项目目录。
- **`warning`**：网关没按请求的 size、quality 或格式出图时出现。经 ChatGPT 账号转发的网关由服务端决定画质，请求更高档不会更好；HTML 里的 width/height 按 `returned` 写。
- **失败**：退出码 1，错误写在 stderr，目标文件不动、不计次数。`moderation_blocked` 是内容规则拒绝：告诉用户被拒了；改写得更含蓄再试时，说明改了哪些要求，不要悄悄降级。

## 要点

- **尺寸**：图标 `1024x1024`、Banner `1536x1024`（默认）、海报 `1024x1536`、架构总览 `2560x1440`；两边须是 16 的倍数，长短边比不超过 3:1，最大 `3840x2160`。
- **格式**：由扩展名决定，`.webp`/`.jpg` 可配 `--compression`。文档配图用 webp。
- **耗时**：sunburst medium 实测 18–45 秒，2560x1440 的 high 40–60 秒；给调用设的超时不低于 300 秒。
- **不要的字**：模型会自己往画面里加标题、书名之类的字。画面不需要文字时，提示词写 "no text, letters or logos anywhere"。
- **图不适合的场景**：精确的数据图、需要逐条核对的结构图，用 SVG 或 HTML 画；生成图适合做总览和题图。
- **凭据**，依次尝试：`-k`/`-b` 参数 → `OPENAI_API_KEY`/`OPENAI_BASE_URL` → Codex 当前选中的 provider（它的 key 只发往它自己的地址）→ Codex `auth.json`。只依赖 Python 3.11+ 标准库，其余参数见 `--help`。
