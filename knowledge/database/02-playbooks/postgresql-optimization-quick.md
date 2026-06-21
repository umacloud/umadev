---
id: postgresql-optimization-quick
title: PostgreSQL 快速优化指南
domain: database
category: 02-playbooks
difficulty: intermediate
tags: [postgresql, database, optimization]
quality_score: 90
maintainer: dba-team@umadev.com
last_updated: 2026-03-29
---

# PostgreSQL 快速优化指南

## 栅查看询性能

### 1. 启用性能监控
```sql
CREATE EXTENSION pg_stat_statements;
SELECT * FROM pg_stat_statements ORDER BY mean_exec_time DESC LIMIT 10;
```

### 2. 分析执行计划
```sql
EXPLAIN ANALYZE SELECT * FROM orders WHERE user_id = 123;
```

## 索引优化

### 创建关键索引
```sql
CREATE INDEX CONCURRENTLY idx_users_email ON users(email);
CREATE INDEX idx_orders_date ON orders(created_at);
```

### 检查未使用索引
```sql
SELECT * FROM pg_stat_user_indexes WHERE idx_scan = 0;
```

## 配置优化
### 关键参数
```sql
ALTER SYSTEM SET shared_buffers = '256MB';
ALTER SYSTEM SET work_mem = '16MB';
ALTER SYSTEM SET max_connections = 200;
```

### 重载配置
```sql
SELECT pg_reload_conf();
```
