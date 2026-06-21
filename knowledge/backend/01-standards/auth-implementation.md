---
id: auth-implementation
title: 认证与授权实现标准（商业级必读 · 全流程）
domain: backend
category: 01-standards
difficulty: intermediate
tags: [认证, 授权, auth, jwt, session, refresh-token, oauth, rbac, 密码重置, 邮箱验证, 登录, 注册, 权限, 商业级]
quality_score: 96
last_updated: 2026-06-19
---

# 认证与授权实现标准（商业级必读 · 全流程）

> 认证是每个商业 app 的刚需，也是 AI 最容易写出**安全漏洞**的地方。本标准给出完整流程 + 安全要点。认证 = 你是谁（authentication），授权 = 你能做什么（authorization），两者都要做对。

## 1. 选型：Session vs Token（先定再写）

| | Session（服务端会话）| JWT/Token |
|---|---|---|
| 适合 | 传统 Web、需要随时吊销 | SPA/移动/多服务、无状态扩展 |
| 存储 | 服务端(Redis) + cookie | 客户端持有 |
| 吊销 | 天然可吊销 | 需黑名单/短过期 |

- 单体 Web 优先 **session + HttpOnly cookie**（简单、可吊销、防 XSS 窃取）。
- SPA/多服务用 **access token(短，15min) + refresh token(长，旋转)**；access 放内存，refresh 放 HttpOnly cookie。
- **不要**把 JWT 放 localStorage（XSS 可窃取）。

## 2. 注册（Registration）

1. 校验 input（email 格式、密码强度：长度≥8 + 复杂度）。
2. 检查 email 唯一（DB 唯一约束兜底，防并发重复）。
3. **密码用 bcrypt/argon2 加盐哈希**存储（cost 合理），绝不明文/弱哈希。
4. 创建用户（默认未验证状态）。
5. 发**邮箱验证**链接（带签名 token，有过期）。
6. 返回不泄露"该 email 是否已注册"（防枚举）。

## 3. 登录（Login）

1. 按 email 取用户；用户不存在也走一遍哈希比对（防时序枚举）。
2. `bcrypt.compare` 校验密码；失败信息模糊（"账号或密码错误"）。
3. **登录限流 + 失败锁定**（防爆破：N 次失败锁定/加验证码）。
4. 成功 → 建 session 或签发 access+refresh token。
5. 记录登录审计（时间、IP、UA）。

## 4. Token 刷新与登出

- **refresh token 旋转**：每次刷新签发新 refresh 并作废旧的（被盗用可检测：旧 refresh 被复用即全部吊销）。
- access 过期 → 用 refresh 换新 access；refresh 过期 → 重新登录。
- 登出：服务端使 session/refresh 失效（删除/加黑名单），清 cookie。
- 维护 token 版本/会话表，支持"登出所有设备"。

## 5. 密码重置（忘记密码）

1. 提交 email → **无论是否存在都回相同响应**（防枚举）。
2. 存在则发**一次性、短过期、签名**的重置 token（存哈希，用后即焚）。
3. 用 token 设新密码 → 校验 token 有效未用未过期 → 更新哈希 → **作废该用户所有 session/refresh**（强制重登）。
4. 重置链接走 HTTPS，token 不进日志。

## 6. 邮箱验证

- 注册后发签名验证 token（有过期）；点击验证后置 `email_verified`。
- 未验证用户限制敏感操作；可重发验证（限流）。

## 7. 授权（Authorization）—— 比认证更易漏

- **每个受保护操作在服务层校验权限**，不能只靠"登录了"或前端隐藏。
- **对象级授权（防 IDOR/BOLA）**：`GET /orders/{id}` 必须校验该 order 属于当前用户；不能凭 id 直取他人资源。
- **RBAC**：用户-角色-权限模型；后台/管理端额外校验角色。复杂场景用策略/ABAC。
- 默认拒绝，显式放行；最小权限。
- 中间件做认证（解析 token/session → 当前用户），授权在业务层（能否操作这条资源）。

## 8. OAuth / 第三方登录（如需）

- 用成熟库/标准流程（Authorization Code + PKCE），不要自己手搓。
- 校验 `state`(防 CSRF)、`nonce`；只信后端换 token，不在前端暴露 client secret。
- 首次第三方登录映射/创建本地用户；处理 email 冲突。

## 9. Cookie 与传输安全

- 认证 cookie：`HttpOnly` + `Secure` + `SameSite=Lax/Strict`。
- 全程 HTTPS；敏感端点防 CSRF（SameSite + token，或纯 Bearer）。
- 不在响应/日志回传密码哈希、完整 token。

## 10. 反模式（出现即不合格）

- 明文/MD5/SHA1 存密码；JWT 放 localStorage；JWT 永不过期或 `alg:none`。
- 登录无限流、错误信息暴露账号是否存在。
- 只校验登录、不做对象级授权（可越权访问他人数据）。
- 密码重置 token 长期有效/可复用/明文存/进日志；重置后不作废旧会话。
- 自己手搓 OAuth；前端持有 client secret。
- refresh token 不旋转、被盗无法检测。

## 11. 最低交付 checklist

- [ ] 先定 session vs token；JWT 不进 localStorage；cookie HttpOnly/Secure/SameSite。
- [ ] 密码 bcrypt/argon2 哈希 + 强度校验 + email 唯一约束。
- [ ] 登录限流/锁定 + 模糊错误 + 审计；access 短过期 + refresh 旋转。
- [ ] 密码重置：防枚举 + 一次性短期签名 token + 重置后作废旧会话。
- [ ] 邮箱验证流程。
- [ ] 每个受保护操作做对象级授权（防 IDOR）+ RBAC + 默认拒绝最小权限。
- [ ] OAuth 用标准库 + PKCE + state；全程 HTTPS + CSRF 防护。

---
**参考**：OWASP ASVS(认证/会话/访问控制)、OWASP Auth Cheat Sheet、OAuth 2.0/OIDC、RFC 6749/7636(PKCE)。
