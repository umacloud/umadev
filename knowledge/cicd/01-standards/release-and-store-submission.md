---
id: release-and-store-submission
title: 发布与上架审核标准（多端 · 商业级）
domain: cicd
category: 01-standards
difficulty: intermediate
tags: [发布, 上架, 审核, app-store, google-play, 华为, 微信小程序, 公证, 隐私, 合规, 提审, 发布, 商业级]
quality_score: 92
last_updated: 2026-06-19
---

# 发布与上架审核标准（多端 · 商业级）

> "做完"不等于"能上架"。各平台审核严格，缺隐私声明、权限说明、资质就被拒。本标准给出各端上架前的硬性 checklist，避免反复被拒。

## 1. 通用（所有端）

- **隐私政策 + 用户协议** 可访问；明确收集了什么数据、用途、第三方 SDK。
- 权限**按需 + 说明用途**；删除未使用的权限/SDK（多余权限是被拒常见原因）。
- 内容合规（无违规/侵权/敏感内容）；版本号/构建号规范；崩溃率达标。
- 各环境配置正确（生产 API、密钥、关闭调试日志）。

## 2. iOS / App Store

- **App Privacy（隐私营养标签）** 如实填写数据收集；如有跟踪需 **ATT (App Tracking Transparency)** 弹窗。
- 权限在 `Info.plist` 配 **用途说明文案(Usage Description)**，缺了直接崩/拒。
- 不下发可执行代码/不绕过审核；用 IAP 卖数字内容（不能用三方支付绕苹果分成）。
- 截图/预览/描述合规；测试账号给审核；支持最新系统与机型；适配深色/动态字体。
- 证书/描述文件/签名正确；构建用正式证书。

## 3. Android / Google Play & 华为应用市场

- **数据安全表单(Data Safety)** 如实填；目标 API level 满足最新要求；64 位支持。
- 权限敏感(定位/短信/通讯录等)需说明与合理性；前台服务/精确闹钟等受限权限需符合政策。
- 用 **App Bundle (.aab)** 上传；签名(Play App Signing)；混淆/资源压缩。
- 华为应用市场：额外资质(类目)、隐私、HMS(如用推送)适配；鸿蒙单独提审。

## 4. 微信小程序 / 各小程序

- 类目**资质**齐全（如电商/医疗/金融需对应资质）；服务器域名 **HTTPS + 白名单**配置。
- 隐私协议 + 用户授权弹窗合规；遵守诱导分享/虚拟支付等运营规范。
- 体验评分(性能/可用性)达标；提审填写完整、测试账号、功能页路径。
- 包大小符合限制(主包/分包)。

## 5. 桌面应用

- **代码签名**：Windows 代码签名证书；**macOS 签名 + 公证(notarization)** + Gatekeeper 通过（不签名用户装不上/报毒）。
- Linux 多发行版打包(AppImage/Flatpak/deb/rpm)。
- 自动更新签名校验；安装包来源可信(HTTPS)。

## 6. Web

- 生产构建(压缩/Tree-shaking/source map 处理)；环境变量/密钥正确不泄漏。
- HTTPS + 安全头(CSP/HSTS)；SEO 基础(sitemap/robots/meta)；监控与告警接好。
- 回滚预案；灰度/蓝绿；健康检查。

## 7. 反模式（出现即不合格）

- 缺隐私政策/权限说明/数据安全表单 → 必被拒。
- iOS 缺 Usage Description（崩溃）/用三方支付卖数字内容/有跟踪不弹 ATT。
- Android 不用 aab/目标 API 过低/敏感权限无理由。
- 小程序无资质/域名未配白名单/诱导分享。
- 桌面不签名不公证；生产泄漏密钥/开调试。

## 8. 最低交付 checklist（提审前）

- [ ] 通用：隐私政策+协议、权限按需+说明、删多余权限/SDK、内容合规、生产配置正确。
- [ ] iOS：App Privacy + ATT(如需) + Info.plist 用途说明 + IAP + 截图/测试账号 + 正式签名。
- [ ] Android：Data Safety + 目标 API + aab 签名 + 敏感权限说明；华为/鸿蒙单独资质提审。
- [ ] 小程序：类目资质 + 域名白名单 + 隐私授权合规 + 体验达标 + 包大小。
- [ ] 桌面：Win 签名 + macOS 签名公证 + 多发行版打包 + 更新签名校验。
- [ ] Web：生产构建 + 安全头 + SEO + 监控告警 + 回滚/灰度 + 健康检查。

---
**参考（官方）**：App Store Review Guidelines、Google Play 政策、华为应用市场审核、微信小程序审核规范、macOS 公证、Google Search Central。
