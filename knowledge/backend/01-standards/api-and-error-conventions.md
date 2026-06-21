---
id: api-and-error-conventions
title: API 设计与错误处理规范（商业级后端必读）
domain: backend
category: 01-standards
difficulty: intermediate
tags: [api设计, rest, 错误处理, error, 状态码, 分页, pagination, 版本, versioning, 幂等, idempotency, dto, 契约, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# API 设计与错误处理规范（商业级后端必读）

> 框架无关的 HTTP API 硬性规范。每个商业级接口都要遵守：一致的资源命名、统一的错误信封、正确的状态码、分页/过滤/排序、版本与幂等。前端按契约对接，混乱的 API 会让整个项目腐化。

## 1. 资源与 URL 命名

- 资源用**名词复数**：`/api/v1/orders`、`/api/v1/orders/{id}/items`。不要用动词 URL（`/getOrders` ❌）。
- 层级表达从属：`/users/{id}/orders`；超过两层从属考虑顶层资源 + 过滤。
- 用 HTTP 方法表达动作：`GET`(查) `POST`(建) `PUT`(整体替换) `PATCH`(局部更新) `DELETE`(删)。
- 非 CRUD 的动作用子资源或动作端点：`POST /orders/{id}/cancel`（而非 `POST /cancelOrder`）。
- 全小写、连字符或直接复数名词；查询参数用 `snake_case` 或 `camelCase` 全项目统一。

## 2. 状态码（用对，不要一律 200）

| 场景 | 码 |
|---|---|
| 查询成功 / 更新成功 | 200 |
| 创建成功（返回新资源，带 `Location`）| 201 |
| 成功但无返回体（删除）| 204 |
| 参数/格式校验失败 | 422（或 400）|
| 未认证（没登录/token 失效）| 401 |
| 已认证但无权限 | 403 |
| 资源不存在 | 404 |
| 冲突（重复、版本冲突、状态不允许）| 409 |
| 限流 | 429 |
| 服务端异常 | 500（不泄露细节）|

- 创建返回 201 + 新资源 + `Location` 头；删除返回 204。
- 不要"业务失败也回 200 再在 body 里塞 `success:false`"——用正确状态码。

## 3. 统一错误信封（全项目一致）

所有错误返回同一结构，便于前端统一处理：

```json
{
  "error": {
    "code": "VALIDATION_ERROR",
    "message": "Request validation failed",
    "details": [
      { "field": "email", "message": "Must be a valid email address" }
    ],
    "request_id": "req_01H..."
  }
}
```

- `code`：机器可读的稳定错误码（`NOT_FOUND` / `FORBIDDEN` / `CONFLICT` / `VALIDATION_ERROR` / `RATE_LIMITED` / `INTERNAL`）。前端按 `code` 分支，不靠 message 文案。
- `message`：人类可读、安全（500 绝不暴露栈/SQL/内部路径）。
- `details`：字段级校验错误数组。
- `request_id`：贯穿日志，便于排查。
- 错误映射集中在接口层一个 error handler/filter，不在每个 controller 里重复 try/catch。

## 4. 请求与响应契约

- 入参：用 DTO + schema 校验（Zod/Joi/Pydantic/Bean Validation），失败 422 + 字段错误。
- 出参：只返回该接口需要的字段（Output DTO），**不要直接吐 ORM entity**（泄露内部字段、关系、敏感列）。
- 时间用 ISO-8601 UTC（`2026-06-19T08:00:00Z`）；金额用最小单位整数（分）或带币种的 decimal，**不要用 float**。
- 布尔/枚举值稳定；新增字段向后兼容，删字段要走版本。

## 5. 列表：分页 + 过滤 + 排序（列表接口必做）

- 分页两种主流：
  - **offset/limit**：`?limit=20&offset=40`，简单但深翻页慢。
  - **cursor**：`?limit=20&cursor=xxx`，大数据/无限滚动首选，稳定不漏不重。
- 返回分页元信息：`{ data: [...], page: { limit, next_cursor | total } }`。
- 过滤用明确参数（`?status=paid&created_after=...`），排序 `?sort=-created_at`（`-` 表降序）。
- 列表默认有上限（如 max limit 100），防止一次拉全表。

## 6. 版本与演进

- URL 版本前缀 `/api/v1/`（或 header 版本）。破坏性变更升大版本。
- 非破坏（加字段、加可选参数）不升版；破坏（删字段、改语义、改必填）必须升版并保留旧版过渡。
- 弃用用 `Deprecation`/`Sunset` 头提示。

## 7. 幂等与并发

- `POST` 创建类提供 **幂等键**（`Idempotency-Key` 头）防重复提交/重复扣款。
- 更新用乐观锁（`version`/`updated_at` 比对），冲突回 409，别静默覆盖。
- 钱、库存等关键写操作必须在事务内 + 幂等。

## 8. 鉴权与安全基线

- 受保护端点声明鉴权（Bearer JWT / Session）；状态变更端点（非公开的 POST/PUT/PATCH/DELETE）必须鉴权。
- 鉴权在中间件统一做；授权（这个用户能否操作这条资源）在服务层校验，别只靠路由。
- 永不在响应里回传密码哈希、token、内部 id 之外的敏感字段。
- 限流（按 IP/用户/端点）保护登录、发码、支付等。

## 9. 反模式（出现即不合格）

- 动词 URL、一律 200、错误结构每个接口不一样。
- 直接返回 ORM entity；500 把栈/SQL 暴露给客户端。
- 列表无分页/无上限、深翻页拉全表。
- 金额用 float；时间不带时区。
- 破坏性变更不升版本直接改线上契约。
- 创建/支付类接口无幂等，重复提交重复下单。

## 10. 最低交付 checklist

- [ ] 资源名词化 URL + 正确 HTTP 方法 + 正确状态码（201/204/401/403/404/409/422/429）。
- [ ] 全项目统一错误信封（code/message/details/request_id），错误映射集中处理。
- [ ] 入参 DTO+schema 校验；出参 DTO，不泄露 entity；时间 UTC、金额整数。
- [ ] 列表有分页+过滤+排序+默认上限。
- [ ] 版本前缀；破坏性变更升版。
- [ ] 关键写操作幂等 + 事务 + 乐观锁。
- [ ] 受保护端点鉴权 + 服务层授权 + 关键端点限流。

---
**参考**：REST 成熟度模型、RFC 7807 Problem Details、Stripe/GitHub API 设计惯例、12-Factor。
