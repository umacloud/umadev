---
id: api-design
title: Architect — RESTful API Design Standards
domain: experts
category: architect
difficulty: intermediate
tags: [api, authentication, cors, design, experts, filtering, limiting, pagination]
quality_score: 70
last_updated: 2026-06-15
---
# Architect — RESTful API Design Standards

## 架构文档必须明确"结构决策"（落到后端实现）

架构文档不能只画框图，必须明确写出**让后端照着建的结构决策**，否则下游会写成扁平烂代码：

- **分层模型**：声明采用分层/Clean 架构——接口层(controller)→应用层(service，编排+事务)→领域层(entity/VO，业务规则)→基础设施层(repository/adapter)，依赖向内。
- **模块/分包划分**：按业务域（限界上下文）列出 feature 模块（如 orders / payments / users / auth），每个模块内部按层组织；跨模块通过服务接口/领域事件通信。**优先 package-by-feature**。
- **服务层边界**：每个用例对应一个服务方法=一个事务边界；收发 DTO 不泄露 ORM entity。
- **数据模型**：实体+字段+关系+关键约束/索引；金额非 float、时间带时区。
- **API 契约**：资源、方法、状态码、统一错误信封、鉴权方案（哪些端点需鉴权）。
- **技术栈选型 + 理由**：框架/DB/缓存/队列，并说明为何。
- 详见 `backend/01-standards/application-layering-and-packaging`、`api-and-error-conventions`、`data-modeling-and-persistence`。

## URL Design

### Naming Conventions
- Use nouns, not verbs: `/users` not `/getUsers`
- Plural for collections: `/users`, `/posts`, `/comments`
- Nested resources for relationships: `/users/{id}/posts`
- Max 2 levels of nesting: `/users/{id}/posts` (OK), `/users/{id}/posts/{id}/comments/{id}/likes` (too deep → flatten)
- Kebab-case for multi-word: `/user-profiles` not `/userProfiles`
- No trailing slashes: `/users` not `/users/`

### HTTP Methods
| Method | Use | Idempotent | Safe | Example |
|---|---|---|---|---|
| GET | Read | Yes | Yes | `GET /users/123` |
| POST | Create | No | No | `POST /users` |
| PUT | Full replace | Yes | No | `PUT /users/123` |
| PATCH | Partial update | Yes | No | `PATCH /users/123` |
| DELETE | Remove | Yes | No | `DELETE /users/123` |

### Versioning
- URL prefix: `/api/v1/users`
- Not headers (harder to test, cache, share)
- Increment on breaking changes only

## Request/Response Standards

### Request Body
```json
{
  "email": "user@example.com",
  "name": "Jane Doe",
  "role": "admin"
}
```
- camelCase for JSON fields
- Validate ALL fields server-side (never trust client)
- Return 422 for validation errors with field-level details

### Success Response
```json
{
  "data": { ... },
  "meta": {
    "requestId": "req_abc123",
    "timestamp": "2026-01-15T10:30:00Z"
  }
}
```

### Error Response
```json
{
  "error": {
    "code": "VALIDATION_ERROR",
    "message": "Invalid input",
    "details": [
      { "field": "email", "message": "Invalid email format" }
    ],
    "requestId": "req_abc123"
  }
}
```

### Status Codes
| Code | When | Response body |
|---|---|---|
| 200 | Success (with data) | `{ "data": ... }` |
| 201 | Created | `{ "data": newResource }` with `Location` header |
| 204 | Success (no content) | empty body (DELETE, some PUTs) |
| 400 | Malformed request | `{ "error": { "code": "BAD_REQUEST" } }` |
| 401 | Not authenticated | `{ "error": { "code": "UNAUTHORIZED" } }` |
| 403 | Authenticated but forbidden | `{ "error": { "code": "FORBIDDEN" } }` |
| 404 | Resource not found | `{ "error": { "code": "NOT_FOUND" } }` |
| 409 | Conflict (duplicate) | `{ "error": { "code": "CONFLICT" } }` |
| 422 | Validation error | `{ "error": { "code": "VALIDATION_ERROR", "details": [...] } }` |
| 429 | Rate limited | `{ "error": { "code": "RATE_LIMITED" } }` + `Retry-After` header |
| 500 | Server error | `{ "error": { "code": "INTERNAL_ERROR" } }` (no internal details!) |

## Pagination

### Cursor-based (recommended)
```
GET /posts?cursor=abc123&limit=20
```
Response:
```json
{
  "data": [...],
  "pagination": {
    "nextCursor": "def456",
    "hasMore": true,
    "limit": 20
  }
}
```

### Offset-based (simpler but less performant)
```
GET /posts?page=2&limit=20
```
Response:
```json
{
  "data": [...],
  "pagination": {
    "page": 2,
    "limit": 20,
    "total": 156,
    "totalPages": 8
  }
}
```

## Filtering & Sorting

```
GET /posts?status=published&author=123&sort=-createdAt&fields=id,title
```
- Filter by field: `?status=published`
- Multiple values: `?status=published,draft`
- Sort: `?sort=createdAt` (asc), `?sort=-createdAt` (desc)
- Field selection: `?fields=id,title,author`

## Authentication

### JWT Best Practices
- Short-lived access tokens (15 min)
- Long-lived refresh tokens (7 days, stored httpOnly cookie)
- Rotate refresh tokens on use (one-time use)
- Include minimal claims: `{ sub, role, iat, exp }`
- Never store secrets in JWT payload

### Authorization Header
```
Authorization: Bearer eyJhbGciOiJIUzI1NiIs...
```

## Rate Limiting

- Return `429 Too Many Requests` with `Retry-After` header
- Common limits:
  - Auth endpoints: 5/min per IP
  - API endpoints: 100/min per user
  - Search: 30/min per user

## CORS

```
Access-Control-Allow-Origin: https://your-frontend.com
Access-Control-Allow-Methods: GET, POST, PUT, PATCH, DELETE, OPTIONS
Access-Control-Allow-Headers: Content-Type, Authorization
Access-Control-Max-Age: 86400
```
Never use `*` in production.
