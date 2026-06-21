---
id: desktop-design
title: 桌面端设计规范（macOS HIG / Windows Fluent · 官方）
domain: desktop
category: 01-standards
difficulty: intermediate
tags: [桌面, desktop, macos, windows, linux, 设计规范, hig, fluent-design, 菜单栏, 快捷键, 多窗口, 平台约定, 商业级]
quality_score: 92
last_updated: 2026-06-19
---

# 桌面端设计规范（macOS HIG / Windows Fluent · 官方）

> 桌面 App 各平台有用户已内化的约定，偏离就产生摩擦。纯底座常把桌面做成"放大的网页"，不遵循 macOS/Windows 原生约定。本标准给出各平台桌面设计要点。

## 1. 通用原则

- 桌面服务**高级用户**：期望深度键盘控制、持久菜单、可调整的多窗口布局、紧密系统集成。
- 用户通过反复使用形成心智模型——**遵循平台约定**（菜单、快捷键、按钮顺序），不要无理由自创。
- 信息密度比移动端高，但仍要清晰层次、留白、对齐。

## 2. macOS（遵循 Apple HIG for Mac）

- **必有菜单栏(Menu Bar)**——Mac 命令的主要发现入口。至少包含：**App / File / Edit / View / Window / Help**；非文档型应用可省 File；应用特有菜单放在 Edit 与 View 或 View 与 Window 之间。
- 标准快捷键：**Cmd** 为修饰键（Cmd+C/V/Z/S/W/Q…），与 Windows 的 Ctrl 不同——不要照搬。
- 工具栏(Toolbar)、侧边栏(Sidebar) 导航、可调分栏；窗口可缩放、记住尺寸位置；支持全屏、多窗口/标签。
- 视觉遵循 macOS：系统材质(毛玻璃)、SF 字体、系统控件、浅/深色；遵循 Clarity/Deference/Depth。
- 系统集成：Dock、菜单栏图标、通知中心、拖放、服务、快捷指令。

## 3. Windows（遵循 Microsoft Fluent Design System）

- 遵循 **Fluent Design**：光(Light)、深度、运动、材质(Acrylic/Mica)、缩放比例。
- **Ctrl** 为修饰键；标准菜单/快捷键；标题栏 + 可选 Ribbon/菜单；右键上下文菜单。
- 导航：NavigationView(左侧/顶部)、命令栏(CommandBar)；适配窗口大小/响应式。
- 用 WinUI/系统控件，支持浅/深色与系统强调色；DPI 缩放适配；触控+鼠标+键盘多输入。
- 系统集成：任务栏、系统托盘、通知、跳转列表、文件关联。

## 4. Linux

- 遵循目标桌面环境约定（GNOME HIG / KDE HIG）；用对应工具包(GTK/Qt)原生控件。
- 尊重系统主题/暗色、键盘约定；打包多发行版(AppImage/Flatpak/deb/rpm)。

## 5. 跨平台桌面（Electron/Tauri/Flutter 时）

- **不要一套 UI 硬套所有平台**：菜单/快捷键/按钮顺序按平台适配（Cmd vs Ctrl、确认/取消按钮顺序 mac 与 win 相反）。
- 用平台原生菜单 API（非自绘网页菜单）；窗口控件(红绿灯 vs 最小化/最大化/关闭)位置按平台。
- 至少让每个平台"感觉对"，而非在所有平台都像 web。

## 6. 通用桌面交互

- 键盘：完整快捷键 + Tab 焦点顺序 + 可访问性。
- 多窗口/标签、拖放、右键上下文菜单、撤销/重做、批量操作。
- 状态保持（窗口位置、上次会话）；空/加载/错误三态；长操作有进度可取消。

## 7. 反模式（出现即不合格 / "像放大的网页"）

- 桌面无原生菜单栏(尤其 macOS)、无快捷键、不能多窗口/调整大小。
- 一套 UI 套所有平台（Cmd/Ctrl 不分、按钮顺序不对、网页右键菜单）。
- 不遵循 macOS HIG / Windows Fluent 视觉与控件；不适配 DPI/暗色/系统强调色。
- 无拖放/上下文菜单/撤销重做等桌面用户期望；不记住窗口状态。

## 8. 最低交付 checklist

- [ ] macOS：原生菜单栏(App/File/Edit/View/Window/Help)+Cmd 快捷键+工具栏/侧边栏+窗口记忆+系统集成。
- [ ] Windows：Fluent 视觉/材质+Ctrl 快捷键+NavigationView/CommandBar+DPI/暗色/强调色+任务栏/托盘。
- [ ] Linux：遵循桌面环境 HIG + 原生工具包 + 多发行版打包。
- [ ] 跨平台：菜单/快捷键/按钮顺序按平台适配，用原生菜单 API。
- [ ] 键盘全可达 + 多窗口/拖放/上下文菜单/撤销重做 + 窗口状态保持 + 三态。

---
**参考（官方）**：Apple HIG (macOS)、Microsoft Fluent Design System、GNOME/KDE HIG、各平台键盘与窗口约定。
