---
id: mobile-app-standard
title: 移动 App 开发标准（iOS/Android/跨平台 · 商业级）
domain: mobile
category: 01-standards
difficulty: intermediate
tags: [移动, mobile, ios, android, swiftui, jetpack compose, flutter, react-native, mvvm, 导航, 离线, 推送, hig, material, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# 移动 App 开发标准（iOS/Android/跨平台 · 商业级）

> 移动 App 不是"把网页塞进手机"。要遵循平台设计规范、处理生命周期/离线/弱网、做好性能与发布。本标准覆盖原生(SwiftUI/Compose)与跨平台(Flutter/RN)。

## 1. 架构（客户端也要分层）

- 用 **MVVM / MVI** 等单向数据流架构：View(声明式 UI) ↔ ViewModel/State(状态+逻辑) ↔ Repository(数据访问) ↔ 领域/网络/本地存储。
- UI 与业务逻辑分离；ViewModel 不持有 UI 引用、可测试。
- 数据访问统一 Repository：网络(API)、本地(DB/缓存)在此封装，UI 不直接调网络。
- 依赖注入装配；按 feature 模块组织。
- 声明式 UI（SwiftUI / Jetpack Compose / Flutter / RN）优先，注意避免不必要重组/重建。

## 2. 平台设计规范（UI/交互按平台，别用 web 范式）

- **iOS**：遵循 **Human Interface Guidelines (HIG)**——原生导航(NavigationStack)、SF Symbols 图标、原生手势、安全区(notch/灵动岛)、动效自然。
- **Android**：遵循 **Material Design 3**——Material 组件、返回手势、自适应布局、动态取色(Material You)。
- 尊重各平台导航范式（iOS 顶部返回 + tab；Android 系统返回）；不要把一端的导航硬搬另一端。
- 适配多屏尺寸、刘海/挖孔、深色模式、字体缩放(动态字体/无障碍)、横竖屏。

## 3. 生命周期与状态

- 正确处理 App/页面生命周期（前后台切换、被系统回收恢复、配置变更如旋转）。
- 状态保存与恢复：进程被杀后能恢复关键状态。
- 后台限制：遵守平台后台执行规则，长任务用平台机制(WorkManager/BGTaskScheduler)。

## 4. 网络、离线与弱网

- 统一网络层 + 超时/重试；处理弱网/无网，给明确反馈而非卡死。
- **离线优先**（适用场景）：本地缓存/数据库(SQLite/Room/CoreData/Realm)，离线可读，联网同步；冲突有解决策略。
- 列表分页 + 下拉刷新 + 上拉加载；图片懒加载 + 缓存 + 占位。
- 乐观更新提升体感，失败回滚。

## 5. 性能

- 列表用平台高效列表（LazyColumn/List/FlatList/ListView.builder）+ 回收复用，不一次渲染全部。
- 图片：合适尺寸/格式、缓存、避免主线程解码大图。
- 主线程不做重计算/IO（避免掉帧/ANR）；耗时放后台线程/协程/isolate。
- 控制重组/重渲染（Compose 稳定性、SwiftUI 最小化 body、RN memo/FlatList 优化）。
- 启动速度、包体积（按需加载、资源压缩、Android App Bundle）。

## 6. 原生能力与权限

- 权限**按需申请 + 说明用途**（相机/定位/通知/相册），尊重拒绝，提供降级。
- 推送：iOS APNs / Android FCM / 鸿蒙 Push；处理 token、点击跳转、前后台展示。
- 安全存储敏感数据用 Keychain(iOS)/Keystore+EncryptedSharedPrefs(Android)，**不用明文 SharedPreferences/UserDefaults 存 token**。
- 深链接(Universal Links/App Links)、分享、支付(IAP/三方)按平台规范。

## 7. 发布与合规

- 商店审核：iOS App Store / Google Play / 华为应用市场各有规范（隐私清单、权限说明、内容合规）。
- 版本/热更新：遵守平台政策（iOS 禁止下发可执行代码）；崩溃监控(Crashlytics/Sentry)。
- 隐私合规：隐私政策、数据收集声明(App Privacy)、个保法/GDPR。

## 8. 反模式（出现即不合格）

- 用 web 的 UI/交互范式套移动端；忽略 HIG/Material。
- UI 直接调网络、无 Repository/分层；ViewModel 持有 View。
- 不处理生命周期/进程回收/弱网；列表一次渲染全部。
- 主线程做重 IO/计算导致卡顿/ANR。
- token 明文存 UserDefaults/SharedPreferences；权限一次性全要不说明。
- 不做崩溃监控、不适配深色/多尺寸/动态字体。

## 9. 最低交付 checklist

- [ ] MVVM/MVI 分层 + Repository 数据访问 + 声明式 UI + feature 模块。
- [ ] 遵循 iOS HIG / Android Material；导航按平台；适配多屏/深色/动态字体/安全区。
- [ ] 正确生命周期 + 状态保存恢复 + 后台规则。
- [ ] 统一网络层 + 弱网/离线处理 + 列表分页/缓存 + 乐观更新。
- [ ] 列表复用 + 图片优化 + 主线程不阻塞 + 控制重组 + 包体优化。
- [ ] 权限按需+说明、敏感数据安全存储、推送、深链、IAP 按规范。
- [ ] 商店合规 + 崩溃监控 + 隐私声明。

---
**参考**：Apple HIG、Material Design 3、SwiftUI/Jetpack Compose、Flutter/React Native 最佳实践、离线优先、移动安全存储。
