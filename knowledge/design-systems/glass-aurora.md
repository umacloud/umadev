---
id: glass-aurora
title: Glass Aurora
domain: design-systems
category: design-systems
difficulty: intermediate
tags: [glassmorphism, aurora, frosted, depth, ai, modern, gradient-controlled, design-systems, palette, patterns]
quality_score: 72
last_updated: 2026-06-19
---
# Glass Aurora

> 克制的玻璃拟态 + 极光氛围：磨砂半透明层、深邃暗背景上一抹受控的极光渐变、清晰的 z 轴层级。现代、有科技感、不廉价。

## When to use

AI / 大模型产品、现代消费工具、creator 工具、做得高级的 web3/crypto、需要"未来感"的发布页。**关键**：极光是**氛围**不是 hero 主体；一旦满屏渐变就变 AI-slop。**不适合**：数据密集后台、严肃金融、儿童教育。

## Color palette

```css
:root {
  --color-bg: #07080d;                 /* 近黑带蓝 */
  --color-surface: rgba(255,255,255,0.04);   /* 玻璃层：低透明白 */
  --color-surface-strong: rgba(255,255,255,0.08);
  --color-glass-border: rgba(255,255,255,0.12);  /* 1px 反光描边 */
  --color-text: #f4f6fb;
  --color-text-secondary: #aab1c5;
  --color-text-tertiary: #6b7390;
  --color-primary: #5e8bff;            /* 主色：克制的电蓝 */
  --color-primary-hover: #7aa2ff;
  --color-accent: #36e0c8;             /* 青绿点缀 */
  --color-success: #36e0c8;
  --color-error: #ff5d6c;
  --color-border: rgba(255,255,255,0.08);
  /* 极光：极低饱和、大模糊、固定在背景，绝不做成 hero 主色块 */
  --aurora: radial-gradient(60% 50% at 20% 0%, rgba(94,139,255,0.18), transparent 70%),
            radial-gradient(50% 40% at 90% 10%, rgba(54,224,200,0.12), transparent 70%);
  --blur: 16px;
  --radius: 16px;
}

@media (prefers-color-scheme: light) {
  :root {
    --color-bg: #f5f7fc;
    --color-surface: rgba(255,255,255,0.7);
    --color-glass-border: rgba(10,20,40,0.08);
    --color-text: #0c1124;
    --color-text-secondary: #4a5270;
    --aurora: radial-gradient(60% 50% at 20% 0%, rgba(94,139,255,0.10), transparent 70%);
  }
}
```

## Typography

- **Display / Headlines**: `"General Sans", "Geist", system-ui, sans-serif`, weight 600
- **Body**: `"Inter", system-ui, sans-serif`（此档正文允许 Inter，但标题必须用更有性格的字）
- **Mono**: `"Geist Mono", monospace` — 用于数据/代码

| Level | Size | Weight | Line-height | Letter-spacing | Use |
|---|---|---|---|---|---|
| display | 3.5rem (56px) | 600 | 1.05 | -0.02em | Hero |
| h1 | 2.25rem (36px) | 600 | 1.1 | -0.015em | Section |
| h2 | 1.5rem (24px) | 600 | 1.2 | -0.01em | Subsection |
| body-lg | 1.125rem (18px) | 400 | 1.6 | 0 | Lead |
| body | 1rem (16px) | 400 | 1.6 | 0 | 正文 |
| caption | 0.8125rem (13px) | 500 | 1.4 | 0.02em | 标签 |

## Spacing

4px 基：`4 / 8 / 12 / 16 / 24 / 32 / 48 / 64 / 96`。玻璃卡片内边距 24-32px，卡片间距 16-24px。

## Layout

- 深背景 + 固定的 `--aurora` 氛围层（`position: fixed; filter: blur(40px)`，不随滚动喧宾）。
- 内容用磨砂玻璃卡浮在其上，建立清晰 z 层级。
- 居中适度但配非对称强调；不要满屏等高卡。

## Component patterns

### Glass card
- `background: var(--color-surface); backdrop-filter: blur(var(--blur)); border: 1px solid var(--color-glass-border); border-radius: var(--radius);`
- 顶部 1px 高光（`box-shadow: inset 0 1px 0 rgba(255,255,255,0.15)`）。

### Hero
- 标题 + 副标 + 主 CTA；背景是 `--aurora`（低饱和、大模糊），**不是**实心紫渐变块。
- CTA：实心 `--color-primary`，hover 微亮 + 轻微上浮。

### Button / Input
- 玻璃或实心两种；focus 用 `--color-primary` 2px 环 + 轻微外发光（克制）。

## Motion

- `--ease: cubic-bezier(0.16, 1, 0.3, 1)`；时长 200/300ms；卡片 hover 上浮 2-4px + 高光增强。
- 入场 staggered fade-up（`translateY(12px)`）；尊重 `prefers-reduced-motion`。

## Do

- 极光：低饱和、大模糊、固定背景、面积克制——只做氛围。
- 玻璃层要有清晰的 1px 反光描边和层级；深色为主。
- 主色 + 一个青绿点缀，其余中性。

## Don't

- 满屏鲜艳渐变 / 紫→粉 hero（这正是要避免的 AI-slop）。
- 玻璃叠太多层导致可读性差、对比不足。
- 在亮背景上乱用低透明玻璃（对比塌掉）。
