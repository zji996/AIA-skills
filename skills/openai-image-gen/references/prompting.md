# 提示词写法

## 通用

- **语言**：提示词一律用英文，即使用户用中文交流。只有画面里要出现的文字保留原语言，逐字写出并加引号。
- **顺序**：主体 → 风格与媒介 → 构图与视角 → 色彩与光线 → 用途约束。用途约束例如“左侧留白放标题”“纯色背景便于抠图”。
- **篇幅**：一两句话的提示词只适合氛围图。

## 示意图、信息图与带文字的画面

模型会按描述摆放元素，也会照抄引号里的字，但不会替你补全结构。所以提示词要写成一份布局说明，按下面的顺序分节：

```text
<一句话：这是什么图、给谁看、什么气质、横竖版>

STYLE: 媒介（flat vector / liquid glass …）、背景 hex、强调色 hex 各自用在哪里、线宽、圆角、光线。
  出现雾化或光晕时写 "flat solid opaque background, no bloom, no haze"；
  要求文字 "crisp, never ghosted or doubled"。

LAYOUT: 从上到下、从左到右逐区描述：每个区域的形状、位置、占比、包含什么、标签是什么。

CONNECTIONS: 每个箭头单独一行，写明起点元素的哪一边、终点元素的哪一边、颜色、有没有标签；
  最后写 "draw no other arrows"。

TEXT RULES: 列出画面上的全部文字串，逐字加引号，然后写
  "no other words, numbers, logos or pseudo-text anywhere"。

AVOID: 3D/isometric, people, robots, circuit textures, lens flares, neon/purple, overlapping labels …
```

- **箭头方向跟着位置走**：箭头方向常由元素的相对位置决定，而不是由描述决定。要让 A 指向 B，先把 B 摆在 A 的下游，例如正下方或右侧，再写起点和终点。
- **易拼错的词**：拆成字母写，例如 `spelled A-N-T-H-R-O-P-I-C`，并写明不带哪些标点。标点也会被模型带偏，例如 `history/` 的斜杠会传到相邻的标签上。
- **文字数量**：标签越少越准，超过约 40 个字符串时，小字出错的概率明显上升。空的气泡、色块要写 "no text inside"，否则模型会补上伪文字。
- **层级**：标题 > 卡片标题 > 芯片或条带标签，分别写清相对字号。
- **提示词存档**：正式图用 `--keep-prompt`，提示词存为输出旁的 `<名字>.prompt.txt`，便于日后复现。

## 编辑提示词

编辑会重绘整张图，并参考原图保持布局。提示词只写要改的地方，例如：

```text
Keep the whole image exactly as it is. Only change: the tag "Anthropic/" becomes "Anthropic" (no slash).
```

连续编辑会让别处的字慢慢走样，所以预算只给 1 次编辑。同一个错误改不掉时，就去修提示词，再走一轮草稿和正式图。

## 模型与参数

- **模型**：`gpt-image-2.5-sunburst` 是 2.5 代的高质量模型，也是正式图的默认；`gpt-image-2.5-flare` 更快也更便宜，质量约等于 gpt-image-2，用于草稿。
- **质量档**：`low`/`medium`/`high`/`xhigh`/`max`。
  - 官方建议 `xhigh`/`max` 只在 `high` 达不到要求时用，更高档不保证更好。
  - 实测 2560x1440 下，`xhigh` 与 `high` 都要 40–60 秒。
- **尺寸**：
  - 两边都须是 16 的倍数，长短边比不超过 3:1，最大 `3840x2160`；
  - 像素数超过 2560x1440 属实验性。
  - 草稿按同一比例缩到长边 1536。
