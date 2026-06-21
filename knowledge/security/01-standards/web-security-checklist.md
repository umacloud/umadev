---
id: web-security-checklist
title: Web 应用安全检查清单
domain: security
category: 03-checklists
difficulty: intermediate
tags: [security, web, checklist]
quality_score: 90
maintainer: security-team@umadev.com
last_updated: 2026-03-29
---

# Web 应用安全检查清单

## OWASP Top 10 (2021)

### 1. 访问控制失效
- [ ] 实施基于角色的访问控制 (RBAC)
- [ ] 验证所有 API 端点
- [ ] 使用最小权限原则
- [ ] 记录访问控制失败

### 2. 加密失败
- [ ] 强制使用 TLS 1.3+
- [ ] 加密敏感数据 (AES-256)
- [ ] 密钥轮换策略
- [ ] 禁用弱加密算法

### 3. 注入攻击
- [ ] 使用参数化查询
- [ ] 输入验证和白名单
- [ ] ORM 防护
- [ ] 错误信息不暴露 SQL

### 4. 不安全设计
- [ ] 威胁建模
- [ ] 安全开发生命周期 (SDLC)
- [ ] 最小权限设计
- [ ] 默认拒绝策略

### 5. 安全配置错误
- [ ] 移除默认账户
- [ ] 禁用不必要的功能
- [ ] 安全 HTTP 头
- [ ] 错误处理不泄露堆栈

### 6. 易受攻击的组件
- [ ] 定期更新依赖
- [ ] 移除未使用的依赖
- [ ] 监控 CVE
- [ ] 锁定版本号

### 7. 身份识别失败
- [ ] 多因素认证 (MFA)
- [ ] 密码强度策略
- [ ] 账户锁定机制
- [ ] 会话超时

### 8. 软件和数据完整性失败
- [ ] 代码签名
- [ ] CI/CD 安全
- [ ] 依赖验证
- [ ] 自动化测试

### 9. 安全日志不足
- [ ] 记录认证事件
- [ ] 监控异常行为
- [ ] 集中式日志
- [ ] 告警机制

### 10. 服务器端请求伪造 (SSRF)
- [ ] 验证用户提供的 URL
- [ ] 网络分段
- [ ] 白名单域名
- [ ] 禁用重定向

## 通用安全实践

### 认证
- [ ] JWT 过期时间 < 1 小时
- [ ] 刷新 token 机制
- [ ] HTTPS Only
- [ ] HttpOnly + Secure cookies

### 数据保护
- [ ] 敏感数据加密
- [ ] 不记录敏感信息
- [ ] 安全删除
- [ ] 数据最小化

### API 安全
- [ ] 速率限制
- [ ] API keys 轮换
- [ ] 输入验证
- [ ] CORS 配置

### 监控
- [ ] 实时告警
- [ ] 异常检测
- [ ] 性能监控
- [ ] 安全事件追踪

## 工具推荐
- OWASP ZAP
- Burp Suite
- SonarQube
- Snyk
- Dependabot
