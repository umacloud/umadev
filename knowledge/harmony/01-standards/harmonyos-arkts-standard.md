---
id: harmonyos-arkts-standard
title: 鸿蒙 HarmonyOS 开发标准（ArkTS/ArkUI/Stage · 商业级）
domain: harmony
category: 01-standards
difficulty: advanced
tags: [鸿蒙, harmonyos, harmonyos-next, arkts, arkui, stage模型, uiability, 声明式, 状态管理, 一次开发多端, 商业级]
quality_score: 94
last_updated: 2026-06-19
---

# 鸿蒙 HarmonyOS 开发标准（ArkTS/ArkUI/Stage · 商业级）

> HarmonyOS NEXT（纯血鸿蒙）**不兼容安卓 apk**，必须用 ArkTS/ArkUI 单独开发。国内面向 C 端的商业 App 应把鸿蒙作为一等端。本标准给出鸿蒙工程规范。

## 1. Stage 模型（应用架构基座）

- 用 **Stage 模型**（API 9+ 官方长期演进模型），不要用旧 FA 模型。
- 核心组件：
  - **UIAbility**：承载 UI 的应用组件，有明确生命周期（`onCreate / onWindowStageCreate / onForeground / onBackground / onDestroy`）——正确处理，别在错误时机做重活。
  - **AbilityStage**：HAP 的运行时容器；**WindowStage**：窗口管理。
  - **ExtensionAbility**：特定场景（卡片、输入法、后台服务等）。
- 模块化：HAP（入口/特性）/ HAR（静态共享库）/ HSP（动态共享包）合理拆分，按 feature 组织。
- `module.json5` / `app.json5` 配置规范；权限在配置声明 + 运行时申请。

## 2. ArkTS（语言规范）

- ArkTS 是 TS 超集但**更严格**：开启 strict、禁用部分动态特性；遵循官方命名与编码规范。
- 遵循官方**高性能编码规则**：优先 `const`、用 TypedArray/HashMap/HashSet 等高效容器、按需 `lazy import`、避免不必要的对象创建与深拷贝、减少跨线程序列化。
- 并发用 **TaskPool / Worker** 把耗时计算放后台，别阻塞 UI 线程。

## 3. ArkUI（声明式 UI）

- **声明式开发范式**：`@Entry @Component struct` + `build()`，数据驱动 UI 自动刷新。
- **状态管理**：用状态装饰器（V1 的 `@State/@Prop/@Link/@Provide/@Consume/@Observed/@ObjectLink`，或 V2 的 `@ComponentV2/@Local/@Param/@Event/@Monitor`）；状态最小化、就近管理、避免大对象触发全量刷新。
- 组件化复用 + 自定义组件；用 `@Builder/@BuilderParam` 抽公共 UI；`@Styles/@Extend` 复用样式。
- 长列表用 **LazyForEach + 数据懒加载**（不要 ForEach 渲染全部），配合组件复用、缓存。
- 遵循 **HarmonyOS Design** 设计规范（鸿蒙视觉/交互/动效语言），用系统组件与设计 token，不要硬套安卓/iOS 范式。

## 4. 一次开发多端部署（如需）

- 用**自适应/响应式布局**（栅格、断点、`@ohos.mediaquery`）适配手机/折叠屏/平板/PC/车机。
- 分层 + 一多能力，UI 随设备形态自适应；公共逻辑共享。

## 5. 数据、网络与存储

- 网络用 `@ohos.net.http` / RCP；统一封装 + 超时重试 + 弱网处理。
- 本地存储：首选项 `Preferences`(轻量)、关系型 `RelationalStore`(结构化)、分布式数据(跨设备)。敏感数据用加密能力，**不明文存密钥/token**。
- 分布式特性（流转、跨设备协同）是鸿蒙差异化能力，按场景使用。

## 6. 性能与发布

- 启动优化、首帧优化；避免主线程耗时；用 DevEco Profiler 定位卡顿/丢帧。
- 包体优化（HSP 动态共享、资源压缩、按需加载）。
- 上架华为应用市场：遵守审核规范、隐私清单、权限说明、个保法合规；崩溃监控(AppGallery Connect)。

## 7. 反模式（出现即不合格）

- 用旧 FA 模型；UIAbility 生命周期乱用（前台做重活、不释放资源）。
- ArkTS 写成松散 JS（用动态特性、不遵循高性能规则）；耗时计算阻塞 UI 线程。
- 状态管理用大对象全量刷新；长列表 ForEach 渲染全部不复用。
- 套用安卓/iOS UI 范式而非 HarmonyOS Design。
- 明文存敏感数据；权限不声明/不说明。

## 8. 最低交付 checklist

- [ ] Stage 模型 + UIAbility 生命周期正确 + HAP/HAR/HSP 模块化。
- [ ] ArkTS strict + 高性能编码规则 + TaskPool/Worker 后台并发。
- [ ] ArkUI 声明式 + 状态最小化就近管理 + LazyForEach 长列表 + 组件/样式复用。
- [ ] 遵循 HarmonyOS Design；需多端则自适应布局一多部署。
- [ ] 网络统一封装+弱网处理；本地/敏感数据安全存储。
- [ ] 性能 Profiler 调优 + 包体优化 + 应用市场合规 + 崩溃监控。

---
**参考**：HarmonyOS NEXT 官方文档、Stage 模型、ArkTS 高性能编码规范、ArkUI 状态管理(V1/V2)、HarmonyOS Design、一次开发多端部署。
