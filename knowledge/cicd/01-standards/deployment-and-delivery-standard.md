---
id: deployment-and-delivery-standard
title: 部署与交付规范（商业级必读）
domain: cicd
category: 01-standards
difficulty: intermediate
tags: [部署, 交付, deployment, ci, cd, dockerfile, 多阶段, 蓝绿, 金丝雀, 回滚, rollback, 零停机, 迁移, 环境, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# 部署与交付规范（商业级必读）

> 能跑通 ≠ 能交付。商业级项目要可重复构建、自动化流水线、零停机部署、一键回滚、环境隔离。这是从"demo"到"上线产品"的关键一段。

## 1. 容器化（Dockerfile）

- **多阶段构建**：build 阶段装依赖/编译，运行阶段只拷产物 → 镜像小、攻击面小。
- 基础镜像用**小而固定版本**（`node:20-slim`、`python:3.12-slim`、distroless），不用 `latest`。
- **非 root 运行**：建专用用户，`USER app`，不用 root。
- 善用层缓存：先拷依赖清单装依赖，再拷源码（改代码不重装依赖）。
- `.dockerignore` 排除 node_modules/.git/test 等；不把密钥/`.env` 打进镜像。
- 声明 `HEALTHCHECK`、`EXPOSE`、合理的启动命令（前台进程、正确处理信号）。

## 2. CI 流水线（每次 PR/push 自动）

标准顺序，任一步失败即**阻断合并**：
```
checkout → 装依赖(带缓存) → lint/format 检查 → 类型检查 → 单元+集成测试 →
构建 → 安全扫描(依赖审计/SAST/镜像扫描) → 质量门 → 产出制品
```
- 测试与质量门失败必须红、阻断；不允许"重跑就过"的 flaky 长期存在。
- 依赖漏洞扫描（npm audit / pip-audit / cargo audit / trivy）纳入流水线。
- 制品/镜像打**不可变版本标签**（git sha / 语义版本），不用 `latest` 部署。

## 3. CD 部署策略（零停机）

- **滚动更新 Rolling**：逐批替换，配合就绪探针，默认零停机。
- **蓝绿 Blue-Green**：新版整套起好、切流量、有问题秒切回——回滚最快。
- **金丝雀 Canary**：先放小比例流量验证指标，再逐步放量。
- 部署期间新旧版本可能并存 → 接口与数据库必须**向后兼容**（见 §4）。
- 所有部署经流水线，**禁止手动 ssh 改生产**。

## 4. 数据库迁移与零停机

- 迁移作为部署的一步，**自动执行**且幂等、可回滚。
- 破坏性 schema 变更走 **expand-contract**（加列→双写→回填→切读→删旧），分多次发布，避免与并存的旧版本不兼容。
- 大表 DDL 用在线方式、回填分批，别锁表停服。

## 5. 环境与配置/密钥

- 至少 dev / staging / prod 三套环境，**同一份代码 + 不同配置**（12-Factor）。
- 配置/密钥按环境注入（env / 密钥管理 / CI secrets），**绝不进仓库、不进镜像、不进日志**。
- staging 尽量贴近 prod；上 prod 前在 staging 验证。

## 6. 回滚与发布安全

- **一键回滚**：保留上一个可用版本，出问题能快速切回（蓝绿/旧镜像重部署）。
- 发布后盯关键指标（错误率、p99、支付成功率）；异常自动/手动回滚。
- 高风险变更用 feature flag 灰度，可不发版即关闭。
- 重要发布避开高峰；有发布记录与可观测。

## 7. 健康检查与运行时

- liveness / readiness 探针；readiness 未就绪不接流量。
- 优雅停机：SIGTERM 后停接新请求、处理完在途、释放资源再退出。
- 资源 requests/limits 合理；崩溃自动重启；日志到 stdout 由平台收集。

## 8. 交付物完备（商业项目应随附）

- `Dockerfile`(多阶段、非root) + `docker-compose`(本地一键起 app+db) 或 k8s 清单。
- CI 配置（lint+test+scan+构建+质量门）。
- 数据库迁移脚本。
- `.env.example`（列全变量、占位值）。
- README：本地启动、环境变量、部署步骤、回滚方式。

## 9. 反模式（出现即不合格）

- 单阶段大镜像、root 运行、用 `latest` 部署。
- 手动 ssh 改生产、无流水线、无版本制品。
- 部署停服（无滚动/蓝绿）；破坏性迁移直接上导致旧版本崩。
- 密钥进镜像/仓库/日志；prod 与 dev 同配置。
- 无回滚方案；发布后不看指标。

## 10. 最低交付 checklist

- [ ] 多阶段、固定版本、非 root 的 Dockerfile + .dockerignore + HEALTHCHECK。
- [ ] CI：lint+类型+单元+集成+安全扫描+质量门，失败阻断合并；制品打不可变版本。
- [ ] CD：滚动/蓝绿/金丝雀之一，零停机；禁止手改生产。
- [ ] 迁移自动化、幂等、expand-contract、向后兼容、不锁表停服。
- [ ] dev/staging/prod 环境隔离，配置/密钥按环境注入不入仓库。
- [ ] 一键回滚 + 发布后指标监控；健康探针 + 优雅停机。
- [ ] 随附 Dockerfile/compose、CI、迁移、.env.example、部署 README。

---
**参考**：12-Factor App、Docker 多阶段构建最佳实践、蓝绿/金丝雀部署、Expand-Contract 迁移、Google SRE 发布工程。
