---
id: config-and-observability
title: 配置管理与可观测性规范（商业级必读）
domain: backend
category: 01-standards
difficulty: intermediate
tags: [配置, config, 12-factor, 环境变量, 密钥, 日志, logging, 指标, metrics, 追踪, tracing, 健康检查, 优雅停机, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# 配置管理与可观测性规范（商业级必读）

> 商业系统要**可配置、可观测、可运维**。配置外置（12-Factor）、结构化日志、指标、追踪、健康检查、优雅停机——线上出问题时能快速定位，是商业级与玩具的分水岭。

## 1. 配置管理（12-Factor）

- **配置与代码分离**：所有环境差异（DB 地址、密钥、开关、外部 URL）从**环境变量**读，绝不硬编码、绝不写死在代码里按环境 if。
- 启动时**校验必需配置**，缺失立即失败并给清晰错误（fail-fast），不要运行到一半才崩。
- 提供 `.env.example` 列出所有变量（占位值）；`.env` 入 gitignore。
- 密钥走密钥管理（Vault/KMS/云 secret），不进仓库、不进镜像、不进日志。
- 配置有合理默认 + 类型校验（如端口是数字）；分环境（dev/staging/prod）但同一份代码。

## 2. 结构化日志

- 日志输出**结构化 JSON**（而非纯字符串拼接），便于检索聚合。
- 每条日志带：时间、级别、**request_id/trace_id**、服务名、关键上下文（userId、orderId）。
- 级别用对：DEBUG(排查) / INFO(关键业务事件) / WARN(可恢复异常) / ERROR(需关注)。生产默认 INFO。
- **绝不打印**密码、token、密钥、完整 PII（脱敏）。
- 日志写 stdout/stderr，由平台收集（12-Factor），不自己管理日志文件轮转。
- 请求贯穿同一 request_id，从入口到各层到响应可串联。

## 3. 指标（Metrics）

- 暴露关键指标（Prometheus 风格）：
  - **RED**（面向请求的服务）：Rate(请求量)、Errors(错误率)、Duration(延迟分布 p50/p95/p99)。
  - **USE**（面向资源）：Utilization、Saturation、Errors。
  - 业务指标：下单数、支付成功率、注册转化等。
- 延迟看分位数（p95/p99），不要只看平均。

## 4. 分布式追踪（Tracing）

- 跨服务调用传递 trace context（OpenTelemetry / W3C traceparent），串联一次请求经过的所有服务/DB/外部调用。
- 关键 span 标注耗时与状态，定位慢在哪一环。

## 5. 健康检查与优雅停机

- 提供 **liveness**（进程活着）与 **readiness**（依赖就绪、可接流量）端点，供 k8s/LB 探测。
- **优雅停机**：收到 SIGTERM 后停止接新请求、处理完在途请求、关闭连接池/释放资源再退出，避免请求被硬切。
- 启动顺序：先连依赖（DB/缓存）就绪再标 ready。

## 6. 错误与告警

- 未捕获异常集中处理 + 上报（Sentry 类）；带 request_id 可回溯。
- 对关键指标设告警阈值（错误率、p99 延迟、队列积压、支付失败率）。
- 错误响应对客户端模糊（500 不泄露内部），日志记完整上下文。

## 7. 反模式（出现即不合格）

- 配置硬编码、按环境 if、密钥进代码/镜像。
- 缺失配置运行到一半才崩（没 fail-fast）。
- 纯字符串日志、无 request_id、无法串联一次请求。
- 日志打印密钥/PII。
- 只有平均延迟、没有 p95/p99；没有错误率/业务指标。
- 没有健康检查；停机硬切导致在途请求失败。

## 8. 最低交付 checklist

- [ ] 配置全从 env 读、启动 fail-fast 校验、提供 .env.example、密钥走密钥管理不入仓库。
- [ ] 结构化 JSON 日志，带 request_id/trace_id 与上下文，脱敏，输出 stdout。
- [ ] 暴露 RED/USE + 关键业务指标，延迟看 p95/p99。
- [ ] 跨服务传递 trace context（OpenTelemetry）。
- [ ] liveness/readiness 端点 + 收到 SIGTERM 优雅停机。
- [ ] 未捕获异常上报告警；关键指标设阈值告警。

---
**参考**：12-Factor App、OpenTelemetry、Google SRE（RED/USE、SLI/SLO）、Prometheus。
