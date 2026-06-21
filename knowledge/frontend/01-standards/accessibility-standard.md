---
id: accessibility-standard
title: 无障碍标准（a11y · 商业级 · WCAG）
domain: frontend
category: 01-standards
difficulty: intermediate
tags: [无障碍, accessibility, a11y, wcag, aria, 键盘, 屏幕阅读器, 对比度, 焦点, 语义化, 商业级]
quality_score: 92
last_updated: 2026-06-19
---

# 无障碍标准（a11y · 商业级 · WCAG）

> 无障碍不是可选项——它是商业产品的法律合规要求（ADA/欧盟无障碍法案/信通院）与用户覆盖问题。目标对齐 **WCAG 2.1 AA**。AI 生成的 UI 常忽略 a11y，必须显式要求。

## 1. 语义化 HTML（地基）

- 用**语义标签**：`<button>` `<a>` `<nav>` `<main>` `<header>` `<h1-h6>` `<label>` `<table>`，而不是一堆 `<div onClick>`。
- 标题层级正确（一个 `<h1>`，逐级 h2/h3，不跳级、不为样式滥用）。
- 列表用 `<ul>/<ol>`，表格数据用 `<table>` + `<th scope>`。
- 用语义元素就自带键盘/焦点/角色，少写 ARIA。

## 2. 键盘可达

- **所有交互都能纯键盘完成**：Tab 顺序合理、可聚焦、可 Enter/Space 触发。
- 焦点**可见**（清晰 focus ring，不要 `outline:none` 不补替代）。
- 自定义控件（下拉/弹窗/标签页）实现对应键盘交互（方向键/Esc 关闭/焦点陷阱）。
- 跳过导航链接（skip to content）便于键盘/屏读用户。

## 3. ARIA（语义不够时才补，别滥用）

- 优先语义 HTML；不得已才用 ARIA role/属性。
- 表单错误 `aria-invalid` + `aria-describedby` 关联错误文本；动态更新用 `aria-live`/`role="alert"`。
- 弹窗 `role="dialog"` + `aria-modal` + 焦点管理（打开聚焦、关闭归还、焦点陷阱）。
- 图标按钮要有 `aria-label`；纯装饰图标 `aria-hidden`。
- **错误 ARIA 比没有更糟**——不确定就用语义 HTML。

## 4. 视觉与对比

- 文本对比度 ≥ **4.5:1**（大字 3:1）；UI 组件/状态对比 ≥ 3:1。
- **不只靠颜色**传达信息（错误/状态加图标/文本，照顾色盲）。
- 支持放大到 200% 不破版；尊重 `prefers-reduced-motion`（减少动画）。
- 触控目标足够大（≥44px）。

## 5. 图片与媒体

- 图片有 `alt`（装饰图 `alt=""`）；复杂图表有文字说明。
- 视频有字幕、音频有文字稿（如适用）。

## 6. 表单 a11y

- 每个输入关联 `<label>`（不要只用 placeholder 当标签）。
- 必填/格式要求明确告知；错误可被屏读播报并定位。
- 分组用 `<fieldset>/<legend>`。

## 7. 反模式（出现即不合格）

- `<div onClick>` 当按钮（不可键盘、无角色）；`outline:none` 去掉焦点环不补。
- 只用颜色表达错误/状态；对比度不达标。
- placeholder 当 label；图片无 alt；图标按钮无 label。
- 弹窗无焦点管理；自定义控件不可键盘操作。
- 滥用/错误 ARIA。

## 8. 最低交付 checklist

- [ ] 语义化 HTML（button/a/nav/label/标题层级）；少 div onClick。
- [ ] 全键盘可达 + 可见焦点 + 弹窗焦点管理 + skip link。
- [ ] 表单 label 关联、错误 aria-invalid/live、图标按钮 aria-label、装饰 aria-hidden。
- [ ] 对比度 ≥4.5:1、不只靠颜色、支持 200% 缩放、尊重 reduced-motion、触控≥44px。
- [ ] 图片 alt、复杂内容文字替代。
- [ ] 用 axe/Lighthouse 自测 a11y。

---
**参考**：WCAG 2.1 AA、WAI-ARIA Authoring Practices、MDN 无障碍、axe-core/Lighthouse 审计。
