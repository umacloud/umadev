---
id: ios-design-hig
title: iOS 设计规范（Apple Human Interface Guidelines · 官方）
domain: mobile
category: 01-standards
difficulty: intermediate
tags: [ios, 设计规范, hig, human-interface-guidelines, apple, sf字体, dynamic-type, 导航, tabbar, 安全区, 深色模式, sf-symbols, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# iOS 设计规范（Apple Human Interface Guidelines · 官方）

> 纯 Claude/Codex 写 iOS UI 常常"web 味"很重、不像原生。本标准是 Apple 官方 HIG 的落地要点——做出**像 iOS 原生**的界面。设计 iOS 应用前必读。

## 1. 四大核心原则（HIG）

- **Clarity 清晰**：内容优先，文字清晰可读，图标精准，留白充分。
- **Deference 谦让**：UI 让位于内容，不喧宾夺主；用半透明/模糊衬托而非抢戏。
- **Depth 层次**：用真实的层级与过渡传达结构（卡片、模态滑入、层叠）。
- **Consistency 一致**：用系统标准组件与交互，符合用户跨 Apple 生态的既有习惯。

## 2. 排版（Typography）

- 用系统字体 **San Francisco (SF Pro)**：正文 SF Pro Text（≤19pt），大标题 SF Pro Display（≥20pt）；中文用苹方(PingFang SC)。
- **必须支持 Dynamic Type**（用户可调字号，无障碍）——用文本样式(Large Title/Title/Body/Caption…)而非写死字号，布局随字号弹性。
- 建立清晰的字阶层级（Large Title → Title → Headline → Body → Footnote）。

## 3. 颜色与材质

- 用**语义系统颜色**（`label`/`secondaryLabel`/`systemBackground`/`systemBlue`…）而非硬编码，自动适配深浅色。
- 一个克制的品牌强调色 + 系统色；不要满屏高饱和。
- 善用材质(Materials/毛玻璃)做层次；遵循系统的浅色/深色双模式。

## 4. 布局与适配

- 内容尊重**安全区(Safe Area)**：避开刘海/灵动岛/Home Indicator/状态栏。
- 标准边距与对齐；触控目标 **≥44×44pt**。
- 适配各机型尺寸、横竖屏、iPad(分屏/多窗)；用 Auto Layout/SwiftUI 自适应，不写死像素。
- 尊重 reduce motion / increase contrast 等无障碍设置。

## 5. 导航范式（iOS 特有，别套 web）

- **Tab Bar**：底部，切换 App 顶层模块，iPhone 上**最多 5 个**。
- **Navigation Bar**：层级钻取(drill-down)，左上返回，标题居中/大标题。
- **Modal Sheet**：聚焦单一任务（新建/编辑），可下拉关闭；破坏性确认用 Action Sheet/Alert。
- 手势：边缘左滑返回、列表左滑操作、下拉刷新——符合系统习惯。

## 6. 组件（用系统标准件）

- 优先用系统组件（按钮、列表 List、表单、分段控件、开关、Picker、搜索栏、上下文菜单），自带正确外观/交互/无障碍。
- 图标用 **SF Symbols**（与系统一致、随字重/字号缩放、支持多色），**不要用 emoji 当功能图标**。
- 列表用 inset grouped 等系统样式；空态/加载态/错误态都要做。

## 7. 体验细节

- 即时反馈：点击态、加载指示、触感反馈(Haptics)。
- 流畅自然的转场动画（遵循系统时长/曲线），不要花哨突兀。
- 首次启动/权限请求有上下文说明；尊重用户选择。

## 8. 反模式（出现即不合格 / "不像 iOS"）

- 把 web/安卓的 UI 范式硬搬到 iOS（如安卓 FAB、底部抽屉当主导航乱用）。
- 写死字号不支持 Dynamic Type；硬编码颜色不适配深色。
- 忽略安全区导致内容被刘海/Home 条遮挡；触控目标过小。
- 用 emoji 当图标而非 SF Symbols；自绘一堆非标准控件却没做无障碍。
- Tab 超过 5 个；模态/导航滥用，不符系统手势。

## 9. 最低交付 checklist

- [ ] 遵循 Clarity/Deference/Depth/Consistency；用系统标准组件。
- [ ] SF/苹方字体 + Dynamic Type + 清晰字阶；语义系统颜色 + 深色适配。
- [ ] 安全区适配 + ≥44pt 触控 + 多机型/横竖屏/iPad 自适应。
- [ ] 导航用 Tab(≤5)/Nav Bar/Modal + 系统手势；SF Symbols 图标无 emoji。
- [ ] 反馈/Haptics/自然转场 + 三态 + 无障碍(VoiceOver/对比度/reduce motion)。

---
**参考（官方）**：Apple Human Interface Guidelines (developer.apple.com/design/human-interface-guidelines)、SF Symbols、Dynamic Type、Safe Area、SwiftUI。
