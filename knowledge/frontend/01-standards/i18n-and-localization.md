---
id: i18n-and-localization
title: 国际化与本地化标准（i18n/l10n · 商业级）
domain: frontend
category: 01-standards
difficulty: intermediate
tags: [国际化, i18n, 本地化, l10n, 多语言, 翻译, locale, 时区, 货币, rtl, 复数, 商业级]
quality_score: 92
last_updated: 2026-06-19
---

# 国际化与本地化标准（i18n/l10n · 商业级）

> 面向多地区/多语言的商业产品必须从一开始就做 i18n——事后补改代价极大（文案散落代码、硬编码格式）。即使先只做一种语言，也要用 i18n 框架预留。

## 1. 文案外置（绝不硬编码）

- **所有用户可见文案走翻译 key**（`t('order.placed')`），绝不在 JSX/模板里写死字符串。
- 用成熟 i18n 库（react-i18next / vue-i18n / FormatJS / next-intl）；翻译资源按 locale 分文件管理。
- key 命名有层级语义（`auth.login.title`），便于维护与翻译协作。
- 翻译缺失有 fallback（回退默认语言）+ 可检测缺失 key。

## 2. 复数、性别、插值

- **复数**用框架的 plural 规则（`{{count}} item / items`），不要自己 `if count===1`——各语言复数规则不同。
- 变量插值用框架机制（`t('greeting', {{name}})`），不要字符串拼接（语序各语言不同）。
- 避免把句子拆成多段拼接（不同语言语序不同会拼错）。

## 3. 格式本地化（按 locale，不硬编码）

- **日期/时间**：用 `Intl.DateTimeFormat` 按 locale + 用户时区格式化；存储用 UTC，展示转本地时区。
- **数字/货币**：用 `Intl.NumberFormat`，货币带币种符号与正确分隔符；金额仍用最小单位整数存。
- **相对时间**（"3 小时前"）用 `Intl.RelativeTimeFormat`。
- 不要硬编码 `MM/DD/YYYY` 或 `$` 这类地区相关格式。

## 4. RTL 与布局

- 支持从右到左语言（阿拉伯/希伯来）时，用逻辑属性（`margin-inline-start` 而非 `margin-left`）、`dir` 属性，避免布局镜像出错。
- 文案长度因语言差异大（德语长、中文短），布局要弹性，别按某语言写死宽度。

## 5. 后端与内容

- 后端错误信息/邮件/通知也要可本地化（按用户 locale 选模板）。
- API 返回可本地化字段时，按 `Accept-Language`/用户偏好返回，或返回 key 让前端译。
- locale 检测：用户偏好 > URL/子域 > `Accept-Language` > 默认；用户可手动切换并持久化。
- SEO：多语言用 `hreflang`、独立 URL（路径或子域）。

## 6. 反模式（出现即不合格）

- 文案硬编码在代码里；事后才想做多语言。
- 自己写复数/性别逻辑；字符串拼接造句。
- 硬编码日期/货币/数字格式；展示用服务器时区而非用户时区。
- RTL 用物理方向属性导致镜像错乱；布局按单一语言长度写死。
- 后端邮件/错误不可本地化。

## 7. 最低交付 checklist

- [ ] 所有文案走 i18n key，用成熟库，资源按 locale 管理，缺失有 fallback。
- [ ] 复数/插值用框架机制，不拼接造句。
- [ ] 日期/数字/货币用 Intl 按 locale + 用户时区；存 UTC/最小单位。
- [ ] 需要时支持 RTL（逻辑属性）+ 弹性布局容纳文案长度差异。
- [ ] locale 检测+切换持久化；后端邮件/通知可本地化；多语言 SEO(hreflang)。

---
**参考**：ICU MessageFormat、`Intl` API、react-i18next/vue-i18n、CLDR 复数规则、RTL 逻辑属性、hreflang。
