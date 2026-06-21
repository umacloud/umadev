---
id: postgresql-performance-optimization
title: PostgreSQL 性能优化实战
domain: database
category: 02-playbooks
difficulty: advanced
tags: [postgresql, database, performance, optimization]
quality_score: 93
maintainer: dba-team@umadev.com
last_updated: 2026-03-29
version: 2.0
---

# PostgreSQL 性能优化实战

## 概述

本实战指南提供系统化的 PostgreSQL 性能优化方法,从慢查询分析到索引优化,从配置调优到监控告警。

## 场景 1: 慢查询分析与优化

### 识别慢查询
```sql
SELECT 
    query,
    calls,
    round(total_exec_time::numeric, 2) as total_ms,
    round(mean_exec_time::numeric, 2) as avg_ms
FROM pg_stat_statements
ORDER by total_exec_time desc
limit 10;
```

### 查询优化策略

```sql
-- 使用 EXPLAIN ANALYZE
EXPLAIN ANALYZE SELECT * from orders where user_id = 123;

-- 添加缺失的索引
CREATE index idx_orders_user_id on orders(user_id);
```

## 场景 2: 知识库优化
### 创建知识库
```sql
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
```
### 查询优化示例
```sql
-- 优化前
SELECT * FROM orders WHERE created_at > '2024-01-01';

-- 优化后
SELECT * FROM orders 
WHERE created_at > '2024-01-01'
LIMIT 100;
```
