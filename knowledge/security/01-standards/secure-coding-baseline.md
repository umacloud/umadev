---
id: secure-coding-baseline
title: 安全编码基线（商业级必读 · OWASP 驱动）
domain: security
category: 01-standards
difficulty: intermediate
tags: [安全, security, owasp, 鉴权, 授权, 注入, sql注入, xss, csrf, 密钥, 密码哈希, 限流, 最小权限, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# 安全编码基线（商业级必读 · OWASP 驱动）

> 上线即面对真实攻击。这是每个商业级后端/全栈必须满足的安全底线（对齐 OWASP Top 10）。**不是可选项**，缺任何一条都可能造成数据泄露/资金损失。

## 1. 认证（Authentication）

- 密码**必须用 bcrypt / argon2 / scrypt 加盐哈希**存储，绝不明文、绝不用 MD5/SHA1。
- 登录失败信息模糊（"账号或密码错误"），不暴露账号是否存在；登录加限流/锁定防爆破。
- JWT：设短过期 + refresh 机制；签名算法固定（拒绝 `alg:none`）；密钥够强且在 env；敏感场景用服务端 session。
- 多因素（MFA）用于高权限/敏感操作。

## 2. 授权（Authorization）—— 越权是头号漏洞

- **每个受保护操作都要在服务层校验"这个用户能否操作这条资源"**（对象级授权），不能只靠"登录了就行"或前端隐藏按钮。
- 防 IDOR / BOLA：`GET /orders/{id}` 必须校验该 order 属于当前用户，不能凭 id 直取。
- 最小权限原则：默认拒绝，按角色/权限显式放行；后台/管理端额外加固。

## 3. 注入（Injection）

- **SQL 一律参数化/预编译或用 ORM 参数绑定**，绝不字符串拼接 SQL。
- 命令执行避免拼接用户输入；必须时用白名单 + 转义。
- NoSQL / LDAP / XPath 同理参数化。
- 模板/表达式不要 eval 用户输入。

## 4. 输入校验与输出编码

- 所有外部输入在边界校验（类型/长度/范围/白名单），拒绝非法而非"尽量修"。
- 输出到 HTML 做转义防 XSS；前端避免 `dangerouslySetInnerHTML`/`v-html` 直出用户内容；设置 CSP。
- 文件上传校验类型/大小/内容，存储隔离、重命名、不可执行。

## 5. 会话与 CSRF

- Cookie 设 `HttpOnly` + `Secure` + `SameSite`；token 不放可被 JS 读取处时防 XSS 窃取。
- 状态变更（表单/写操作）防 CSRF：SameSite cookie + CSRF token 或仅用 Bearer。
- 退出登录使 token/session 失效。

## 6. 密钥与配置

- **密钥/密码/token 绝不硬编码进源码或提交进仓库**，一律走环境变量/密钥管理（Vault/KMS），`.env` 入 gitignore，提供 `.env.example` 占位。
- 不在日志/报错/响应里打印密钥、密码、完整 token、PII。
- 第三方密钥定期轮换；不同环境不同密钥。

## 7. 传输与数据保护

- 全站 HTTPS / TLS；启用 HSTS。
- 敏感数据（PII、支付）传输加密、必要时静态加密；最小化收集与留存。
- 合规（GDPR/个保法）：可删除、可导出、明确留存期。

## 8. 依赖与供应链

- 锁定依赖版本（lockfile）；CI 跑 `npm audit` / `pip-audit` / `cargo audit` 扫漏洞。
- 不引入未审查的小众包（防 typosquatting/供应链投毒）。
- 及时升级有 CVE 的依赖。

## 9. 限流、错误与日志

- 登录、发码、支付、搜索等加限流（按 IP/用户/端点）。
- 错误对客户端模糊（500 不暴露栈/SQL/路径），对内记录完整上下文（requestId）。
- 安全相关事件（登录、权限变更、失败尝试）审计日志。

## 10. 反模式（出现即不合格）

- 明文/弱哈希存密码；JWT 允许 `alg:none`、永不过期。
- 只校验登录、不做对象级授权（可越权访问他人数据）。
- 拼接 SQL；eval 用户输入；`v-html`/`dangerouslySetInnerHTML` 直出用户内容。
- 密钥硬编码/提交进仓库；日志打印密钥/PII。
- 无限流、无依赖漏洞扫描、500 暴露内部细节。

## 11. 最低交付 checklist

- [ ] 密码 bcrypt/argon2 哈希；登录限流防爆破；JWT 短过期且算法固定。
- [ ] 每个受保护操作做对象级授权（防 IDOR/BOLA），默认拒绝、最小权限。
- [ ] SQL 全参数化；输入边界校验；输出转义 + CSP 防 XSS。
- [ ] 状态变更防 CSRF；Cookie HttpOnly/Secure/SameSite。
- [ ] 密钥全走 env/密钥管理，不入源码/日志；HTTPS+HSTS。
- [ ] 依赖锁定 + CI 漏洞扫描；关键端点限流；500 不泄露细节 + 审计日志。

---
**参考**：OWASP Top 10、OWASP ASVS、OWASP Cheat Sheets、CWE。
