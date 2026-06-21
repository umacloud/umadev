---
id: miniprogram-uniapp-ai-mistakes
title: 小程序 / uniapp · AI 高频错误避坑卡（错 -> 对）
domain: miniprogram
category: 01-standards
difficulty: intermediate
tags: [小程序, miniprogram, 微信, uniapp, uni-app, 踩坑, AI高频错误, setData, 合法域名, 条件编译, rpx, 生命周期, 商业级]
quality_score: 95
last_updated: 2026-06-20
---

# 小程序 / uniapp · AI 高频错误避坑卡

> AI 写小程序/uniapp 最容易把 **web 那套** 直接搬过来，但小程序是**双线程、无 DOM、域名白名单、体积受限**的环境。下面是实测高频错误的「错 -> 对」清单，写代码前逐条自检。配套参考见 `miniprogram-standard.md` 与 `cross-platform/01-standards/cross-platform-frameworks.md`。

## 一、微信小程序（原生）

1. **用 web API**：`document` / `window` / `localStorage` / `fetch` / `cookie`
   -> 小程序逻辑层无 DOM/BOM。存储用 `wx.setStorageSync`/`wx.getStorageSync`，请求用 `wx.request`，没有 fetch/axios（axios 需适配器）。
2. **`wx.request` 直接打域名**：真机报「不在以下 request 合法域名列表中」
   -> 域名必须在小程序管理后台「开发设置 -> 服务器域名」白名单，且只能 **https**。开发期可勾「不校验合法域名」，上线必须配。
3. **直接改 `this.data`**：`this.data.x = v` 视图不刷新
   -> 必须 `this.setData({ x: v })`。`setData` 是逻辑层->视图层唯一通道且**跨线程序列化**：别在循环/高频里调用，别一次塞大对象，局部更新用路径 `this.setData({ 'list[0].done': true })`。
4. **px + HTML/CSS 任意选择器**
   -> WXML/WXSS：尺寸用 **`rpx`**（`750rpx` = 屏宽）；选择器受限（**无 `*` 通配、后代选择器有限、不支持级联**），组件样式默认隔离。
5. **套 React/Vue 生命周期**：`componentDidMount` / `created`
   -> Page 用 `onLoad`(带参) / `onShow` / `onReady`；Component 用 `attached` / `ready`；页面通信用 `onLoad` 的 query + 全局 `getApp()`/`eventChannel`。
6. **tabBar 页用 `wx.navigateTo`**：跳不过去
   -> tabBar 页只能 `wx.switchTab`；普通页 `wx.navigateTo`（保留栈，**最多 10 层**），重定向 `wx.redirectTo`，返回 `wx.navigateBack`。
7. **`wx.getUserInfo` 拿头像昵称**：已废弃/拿不到
   -> 用 `wx.getUserProfile`（**必须用户点击事件里触发**，不能进页面自动调）。登录用 `wx.login` 拿 `code`，**后端**用 code 换 openid/session_key，别在前端解。
8. **直接 `npm install` 引包就用**
   -> 需「工具 -> 构建 npm」生成 `miniprogram_npm`；主包 **<= 2MB**，超了用**分包**（`subPackages`）+ 分包预下载。
9. **支付/手机号等能力不看资质**
   -> `wx.requestPayment`、手机号快速验证等需对应**类目/资质**，且支付需后端下单签名，AI 常漏后端这步。

## 二、uniapp（跨端：H5 + 各家小程序 + App）

1. **写平台专属/web 代码不做条件编译**：一处改动多端炸
   -> 平台差异用**条件编译**隔离：`#ifdef MP-WEIXIN … #endif`、`#ifdef H5`、`#ifdef APP-PLUS`、`#ifndef H5`。
2. **用 `wx.*` / `window` / `document`**：换端即失效
   -> 跨端统一用 **`uni.*`**（`uni.request` / `uni.navigateTo` / `uni.setStorageSync`）。DOM/BOM 在小程序与 App 端不存在。
3. **用 vue-router / `<router-link>`**
   -> 路由在 **`pages.json`** 声明（含 `tabBar`）；跳转 `uni.navigateTo` / `uni.switchTab`，不是 vue-router。
4. **复杂 template 表达式 / 渲染函数**
   -> 小程序端模板编译受限：逻辑放 `computed`/`methods`，少在模板里写复杂表达式；慎用 `render()`/JSX。
5. **appid、平台 key 硬编码进代码**
   -> 各平台配置进 **`manifest.json`**（每端独立 appid / SDK key）。
6. **`mounted` 里拿节点尺寸**：小程序端拿不到
   -> 用 `uni.createSelectorQuery().in(this)`；`mounted` 在小程序端时机不可靠，DOM 思路在此不成立。
7. **直接用浏览器专属库**（依赖 `document` 的图表/富文本）
   -> 选支持多端的库或按端条件编译；`rpx` 做自适应单位。

## 三、写完自检（小程序/uniapp 通用）
- [ ] 没有用 `document`/`window`/`fetch`/`localStorage`（改 `wx.*`/`uni.*`）
- [ ] 所有请求域名已规划进合法域名白名单、全 https
- [ ] 状态更新走 `setData`/响应式，无直接赋值；无高频/大对象 setData
- [ ] 尺寸用 `rpx`；选择器在小程序约束内
- [ ] tabBar 用 switchTab；导航不超 10 层
- [ ] 登录走 `wx.login` + 后端换 openid；敏感能力看资质、签名在后端
- [ ] uniapp 平台差异已条件编译；路由在 pages.json；平台配置在 manifest.json
