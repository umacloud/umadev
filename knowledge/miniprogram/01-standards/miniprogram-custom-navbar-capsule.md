---
id: miniprogram-custom-navbar-capsule
title: 小程序自定义导航栏 · 顶部适配与右上角胶囊（设计规范 · 必读）
domain: miniprogram
category: 01-standards
difficulty: intermediate
tags: [小程序, miniprogram, 微信, uniapp, 自定义导航栏, navigationStyle, custom, 胶囊, 顶部适配, 状态栏, statusBarHeight, getMenuButtonBoundingClientRect, 安全区, 商业级]
quality_score: 96
last_updated: 2026-06-20
---

# 小程序自定义导航栏 · 顶部适配与右上角胶囊

> **AI 写小程序自定义导航栏最高频的错误**：用了 `navigationStyle: "custom"` 自己画顶栏，却**没做顶部适配**——内容顶到状态栏下面、标题被状态栏盖住，或者把按钮放到了**右上角胶囊（微信常驻的那个药丸形按钮，含「更多」和「关闭」）底下点不到**。胶囊不可隐藏、位置随机型而变，必须**动态测量、给它让位**。本规范给出正确做法。

## 1. 什么时候要自己适配
- 页面或全局配置 `navigationStyle: "custom"` 后，**整个顶部区域归你画**，微信只保留**右上角胶囊**。
- 此时必须自己处理:**状态栏高度** + **导航栏高度** + **与胶囊垂直对齐** + **右侧给胶囊留空**。
- 不用 custom（默认导航栏）时不需要这套——但商业小程序为了品牌/沉浸式 banner 常用 custom。

## 2. 两个必测量值（绝不能写死）
不同机型(iOS/Android)、不同微信版本，状态栏高度和胶囊位置**都不一样**，必须运行时取:

```js
// 状态栏高度
const { statusBarHeight, safeArea } = wx.getWindowInfo(); // 新 API；旧版用 wx.getSystemInfoSync()
// 右上角胶囊的位置与尺寸（相对屏幕左上角，单位 px）
const cap = wx.getMenuButtonBoundingClientRect();
// cap = { top, right, bottom, left, width, height }
```

## 3. 正确的高度与对齐公式
导航栏内容应与胶囊**垂直居中对齐**，整条导航栏高度按胶囊推算:

```js
// 胶囊上边距状态栏的间隙（上下对称），推出导航栏内容区高度
const gap = cap.top - statusBarHeight;            // 胶囊到状态栏的间距
const navBarHeight = cap.height + gap * 2;        // 导航栏内容区高度（与胶囊上下等距）
const navTotalHeight = statusBarHeight + navBarHeight; // 顶栏总高（含状态栏）

this.setData({
  statusBarHeight,      // 顶部用它做 padding-top，避开状态栏
  navBarHeight,         // 导航栏内容区高度
  navTotalHeight,       // 占位高度，给页面内容做 margin-top
  capsuleRight: cap.right, // 右侧需要让出的宽度参考
});
```

WXSS（用 px，因为测量值是 px，别混 rpx）:
```css
.custom-nav { position: fixed; top: 0; left: 0; right: 0; z-index: 999; }
.custom-nav .status-bar { height: {{statusBarHeight}}px; }      /* 状态栏占位 */
.custom-nav .bar { height: {{navBarHeight}}px; display: flex; align-items: center; }
/* 标题/返回键放左侧；右侧到胶囊之间留空，绝不在胶囊下放可点元素 */
.page-body { margin-top: {{navTotalHeight}}px; }                /* 内容避开固定顶栏 */
```

## 4. 硬性规则（自检）
- [ ] 顶栏内容**从状态栏下方**开始（`padding-top: statusBarHeight`），标题不被状态栏盖。
- [ ] 导航栏高度/标题位置**与胶囊垂直居中对齐**（用公式，别写死 44px / 88rpx）。
- [ ] **右上角胶囊区域不放任何可点控件**；自定义按钮放左侧或胶囊左边，给胶囊让出宽度。
- [ ] 顶栏 `position: fixed`，页面内容 `margin-top: navTotalHeight` 避免被遮。
- [ ] 测量值是 **px**，导航栏这块用 px，不要和 rpx 混算。
- [ ] 底部同样做**安全区**适配:`safeArea` / `env(safe-area-inset-bottom)`（iPhone 底部小黑条）。
- [ ] 返回逻辑自理:custom 后没有系统返回键，首页判断 `getCurrentPages().length` 决定显示返回箭头还是首页图标。

## 5. uniapp 写法
- 取值:`uni.getMenuButtonBoundingClientRect()` + `uni.getWindowInfo()`（或 `uni.getSystemInfoSync()`），公式同上。
- uniapp 提供 CSS 变量 `--status-bar-height` 可直接用:`padding-top: var(--status-bar-height);`。
- 平台差异用条件编译:H5/App 没有胶囊，`#ifdef MP-WEIXIN` 才做胶囊对齐。
- 也可在 `pages.json` 单页设 `"navigationStyle": "custom"`。

## 6. 常见错法（别这么写）
- 写死 `navBarHeight = 44`（只在部分 iPhone 对，安卓/异形屏全错）。
- 用 `rpx` 套测量出来的 px 值（量纲错）。
- 自定义胶囊样按钮盖在真胶囊上（点不到/重叠）。
- 忘了页面内容 `margin-top`，首屏内容被固定顶栏吃掉一截。
