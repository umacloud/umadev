---
id: android-material-design
title: Android 设计规范（Material Design 3 · 官方）
domain: mobile
category: 01-standards
difficulty: intermediate
tags: [android, 设计规范, material-design, material3, m3, 色彩角色, type-scale, elevation, motion, dynamic-color, 自适应, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# Android 设计规范（Material Design 3 · 官方）

> 纯 Claude/Codex 写 Android UI 常不遵循 Material、像套了个 iOS 或 web 壳。本标准是 Google 官方 **Material Design 3 (M3)** 的落地要点——做出**像 Android 原生**的界面。

## 1. 色彩系统（M3 Color Roles）

- 用 M3 的**语义色角色**而非硬编码 hex：`primary`(主要按钮/激活态)、`secondary`(次要)、`tertiary`(对比强调)，及其 `on-*`(其上文字)、`surface`/`background`/`error` 等。
- 从一个**种子色(seed)** 生成完整 tonal palette；支持 **Dynamic Color**（Material You：跟随用户壁纸取色，Android 12+）。
- 必须支持深色主题（M3 的 dark scheme），文字/容器对比达标。

## 2. 排版（Type Scale）

- 用 M3 **type scale**：`Display / Headline / Title / Body / Label`，各有 Large/Medium/Small；用角色而非写死字号。
- 默认字体 Roboto（中文思源黑体/系统中文），清晰层级。

## 3. 形状与高度（Shape & Elevation）

- 用 M3 **shape scale**（圆角等级：none/xs/s/m/l/xl，对应不同组件）。
- **Elevation**：用 z 轴高度 + tonal surface（M3 用表面色调变化表达层级，弱化阴影）传达层次；同类组件高度一致。

## 4. 动效（Motion）

- 用 M3 动效（标准缓动曲线、容器变换 container transform、共享轴 shared axis）引导用户、表达层级关系，**有意义不炫技**；尊重 reduce motion。

## 5. 组件（用 M3 组件库）

- 用 Material Components（Jetpack Compose Material3 / Views MDC）：`Button`(filled/tonal/outlined/text/elevated)、`TopAppBar`、`NavigationBar`(底部)/`NavigationRail`/`NavigationDrawer`、`FAB`、`Card`、`TextField`(outlined/filled)、`Chip`、`Snackbar`、`BottomSheet`、`Dialog` 等。
- 图标用 **Material Symbols**，不要用 emoji 当功能图标。
- **FAB** 用于页面主操作（Android 特有，别照搬到 iOS）。

## 6. 导航范式（Android 特有）

- 顶层切换用 **Navigation Bar**(底部，3–5 项) / 大屏用 Navigation Rail / Drawer。
- **顶部 App Bar** 承载标题与操作；**系统返回**（手势/返回键）必须正确处理（与 iOS 左上返回不同）。
- 大屏/平板/折叠屏用**自适应布局**（窗口尺寸类 compact/medium/expanded，列表-详情等 canonical layouts）。

## 7. 布局与适配

- 8dp 栅格基线；触控目标 **≥48×48dp**。
- 边到边(edge-to-edge) + 处理系统栏 insets；适配深色、字体缩放、不同密度(dpi)。
- 自适应不同屏幕尺寸/折叠态，不写死宽度。

## 8. 反模式（出现即不合格 / "不像 Android"）

- 不用 M3 色角色/type scale，硬编码颜色字号；不支持深色/Dynamic Color。
- 套 iOS 范式（顶部 tab、iOS 返回）忽略系统返回；把 FAB 乱用或缺主操作入口。
- 不适配大屏/折叠屏；忽略系统栏 insets；触控目标过小。
- emoji 当图标；自绘非 Material 控件且无无障碍。

## 9. 最低交付 checklist

- [ ] M3 色角色(seed→palette)+Dynamic Color+深色；type scale；shape/elevation 体系。
- [ ] 用 Material3 组件 + Material Symbols 图标(无 emoji)；FAB 用于主操作。
- [ ] 底部 Navigation Bar/Rail/Drawer + 顶部 App Bar + 正确系统返回。
- [ ] 8dp 栅格 + ≥48dp 触控 + edge-to-edge insets + 自适应大屏/折叠屏/密度/字体缩放。
- [ ] M3 有意义动效 + 三态 + 无障碍(TalkBack/对比度/reduce motion)。

---
**参考（官方）**：Material Design 3 (m3.material.io)、Material 3 in Compose、Dynamic Color/Material You、Material Symbols、自适应布局(window size classes)。
