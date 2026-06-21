---
id: rest-api-design-guide
title: REST API 设计完全指南
domain: api
category: 01-standards
difficulty: intermediate
tags: [rest, api, design]
quality_score: 92
maintainer: api-team@umadev.com
last_updated: 2026-03-29
---

# REST API 设计完全指南

## 核心原则

### 1. 资源命名
```
✅ 正确: /users, /orders
❌ 错误: /getUsers, /createOrder
```

### 2. HTTP 方法
- GET: 获取资源
- POST: 创建资源
- PUT: 完整更新
- PATCH: 部分更新
- DELETE: 删除资源

### 3. 状态码
- 200: 成功
- 201: 已创建
- 400: 请求错误
- 401: 未授权
- 403: 禁止访问
- 404: 未找到
- 500: 服务器错误

## 实战示例

### 分页
```python
@app.get("/users")
async def list_users(page: int = 1, limit: int = 20):
    offset = (page - 1) * limit
    users = db.query(User).offset(offset).limit(limit).all()
    total = db.query(User).count()
    
    return {
        "data": users,
        "meta": {"total": total, "page": page}
    }
```

### 错误处理
```json
{
  "error": {
    "code": "VALIDATION_ERROR",
    "message": "Invalid email"
  }
}
```
