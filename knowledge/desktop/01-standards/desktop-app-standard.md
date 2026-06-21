---
id: desktop-app-standard
title: 桌面应用开发标准（Windows/macOS/Linux · 商业级）
domain: desktop
category: 01-standards
difficulty: intermediate
tags: [桌面, desktop, electron, tauri, windows, macos, linux, 主进程, 渲染进程, ipc, 自动更新, 代码签名, 系统集成, 商业级]
quality_score: 93
last_updated: 2026-06-19
---

# 桌面应用开发标准（Windows/macOS/Linux · 商业级）

> 桌面 App 要做好进程架构、安全、系统集成、打包签名与多平台差异。"把网页用 Electron 一包"远不够商业级。

## 1. 技术选型

- **Tauri**（Rust 后端 + 系统 WebView）：包小(几 MB)、内存省、默认更安全。**新项目推荐**。
- **Electron**（Chromium + Node）：生态成熟、一致渲染，但包大(~100MB+)、内存高。需要 Node 生态/特定 Chromium 特性时用。
- **原生 / 跨平台原生**：重性能/深度系统集成 → 原生(SwiftUI/AppKit、WinUI/WPF)、或 Qt/.NET MAUI/Flutter Desktop。
- 不要为一个简单工具上 Electron 拖出 100MB+ 包；也不要用 web 技术硬做重图形/底层场景。

## 2. 进程架构与 IPC

- **主进程**（系统能力/窗口/生命周期）与**渲染进程/前端**（UI）分离；通过 **IPC** 通信。
- 渲染层不直接拿系统全权限；系统能力封装在主进程，渲染层经受控 IPC 调用（最小暴露面）。
- 前端本身仍按前端架构分层（feature/状态/数据访问），系统调用走"桌面能力适配层"。

## 3. 安全（桌面是高权限环境，极重要）

- **Electron**：`contextIsolation: true`、`nodeIntegration: false`、用 `preload` + `contextBridge` 暴露**最小** API；开启 CSP；校验/限制 `webContents` 导航与新窗口；不加载不可信远程内容到有 Node 权限的窗口。
- **Tauri**：用 **allowlist/capabilities** 最小授权，只开用到的系统能力；校验前端→后端命令参数。
- 不在渲染层执行不可信代码；IPC 入参当不可信处理(校验)；密钥不放前端。
- 自动更新走 **HTTPS + 签名校验**，防中间人投毒。

## 4. 系统集成（桌面体验关键）

- 原生菜单（macOS 顶部菜单栏 / Windows 菜单）、托盘(Tray)、系统通知、全局快捷键。
- 文件系统：打开/保存对话框、拖拽文件、最近文件、关联文件类型。
- 窗口管理：多窗口、最小化到托盘、记住窗口位置/尺寸、多显示器。
- 遵循各平台 UI 约定（macOS 与 Windows 的按钮顺序、菜单、快捷键 Cmd vs Ctrl 不同）。

## 5. 打包、签名、分发

- 多平台产物：Windows(`.exe/.msi`)、macOS(`.dmg/.pkg`)、Linux(`.AppImage/.deb/.rpm`)。
- **代码签名**：Windows 代码签名证书、**macOS 签名 + 公证(notarization)**——不签名会被系统拦截/警告，商业必做。
- **自动更新**（electron-updater / Tauri updater）：增量/全量、回滚、签名校验、灰度。
- 包体优化、启动速度；崩溃监控(Sentry)。

## 6. 离线与数据

- 桌面常需离线：本地数据库(SQLite)、本地配置、缓存；联网同步 + 冲突解决。
- 用户数据存平台标准目录（AppData / Application Support / .config），不要乱放。

## 7. 反模式（出现即不合格）

- Electron 开 `nodeIntegration` + 关 `contextIsolation` + 加载远程内容（远程代码可拿系统权限，重大漏洞）。
- 渲染层直接全权访问系统/文件/shell；IPC 不校验入参。
- 不做代码签名/公证（用户装不上或报毒）；自动更新无签名校验。
- 用 web 范式忽略原生菜单/快捷键/多平台差异(Cmd vs Ctrl)。
- 简单工具硬上 Electron 拖出超大包。

## 8. 最低交付 checklist

- [ ] 选型合理(优先 Tauri/按需 Electron/原生)；主进程与渲染分离 + 最小 IPC。
- [ ] 安全：Electron(contextIsolation/nodeIntegration off/preload+contextBridge/CSP) 或 Tauri(allowlist 最小授权)；IPC 入参校验。
- [ ] 系统集成：原生菜单/托盘/通知/快捷键/文件对话框/窗口记忆/多显示器，遵循各平台约定。
- [ ] 多平台打包 + 代码签名 + macOS 公证 + 自动更新(签名校验/回滚)。
- [ ] 离线数据本地库 + 标准用户目录 + 同步冲突处理 + 崩溃监控。

---
**参考**：Tauri 安全(allowlist/capabilities)、Electron 安全清单(contextIsolation/preload)、代码签名与公证、各平台桌面 UI 约定、自动更新。
