---
id: premium-luxury
title: Premium Luxury
domain: design-systems
category: premium-luxury.md
difficulty: intermediate
tags: [luxury, premium, refined, elegant, serif, dark, single-accent, generous-space, design-systems, palette, patterns]
quality_score: 72
last_updated: 2026-06-19
last_note: high-end refined aesthetic
---
# Premium Luxury

> 高端、精致、克制：近黑底 + 单一精炼金属/宝石色、大量呼吸留白、优雅的衬线/无衬线对比、慢而顺滑的动效。"$150k 机构感"——少即是贵。

## When to use

奢侈品牌、高端金融/私行/财富、汽车/腕表/珠宝、高端 SaaS / 企业旗舰、会员制产品、精品内容。**核心**：克制是奢华的本质——任何"多"都减分。**不适合**：大众消费打折促销、儿童、数据密集后台。

## Color palette

```css
:root {
  --color-bg: #0b0b0c;
  --color-surface: #131316;
  --color-surface-elevated: #1b1b1f;
  --color-text: #f5f3ef;               /* 暖白，非纯白 */
  --color-text-secondary: #b8b3a8;
  --color-text-tertiary: #76726a;
  --color-primary: #c8a96a;            /* 单一精炼金 —— 唯一强调，面积极小 */
  --color-primary-hover: #d9bd84;
  --color-accent: #c8a96a;             /* 不引入第二强调色 */
  --color-success: #7d9a76;
  --color-error: #b5544e;
  --color-border: #26262b;
  --color-border-accent: rgba(200,169,106,0.4);
  --radius: 4px;                        /* 极小圆角，克制 */
  --shadow-soft: 0 24px 60px rgba(0,0,0,0.5);
}

@media (prefers-color-scheme: light) {
  :root {
    --color-bg: #faf8f3;               /* 暖象牙白（非奶油 AI 米色——靠极低饱和的真象牙 + 金强调区分） */
    --color-surface: #ffffff;
    --color-text: #1a1814;
    --color-text-secondary: #5c574d;
    --color-primary: #9a7b3f;          /* 浅底下加深金以保对比 */
    --color-border: #e7e2d6;
  }
}
```

## Typography

- **Display / Headlines**: `"Canela", "Ogg", "Tiempos Headline", Georgia, serif`（精致衬线建立高级感）weight 400–500
- **Body / UI**: `"Söhne", "Suisse Int'l", "Neue Haas Grotesk", system-ui, sans-serif`, weight 400
- 衬线标题 × 无衬线正文的对比，是奢华版式的标志。

| Level | Size | Weight | Line-height | Letter-spacing | Use |
|---|---|---|---|---|---|
| display | 4rem (64px) | 400 | 1.05 | -0.01em | Hero（衬线，细 weight 反而更贵） |
| h1 | 2.5rem (40px) | 500 | 1.15 | -0.005em | Section（衬线） |
| h2 | 1.5rem (24px) | 500 | 1.25 | 0 | Subsection |
| body-lg | 1.25rem (20px) | 400 | 1.7 | 0 | Lead（无衬线） |
| body | 1rem (16px) | 400 | 1.7 | 0 | 正文 |
| overline | 0.75rem (12px) | 500 | 1.2 | 0.18em | 标签，ALL CAPS，宽字距 |

## Spacing

8px 基，**慷慨**：`8 / 16 / 32 / 64 / 96 / 160 / 240`。Hero 与区块用 160px+ 垂直留白。留白是主角。

## Layout

- 大量负空间；少而精的元素；严格对齐与基线网格。
- 居中或经典栅格皆可，但**密度低**——一屏只讲一件事。
- 全幅高质量影像（产品/材质特写）配极简文字。

## Component patterns

### Hero
- 衬线大标题（细 weight）+ 一行副标 + 一个克制的描边/文字按钮（非实心大色块）。
- 背景：纯深色或极细的材质纹理；金色仅出现在一处。

### Card
- 极小圆角、`--shadow-soft` 极柔阴影或 1px `--color-border`；可用"双层内描边"（concentric border）增加精工感。
- hover：极轻微上浮 + 边框转金，动作慢（500-700ms）。

### Button
- 首选描边/幽灵按钮或细金线；实心仅留给唯一主 CTA。圆角小。

## Motion

- `--ease: cubic-bezier(0.32, 0.72, 0, 1)`；时长 **500–700ms**（慢即贵）；退场 ≈75%。
- 入场：缓慢 fade + 轻微上移 + 轻微 blur 收敛；尊重 `prefers-reduced-motion`。

## Do

- 单一金/宝石强调色，面积极小（一屏 1-2 处）。
- 衬线标题 × 无衬线正文；细字重；宽字距 ALL CAPS 标签。
- 慷慨留白 + 慢动效 + 高质量影像。

## Don't

- 多强调色、饱和色、渐变、紫色。
- 拥挤密集、小留白、廉价的实心大色块按钮。
- 快/弹跳动效（与"贵"相反）；emoji。
