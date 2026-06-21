---
id: brutalist-bold
title: Brutalist Bold
domain: design-systems
category: brutalist-bold.md
difficulty: intermediate
tags: [brutalist, swiss, editorial, mono, high-contrast, oversized-type, design-systems, palette, patterns]
quality_score: 72
last_updated: 2026-06-19
---
# Brutalist Bold

> 瑞士国际主义 / 数字野兽派：巨型排版、单色高对比、硬边、网格驱动、近乎零圆角。强烈、自信、有态度。

## When to use

创意机构、作品集、时尚/文化/音乐、艺术展、宣言式落地页、开发者硬核工具的"反精致"品牌。要"被记住"胜过"友好"的产品。**不适合**：需要亲和力的消费/教育/医疗、数据密集后台。

## Color palette

```css
:root {
  --color-bg: #0a0a0a;
  --color-surface: #141414;
  --color-surface-elevated: #1c1c1c;
  --color-text: #fafafa;
  --color-text-secondary: #a3a3a3;
  --color-text-tertiary: #6b6b6b;
  --color-primary: #e6ff00;            /* 单一刺眼信号色：电光黄 */
  --color-primary-hover: #f0ff4d;
  --color-accent: #e61919;             /* 危险红，仅用于最强调 */
  --color-border: #2a2a2a;
  --color-border-strong: #fafafa;      /* 硬边：纯白/纯黑 1-2px */
  --color-success: #00e676;
  --color-error: #e61919;
  --radius: 0px;                       /* 野兽派：拒绝圆角 */
}

@media (prefers-color-scheme: light) {
  :root {
    --color-bg: #f2f2f2;
    --color-surface: #ffffff;
    --color-text: #0a0a0a;
    --color-text-secondary: #404040;
    --color-primary: #1a1aff;          /* 浅色下换电光蓝 */
    --color-border-strong: #0a0a0a;
  }
}
```

## Typography

- **Display**: `"Archivo Expanded", "Anton", "Helvetica Now Display", Helvetica, Arial, sans-serif`, weight 800–900
- **Body**: `"Inter Tight", Helvetica, Arial, sans-serif`（此档允许 Helvetica 家族——它是瑞士风的正统）
- **Mono / 数字**: `"JetBrains Mono", "Space Mono", monospace`

| Level | Size | Weight | Line-height | Letter-spacing | Use |
|---|---|---|---|---|---|
| mega | clamp(4rem, 11vw, 15rem) | 900 | 0.85 | -0.04em | 满屏巨标题，UPPERCASE |
| display | 4rem (64px) | 800 | 0.9 | -0.03em | Hero |
| h1 | 2.5rem (40px) | 800 | 0.95 | -0.02em | Section，常 UPPERCASE |
| h2 | 1.5rem (24px) | 700 | 1.1 | -0.01em | Subsection |
| body | 1rem (16px) | 400 | 1.5 | 0 | 正文 |
| label | 0.75rem (12px) | 700 | 1.1 | 0.08em | 标签，UPPERCASE + mono |

## Spacing

8px 基：`8 / 16 / 24 / 48 / 80 / 128`。区块之间用**硬分隔线**（1-2px 实线）而非留白渐变。

## Layout

- 暴露的网格：可见的列线/基线，模块化排布，刻意的不对称。
- 满铺色块分区；大留白 + 大字块对撞。
- 内容硬左对齐为主（非居中）；编号区块用 mono 数字 `01 / 02 / 03`。

## Component patterns

### Hero
- 巨型 UPPERCASE 标题（mega 级），一句话副标，方形（零圆角）实心按钮。
- 背景纯色块；可叠一条 1px 全宽分隔线。

### Card
- 硬边框（1-2px 实线 `--color-border-strong`），零圆角，零阴影；hover 时整卡反色（背景↔前景互换）。

### Button
- 方角、实心、`--color-primary`；hover：瞬时反色或位移 2px + 出现硬投影 `4px 4px 0 var(--color-text)`。

## Motion

- `--transition: 120ms steps(1)` 或 `cubic-bezier(0.2,0,0,1)`——干脆、机械，不要缓动 bounce。
- 入场用硬切/位移，不要柔和 fade；尊重 `prefers-reduced-motion`。

## Do

- 一个页面只用 1 个信号色，且面积极小（CTA、关键数字）。
- 巨字 + 硬边 + 网格三件套；mono 数字。
- UPPERCASE 标题与标签，字距收紧（标题）/放开（标签）。

## Don't

- 圆角、柔和阴影、渐变、紫色——与本档冲突。
- 居中、柔弱小字、卡片堆叠的"友好"布局（那是 soft-warm）。
- 超过 2 种信号色；emoji。
