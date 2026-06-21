---
id: miniprogram-standard
title: 小程序开发标准（微信/支付宝/uni-app · 商业级）
domain: miniprogram
category: 01-standards
difficulty: intermediate
tags: [小程序, miniprogram, 微信, 支付宝, 双线程, setData, 分包, 性能, 生命周期, 组件, uni-app, taro, 审核, 商业级]
quality_score: 94
last_updated: 2026-06-19
---

# 小程序开发标准（微信/支付宝/抖音/uni-app · 商业级）

> 小程序是国内重要的商业触点（即用即走、依附超级 App 流量）。它的**双线程架构**与体积/性能约束决定了写法和 web 很不一样。本标准给出商业级小程序规范。

## 1. 双线程架构（理解它才能写好）

- 小程序分**逻辑层(JS 引擎)** 与 **视图层(WebView/渲染)**，两层通过 Native 桥**异步通信**（`evaluateJavascript`）。
- **`setData` 是逻辑层→视图层的唯一数据通道，也是性能瓶颈**：每次 setData 都跨线程序列化传输。优化 setData 是小程序性能的核心。
- 视图层不能直接操作 DOM/拿不到 JS 对象；交互事件从视图层异步回传逻辑层。

## 2. setData 性能（最关键）

- **只传变化的最小数据**：用数据路径 `this.setData({{'list[2].name': v}})`，不要整对象 setData。
- **合并/节流 setData**：避免在循环/高频事件(scroll/input)里频繁调用；合并成一次。
- **与渲染无关的数据不要放 `data`**（放普通变量/`this`），data 越大 setData 越慢。
- 避免一次 setData 传超大数组/长列表全量；长列表用分页 + 虚拟列表(recycle-view)。

## 3. 包体积与分包

- 主包有体积上限（微信主包 2MB，整体含分包 ≤ 20MB），**必须分包**：
  - 主包只放 启动页/TabBar 页/公共组件；其余功能拆 **分包**。
  - **分包预下载**（`preloadRule`）提升进入分包页的速度。
  - **独立分包**用于可脱离主包独立运行的活动页。
- 资源：图片用 CDN、压缩、懒加载；避免大图打进包。

## 4. 工程结构与组件化

- 按 feature/页面组织；公共逻辑抽工具/`behavior`(微信)/mixin；UI 抽**自定义组件**复用。
- 数据请求统一封装（request 拦截、鉴权、错误处理、loading），不在页面里散落 `wx.request`。
- 状态跨页共享用全局 store(如 mobx-miniprogram / 全局对象 / 本地缓存)，别滥用全局。
- 用 TypeScript + 规范的目录；用官方/成熟 UI 库(WeUI/Vant Weapp)对齐设计规范。

## 5. 生命周期与体验

- 正确用页面/组件/App 生命周期（`onLoad/onShow/onReady/onHide/onUnload`；组件 `attached/detached`）；onLoad 取参数、onShow 刷新。
- 首屏：骨架屏/loading；下拉刷新、触底加载、空态/错误态都要做。
- 分包加载、跳转有反馈；合理使用缓存(`Storage`)做秒开。

## 6. 平台能力与合规

- 登录用平台标准流程（微信 `wx.login` → code 换 openid/session，**在后端换**，不在前端暴露 secret）；用户信息/手机号按最新合规接口获取（需用户授权）。
- 支付用平台支付(微信支付/支付宝)，金额/订单后端为准 + 幂等（见支付标准）。
- 权限/隐私：调用敏感接口(定位/相机/通讯录)需 scope 授权 + 用途说明 + 隐私协议；遵守平台**审核规范**（类目资质、内容合规），否则审核拒绝。
- 域名必须 HTTPS 且在后台配置白名单（request/上传/下载/socket 合法域名）。

## 7. uni-app / Taro 跨端（一套多端）

- 要同时覆盖 多家小程序 + App + H5，用 **uni-app / Taro** 一套代码多端编译，省重复开发。
- 跨端注意：差量更新、条件编译处理平台差异、不要用某端独有 API 不做兼容、样式用 rpx 适配。
- uni-app 底层自动差量数据更新，仍要遵守"少 setData/data 精简"的原则。

## 8. 反模式（出现即不合格）

- 整对象/超大数据 setData；高频事件里频繁 setData；渲染无关数据塞 data。
- 不分包导致主包超限/启动慢；大图打进包。
- 页面里散落 `wx.request` 无统一封装；前端暴露 appsecret。
- 不处理空态/错误/弱网；登录/支付不走后端校验、无幂等。
- 调敏感接口不授权说明、不配合规域名白名单、忽略审核规范。

## 9. 最低交付 checklist

- [ ] setData 只传最小变化、路径更新、合并节流；渲染无关数据不入 data；长列表分页/虚拟。
- [ ] 分包(主包仅必要) + 预下载 + 图片 CDN/压缩/懒加载，主包<上限。
- [ ] 统一请求封装 + 自定义组件复用 + 合理全局状态 + TS + UI 库对齐设计。
- [ ] 生命周期正确 + 首屏骨架/loading + 三态 + 缓存秒开。
- [ ] 登录/支付后端校验+幂等；敏感接口授权说明+隐私协议+合规域名+过审核。
- [ ] 多端用 uni-app/Taro 条件编译 + rpx 适配。

---
**参考**：微信小程序官方性能优化/分包文档、setData 最佳实践、uni-app/Taro 跨端、平台审核规范、WeUI/Vant Weapp 设计。
