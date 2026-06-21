---
id: cross-platform-frameworks
title: 跨平台框架选型与通用模式（商业级必读）
domain: cross-platform
category: 01-standards
difficulty: intermediate
tags: [跨平台框架, flutter, react-native, uni-app, taro, kotlin-multiplatform, kmp, maui, capacitor, ionic, tauri, electron, 条件编译, 原生桥, 商业级]
quality_score: 94
last_updated: 2026-06-19
---

# 跨平台框架选型与通用模式（商业级必读）

> 跨平台框架能"一套代码多端"，但每种适用场景、架构、坑都不同。选错框架或不懂其原生桥/性能模型，会做出割裂、卡顿、难维护的应用。本标准给出选型矩阵 + 通用模式。

## 1. 框架选型矩阵

| 框架 | 技术栈 | 覆盖端 | 渲染 | 适合 |
|---|---|---|---|---|
| **Flutter** | Dart | iOS/Android/Web/桌面/鸿蒙(社区) | 自绘引擎(Skia/Impeller) | 高性能、跨端一致 UI、动画复杂 |
| **React Native** | JS/TS(React) | iOS/Android(+桌面/web) | 映射原生组件 | 贴近 web 团队、原生体验、生态广 |
| **uni-app** | Vue | 各小程序+App+H5 | 各端原生/webview | 国内多小程序+App+H5 一套 |
| **Taro** | React/Vue | 各小程序+RN+H5 | 各端 | 国内多端、React 团队 |
| **Kotlin Multiplatform** | Kotlin | 共享业务逻辑(iOS/Android/桌面/web) | UI 各端原生(或 Compose MP) | 共享逻辑、UI 保留原生 |
| **.NET MAUI** | C# | iOS/Android/Win/mac | 原生 | .NET 团队、企业 |
| **Capacitor/Ionic** | Web | iOS/Android/Web(PWA) | WebView | web 应用快速套壳上架 |
| **Tauri/Electron** | Web(+Rust/Node) | 桌面(Tauri 也试验移动) | WebView | 桌面 |
| **Compose Multiplatform** | Kotlin | Android/iOS/桌面/web | 自绘 | Compose 团队跨端 UI |

选型原则：
- **跨端一致 UI + 高性能** → Flutter。
- **web/React 团队 + 原生体验** → React Native。
- **国内多小程序+App+H5** → uni-app / Taro。
- **共享逻辑、UI 要原生质感** → Kotlin Multiplatform（+ 各端原生 UI 或 Compose MP）。
- **已有大量 web、快速上架** → Capacitor。
- **企业 .NET** → MAUI。
- 别用 WebView 套壳硬做重交互/高性能场景；别为单端用跨平台框架徒增复杂度。

## 2. 通用架构（无论哪个框架）

- **共享业务逻辑/数据/契约，UI 适配平台**（见多端架构标准）。客户端仍分层：UI / 状态 / 数据访问(API) / 领域。
- 数据访问统一（typed client）；状态管理用框架推荐方案（Flutter: Riverpod/Bloc；RN: Redux Toolkit/Zustand+React Query；uni-app: Pinia/Vuex）。
- 按 feature 分包；公共能力抽包/模块。

## 3. 原生桥与平台差异（跨平台的核心难点）

- **原生能力**通过框架的桥接调用（Flutter Platform Channels / Method Channel、RN Native Modules/TurboModules、uni-app 原生插件）；找不到现成插件时要会写原生扩展。
- **平台差异处理**：
  - 用**条件编译/平台判断**隔离差异（uni-app `#ifdef`、Flutter `Platform.isIOS`、RN `Platform.OS`），把差异收敛到适配层，不要散落业务里。
  - 不要用某端独有 API 不做兼容/降级；功能不可用时优雅降级。
- **样式适配**：uni-app/小程序用 `rpx`、Flutter 用逻辑像素 + 自适应、RN 用 Flexbox + 尺寸适配；适配不同屏幕/刘海/安全区。

## 4. 性能（跨平台易踩的坑）

- **桥通信开销**：RN/小程序的 JS↔原生桥是瓶颈，减少跨桥频率与数据量（RN 新架构 JSI/Fabric 改善；小程序少 setData）。
- 长列表用框架高效列表（Flutter `ListView.builder`、RN `FlatList`/FlashList、uni-app 虚拟列表），不渲染全部。
- 图片/动画优化；避免主线程/JS 线程阻塞；Flutter 注意 build 方法轻量、避免不必要 rebuild。
- 包体：按需加载、分包、Tree-shaking、资源压缩。

## 5. 工程化与发布

- 一套代码多端构建：CI 分端打包；条件编译产物正确。
- 各端发布走各自商店/平台规范（见移动/小程序/桌面标准）；版本与热更新遵守平台政策。
- 跨端测试：核心逻辑单测共享；各端 e2e/真机验证（不能只测一端就发全端）。

## 6. 反模式（出现即不合格）

- 框架选型与场景不符（WebView 套壳做重交互、单端硬上跨平台）。
- 平台差异散落业务代码，不收敛到适配层；用独有 API 不降级。
- 不懂原生桥性能模型，频繁跨桥/大数据传输导致卡顿。
- 共享逻辑各端重复写；UI 强行一端范式套所有端（忽略各端设计规范）。
- 只测一端就发全端。

## 7. 最低交付 checklist

- [ ] 按场景选对框架并在架构文档说明理由。
- [ ] 共享业务/数据/契约 + UI 按平台；客户端分层 + feature 分包 + 统一数据访问。
- [ ] 原生能力经桥接/插件；平台差异收敛到适配层(条件编译) + 优雅降级。
- [ ] 性能：减少跨桥开销、长列表高效化、主线程不阻塞、包体优化。
- [ ] 各端按平台规范发布 + 多端真机测试，不只测一端。

---
**参考**：Flutter / React Native(新架构 Fabric/TurboModules) / uni-app / Taro / Kotlin Multiplatform / .NET MAUI / Capacitor 官方文档、平台桥接与条件编译、各端设计规范。
