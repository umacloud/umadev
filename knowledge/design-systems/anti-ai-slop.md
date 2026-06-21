# 反 AI-slop 与设计品味（每次做 UI 必读 · 默认强制）

> 纯模型产出最大的问题不是"丑"，是"**generic（千篇一律、一眼 AI）**"。区别精品与 generic 的，是 **token 背后的"为什么"** 与一套**正向的品味规则**——不是只躲开几个雷，而是主动做对。本文件随设计系统**默认绑定**，每次 UI 都生效。

---

## 0. 先定方向，再写代码（最关键的一步）

动手前先在 UIUX 文档里明确（一句话即可，但必须真的做选择）：
- **Purpose & Audience**：解决什么问题？谁在用？（决定信息密度与语气）
- **一个大胆的概念方向（Motif）**：在下列里**选一个并 commit**，不要"clean professional"这种空话——
  `极简瑞士 / 编辑杂志感 / 技术工具冷峻 / 温暖亲和 / 大胆几何 / 极繁主义 / 复古未来 / Art Deco / 野兽派 brutalist / 玻璃拟态`。
- **令人记住的一点**：这个界面有什么是用户会记住的？（一个标志性的排版/色彩/动效签名）
- **真实参照**：选 1–3 个真实产品作锚（如"信息密度像 Linear、排版像 Stripe、留白像 Apple"），并写出**要借鉴的具体动作**，而非泛泛"现代"。

> 原则：**reference-based 胜过 descriptive**。"像 Linear 一样:单色为主、1px 内描边代替投影、密集信息层级" 远胜 "做个干净专业的页面"。

---

## 1. 排版即身份（Typography as identity）

- **不要默认 Inter / Arial / 系统字体**——它们是 AI 味的头号信号。选有性格的字体族（可配一个 display 标题字 + 一个干净正文字），用 `next/font` 或 `@font-face` 真正加载。
- **战略性字距 letter-spacing**：大标题收紧（`-0.02em ~ -0.01em`，更自信）；全大写的小标签/eyebrow 放开（`0.06em ~ 0.1em`）；正文 `0`。
- **模块化字阶**（用比例而非随手取值）：1.2（紧凑/数据密）或 1.25（通用）；建立 `--text-xs…--text-5xl`，标题与正文**对比要拉开**（display 可到 48–96px）。
- 正文行高 1.5–1.7，行宽 50–75 字符；标题行高 1.05–1.2。一个页面字重 ≤3 种。

## 2. 色彩：主色 + 锐利强调，OKLCH 思维

- **70–90% 中性 + 5–10% 主色 + 1 个锐利 accent** 留给最高优先 CTA。不要满屏高饱和、不要彩虹。
- 用 **OKLCH** 定义色阶（感知均匀、暗色更稳），P3 广色域可选；中性色带一点点主色色相的"温度"，别用纯灰 `#808080`。
- 全部走**语义 token**（`--color-primary` / `--color-surface` / `--color-muted-foreground`…），组件里**永不**写裸 hex。深色模式是必须，不是可选。
- 对比度 ≥ 4.5:1（正文）/ 3:1（大字与 UI），别 gray-on-gray。

## 3. 深度与阴影：contextual，不是统一糊一层

- 阴影要**表达 z 轴关系**：贴地的卡片极轻、浮层/弹窗更深、按下变浅；同层一致。
- 现代精品更爱 **1px 内/外描边 + 极轻阴影** 而非厚重 drop-shadow（"为什么用 1px 描边而非投影"——更干净、更工程感）。
- 半透明/毛玻璃用于建立层级，不是装饰。

## 4. 氛围与质感：别留大白板

- 背景别只用纯色：用**极细的渐变、噪点纹理、网格/点阵、光晕**制造氛围（克制，不喧宾）。
- 避免"居中一切 + 等高卡片 3 连 + 没有证据的 hero"这种模板感；用**非对称布局、节奏变化（密-疏交替）、锚点对齐**。
- 真实内容优先：真实文案/截图/数据，不要 Lorem ipsum、不要"Welcome to [App]"、不要编造指标。

## 5. 动效：必须自证存在

- 动画要**有意义**（引导注意、表达空间连续），时长 150–300ms，缓动用自然曲线（`cubic-bezier(0.2,0,0,1)` 类）。
- 一次**编排好的入场（staggered reveal）** 胜过满屏散乱微交互；尊重 `prefers-reduced-motion`。
- 不要 animate width/height（用 transform/opacity）；不要纯装饰性动画。

## 6. 优先级规则表（按影响排序，源自平台官方 HIG / Material）

| 优先 | 类别 | 必须做 | 不可做（Anti-pattern） |
|---|---|---|---|
| 1 | 无障碍 | 对比 4.5:1、焦点环可见、icon 按钮加 aria-label、键盘可达、不只靠颜色传达 | 删焦点环、icon-only 无标签、颜色单独表意 |
| 2 | 触控/交互 | 触控目标 ≥44×44、间距 ≥8、异步有 loading、按压有反馈 | 只靠 hover、0ms 瞬变、手势无替代 |
| 3 | 性能 | WebP/AVIF、懒加载、给图片留位防 CLS、骨架屏 | 布局抖动、首屏长 spinner、长列表不虚拟化 |
| 4 | 风格选择 | 匹配产品类型、全站一致、**SVG 图标库（Lucide/Heroicons）** | emoji 当图标、flat 与拟物乱混 |
| 5 | 布局/响应式 | mobile-first 断点、viewport meta、无横向滚动 | 固定 px 容器、禁缩放、横向滚动主内容 |
| 6 | 字体/色彩 | base 16px、行高 1.5、语义 token | 正文 <12px、gray-on-gray、组件里裸 hex |
| 7 | 动效 | 150–300ms、传达意义、空间连续 | 装饰性动画、animate 宽高、无 reduced-motion |
| 8 | 表单/反馈 | 可见 label、错误就近、helper 文本、渐进披露 | 只用 placeholder 当 label、错误只堆顶部 |
| 9 | 导航 | 可预测返回、底部导航 ≤5、深链 | 导航过载、返回行为坏、无深链 |
| 10 | 图表/数据 | 图例、tooltip、无障碍配色 | 只靠颜色区分数据 |

## 7. 组件必须覆盖全部状态

每个交互组件做满 **7 态**：default / hover / focus(可见焦点环) / active(按下) / disabled / loading / error。
每个数据视图做满 **空 / 加载(骨架) / 错误 / 正常 / 极多** 五态——空态要有引导而非空白。

## 8. 出现即不合格（P0 cardinal sins）

紫→粉渐变 hero · emoji 当功能图标 · 系统默认字体唯一 · 组件里裸 hex · 无深色模式 · Lorem ipsum / "Welcome to [App]" / 编造指标 · 居中一切 + 等高卡片堆叠 + 无证据 hero · 焦点环被删 · 动画无意义。

## 9. 最低交付 checklist

- [ ] 先定 Motif/参照/记忆点，UIUX 文档 `## Visual direction` 写清。
- [ ] distinctive 字体 + 模块化字阶 + 战略 letter-spacing；语义 token + OKLCH + 深色。
- [ ] contextual 阴影/描边 + atmospheric 背景；非对称节奏，无模板感。
- [ ] 动效自证存在(150–300ms + 编排入场 + reduced-motion)。
- [ ] 优先级表 1→10 全过；组件 7 态 + 数据 5 态；真实内容；无 P0 sins。

---

## 10. 硬规格（battle-tested 具体值，照抄即可）

**字体 reflex-reject（默认禁用，除非品牌 brief 明确点名）**
- 被用烂的"安全默认"：`Inter / Roboto / Open Sans / Lato / Montserrat / Poppins / Nunito` — 一眼 AI。
- 设计师"反射性"高级字：`Playfair Display / Fraunces / Cormorant / Space Grotesk / DM Sans` — 也别条件反射地用。
- 正确做法：先写**三个具象品牌气质词**（"warm and mechanical and opinionated"，不是"modern"），再据此选字；display + body 在**对比轴**上配对，≤3 个字体族。

**色彩硬规格**
- **60-30-10**（按视觉重量）：60% 中性面 / 30% 次要 / 10% 主色；accent 占视口 **≤3%**，只给最高优先 CTA。
- 用 **OKLCH**；中性色向品牌色相偏 `+0.005~0.015` chroma（有温度的灰）。
- **禁 AI 紫**：hue 250–310 的主色、`#7c3aed / #8b5cf6 / #a855f7 / #764ba2 / #667eea` 渐变。
- **禁"奶油米色"带**：OKLCH `L 0.84–0.97 · C<0.06 · hue 40–100`（以及 `--paper/--cream/--sand/--linen` 这种 token 命名本身就是 tell）。
- 深色模式：正文字重 −50，抬升用**更亮**不是更暗，绝不切换色相。纯 `#000/#fff` 禁用（用近黑近白）。

**字体/标题硬规格**
- hero 标题按**字数**定级：21–50 字符 = display 大字；**>90 字符 = 头号 AI tell**（拆短）。
- display 字距下限 `≥ -0.04em`；ALL-CAPS 字距 `0.05–0.12em`；全大写行高下限 1.0；**斜体标题全局禁用**。
- 标题与正文字重差 `≥300`；type scale 比例 `≥1.25`；正文 `≥16px`。

**间距/层级**
- 4pt 刻度 `4 8 12 16 24 32 48 64 96`；紧密分组 8–12px、区块分隔 48–96px。
- 卡片栅格 `repeat(auto-fit, minmax(280px,1fr))`；z-index 用**语义命名层级**（绝不 `999/9999`）。

**动效硬规格**
- 时长桶 `120 / 220 / 420ms`（或 100/300/500）；**退场 ≈ 进场的 75%**；80ms 以下视为"瞬时"。
- 缓动：`--ease-out: cubic-bezier(0.16,1,0.3,1)`、quart `(0.25,1,0.5,1)`、expo；**禁 bounce `(0.34,1.56,0.64,1)` / elastic 过冲**。
- stagger `calc(var(--i)*50ms)` 封顶 500ms；`@media (prefers-reduced-motion)` 块必写；只用 transform/opacity 动，禁 animate 宽高。

**结构变化（AI 指纹的真正来源）**
- "**对称读作'生成的'，非对称读作'有意的'**"。先选一个具名的**页面骨架/导航/页脚**原型再写代码；同一产品多页不要重复同一种骨架。
- 图标当作排版：同一页只用一套图标库、同一描边粗细。

**文案 tells（出现即扣分）**
- 营销空话：`streamline / empower / supercharge / world-class / enterprise-grade / seamless / cutting-edge`。
- 编造指标：`trusted by 50,000+ / 99.9% / 10x faster`（无真实出处别写）。
- 占位名：`Jane Doe / John Smith / Acme / example.com / lorem ipsum`。
- 假 UI chrome：div 画的浏览器地址栏/手机边框/手绘 sketchy SVG。
- em-dash `—` 滥用（≥5 个）是常见 tell；克制使用。

> 注：设计**生成时先大胆**，把无障碍的强校验放到 review/quality 阶段——设计阶段过度提醒无障碍会让模型退回"安全但平庸"。但对比度、焦点环、aria 这些**底线仍不可破**。

## 11. 强制差异化（break the default aesthetic）

设计前先选**一个**家族并 commit（force a choice，不要"融合多个"）：
`modern-minimal · editorial-clean · tech-utility(terminal/data-dense) · soft-warm · bold-geometric · brutalist-bold(swiss/tactical) · glass-aurora · premium-luxury` —— 选定后写一行 **AVOID**（明确这个产品**不**走哪个方向）。

**10 条硬拒绝（出现即重做）**：
1. 不用默认 teal `#16d5e6` / 默认 indigo-blue 作主色（除非 brief 点名）。
2. **只用一个** accent；第二个强调色 = 失败。
3. 容器嵌套深度 **≤2**（卡中卡中卡 = 失败）。
4. 主字体不用 Inter/Roboto/Arial/Helvetica（brutalist 档的 Helvetica 例外）。
5. Hero 里不放 3 列等宽 feature 卡网格。
6. 一个页面只用**一套**图标库；能用纯排版表达就别堆图标网格。
7. 深色背景上不放紫→粉/靛紫渐变。
8. 不用"玻璃卡堆叠"当 hero（除非 glass-aurora 档且克制）。
9. 动效只为表意，且必带 `prefers-reduced-motion`。
10. 文案产品专属，不用占位/buzzword/编造指标。

**成败判据（thumbnail test）**：把成品缩成缩略图，**不应与任何其它 AI 生成的项目雷同**——一眼能认出是"这个产品"，而不是"又一个 AI 页面"。

**生成顺序（每步定了再下一步）**：① Design Read 一句话(什么页面/给谁/什么气质/选哪个家族) → ② 锁 token 表(OKLCH/hex + 字体 + 图标库 + 间距/圆角) → ③ 布局骨架(可先 ASCII 线框) + 动效规格 → ④ 才写实现，只引用已锁 token。

## 12. 交付前自评门（pre-emit self-critique，必做）

把 UI 交回前，先对自己的产出按 6 个维度各打 1–5 分；**任何一项 <3 必须先改再交**：
- **Philosophy 立场**：是否 commit 了一个明确方向，而非"安全平均"？
- **Hierarchy 层级**：一眼能看出主次？标题/正文对比够强？
- **Execution 执行**：token/间距/状态/对齐到位，无半成品？
- **Specificity 专属性**：内容/配色/排版属于"这个产品"，而非通用模板？
- **Restraint 克制**：有没有多余装饰/第二强调色/无意义动效？
- **Variety 变化**：结构是否有意打破对称与等距，而非千篇一律？

> 自评是给自己看的硬门槛——分低就重做，不要把"能跑的 generic"当完成。
> 终极判据仍是 **thumbnail test**：缩成缩略图，能不能一眼认出是"这个产品"而不是"又一个 AI 页面"。

---
**一句话**：先 commit 一个大胆方向 + 真实参照，再把每个 token 都问"为什么是它"——这就是从 generic 到精品的全部距离。
