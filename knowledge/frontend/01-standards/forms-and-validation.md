---
id: forms-and-validation
title: 前端表单与校验标准（商业级必读）
domain: frontend
category: 01-standards
difficulty: intermediate
tags: [表单, form, 校验, validation, 受控, 错误提示, 提交状态, 多步, 可访问性, a11y, 乐观更新, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# 前端表单与校验标准（商业级必读）

> 表单是商业前端最复杂、最影响转化与体验的部分，也是 AI 最容易做得粗糙的地方（没校验、没错误态、没加载态、不可访问）。本标准给出商业级表单的完整要求。

## 1. 受控与状态

- 输入用**受控组件**（值绑定 state），或用成熟表单库（React Hook Form / Formik / VeeValidate）管理状态、校验、提交。
- 表单状态明确：`idle / validating / submitting / success / error`，UI 据此切换。
- 复杂表单用表单库 + schema 校验（Zod/Yup），不要手写一堆 useState + if。

## 2. 校验（双端，前端体验 + 后端兜底）

- **前端校验**给即时反馈（格式、必填、范围、一致性如两次密码），但**绝不替代后端校验**。
- **后端永远再校验一遍**（前端可绕过）；前端展示后端返回的字段级错误（对齐错误信封 `details[].field`）。
- 校验时机：失焦(blur)校验单字段 + 提交时校验全表单；不要每个按键就报红打断输入。
- 校验规则尽量与后端共享（同一份 schema/常量），避免前后端规则漂移。

## 3. 错误展示

- **字段级错误**显示在对应输入下方，文案具体可行动（"密码至少 8 位" 而非 "无效"）。
- 提交失败的**整体错误**（如 409 邮箱已注册、网络错误）在表单顶部/对应字段提示。
- 错误用颜色 + 图标 + 文本（不只靠颜色，照顾色盲）；`aria-invalid` + `aria-describedby` 关联错误。
- 后端 422 字段错误自动映射到对应输入。

## 4. 提交状态与防重复

- 提交中**禁用提交按钮 + loading 指示**，防重复提交。
- 创建/支付类提交配合后端**幂等键**防重复下单/扣款。
- 提交成功给明确反馈（跳转/toast/inline success）；失败保留用户已填内容，不清空。
- 网络慢时有反馈，不让用户以为卡死。

## 5. 体验细节

- 合理的 `type`（email/tel/number/password）、`autocomplete`、`inputmode`，移动端键盘正确。
- 必填标注清晰；占位符不当标签用（要有真 `<label>`）。
- 长表单分组/分步（multi-step），有进度指示，可返回上一步且保留数据。
- 危险操作（删除/不可逆）二次确认。
- 自动聚焦首个字段 / 出错时聚焦首个错误字段。

## 6. 可访问性（a11y，商业必做）

- 每个输入有关联 `<label for>`（或 aria-label）；不要只靠 placeholder。
- 错误用 `aria-invalid` + `role="alert"`/`aria-live` 让屏读播报。
- 键盘可完整操作；焦点顺序合理；提交可回车。
- 对比度达标；触控目标够大。

## 7. 反模式（出现即不合格）

- 没有任何校验，或只有前端校验（后端不验）。
- 只画 happy path：没有 loading/error/disabled 态，提交可重复点。
- 错误只变红框没有文案，或文案泛泛（"出错了"）。
- 用 placeholder 当 label；无 `<label>`；不可键盘操作。
- 提交失败清空用户输入；每个按键就报红。
- 密码/支付表单无强度/格式反馈、无防重复提交。

## 8. 最低交付 checklist

- [ ] 受控/表单库 + schema 校验；状态机 idle/submitting/success/error 清晰。
- [ ] 前端即时校验 + 后端兜底校验；422 字段错误映射到输入。
- [ ] 字段级 + 整体错误，文案具体可行动，颜色+图标+文本+aria。
- [ ] 提交中禁用+loading 防重复；创建/支付配合幂等；成功明确反馈、失败保留输入。
- [ ] 正确 input type/autocomplete；真 `<label>`；长表单分步保留数据；危险操作二次确认。
- [ ] a11y：label 关联、aria-invalid/live、键盘可达、对比度达标。

---
**参考**：React Hook Form / Zod、WCAG 表单可访问性、WAI-ARIA、HTML 表单最佳实践。
