---
id: data-modeling-and-persistence
title: 数据建模与持久化规范（商业级后端必读）
domain: backend
category: 01-standards
difficulty: intermediate
tags: [数据建模, 数据库, 持久化, schema, 迁移, migration, 索引, index, 事务, transaction, n+1, 软删除, 并发, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# 数据建模与持久化规范（商业级后端必读）

> 框架/数据库无关的持久化硬性规范。数据层一旦设计错，后期改动代价极高。建表、迁移、索引、事务、并发都要按本标准来。

## 1. Schema 设计

- 每张表有**主键**：用 UUID（分布式友好、不暴露规模）或自增 BigInt（单库简单）；全项目统一一种风格。
- 必备审计列：`created_at`、`updated_at`（带时区，UTC 存储）；需要追溯的加 `created_by`/`updated_by`。
- 字段类型选对：金额用 `DECIMAL`/整数最小单位（**绝不用 float/double 存钱**）；时间用 `timestamptz`；枚举用受约束的字符串或原生 enum，不要用魔法数字。
- **非空约束 + 默认值**显式声明；该唯一的加唯一约束（如 email），别只靠应用层查重（有并发漏洞）。
- 外键 + 级联策略明确（`ON DELETE RESTRICT/CASCADE/SET NULL`）；关系正确（1:1 / 1:N / N:M 用中间表）。
- 命名一致：表用复数 snake_case（`order_items`），列 snake_case，外键 `<entity>_id`。

## 2. 范式与反范式

- 默认第三范式（消除冗余、保证一致性）。
- 仅在有明确读性能证据时，针对性反范式（冗余字段/物化视图），并写清同步策略，别一上来就反范式。
- JSON 列适合稀疏/可变结构（设置、元数据），但**不要**把核心可查询业务字段塞进 JSON（无法建索引、难约束）。

## 3. 迁移（Migration）—— 数据库变更的唯一通道

- 所有 schema 变更走**版本化迁移文件**（Flyway/Liquibase/Prisma/Alembic/TypeORM migrations），纳入版本控制，**禁止手改生产库**。
- 迁移要**可前滚**，关键变更准备回滚方案；一个迁移聚焦一件事。
- **向后兼容的扩展式变更**（expand/contract）：加列→双写→回填→切读→删旧列，分多步上线，避免停机和锁表。
- 大表加索引/改列用在线 DDL（`CREATE INDEX CONCURRENTLY` 等），避免长事务锁表。
- 回填大数据分批，别一条 SQL 锁全表。

## 4. 索引

- 给**高频查询条件、外键、排序字段、唯一约束**建索引。
- 复合索引遵循最左前缀；覆盖索引减少回表。
- 不要无脑全建——索引拖慢写入、占空间；按真实查询建。
- 定期看慢查询日志 + `EXPLAIN`，针对性优化，而非凭感觉。

## 5. 事务与一致性

- 事务边界在**服务层**（一个用例一个事务），不在 repository、不在 controller。
- 事务尽量短：把外部调用（发邮件、调第三方）移出事务，失败用补偿/重试，别让外部 IO 拖长锁。
- 并发写用乐观锁（`version`/`updated_at` 比对，冲突 409）或必要时悲观锁（`SELECT ... FOR UPDATE`）。
- 跨服务一致性用 Saga/事件 + 幂等消费，而非分布式 2PC。
- 钱/库存等关键不变量在 DB 层兜底（约束、唯一索引、`CHECK`），不只靠应用逻辑。

## 6. 查询性能：消灭 N+1

- **N+1 是后端头号性能杀手**：循环里逐条查关联 = N+1 次查询。
- 解决：预加载（eager load / `JOIN` / `include` / `selectinload`）、批量查（`WHERE id IN (...)`）、DataLoader 批处理。
- 列表接口务必检查是否产生 N+1；ORM 默认懒加载要警惕。
- 只查需要的列，别 `SELECT *` 拉大对象。

## 7. 软删除与数据生命周期

- 需要追溯/合规的数据用**软删除**（`deleted_at` 置时间），查询默认过滤；硬删除仅用于真正一次性数据。
- 唯一约束要考虑软删除（如 email 唯一 + 未删除）。
- 明确数据保留策略与归档；个人数据遵守合规（可删除/可导出）。

## 8. 仓储层（Repository）边界

- Repository 只做持久化：领域对象 ↔ 持久化模型互转、封装查询，**不含业务规则**、**不 commit 事务**。
- 领域层不感知 ORM；查询构造、SQL、ORM API 都关在 repository 内。
- 复杂查询给清晰命名的方法（`findOpenOrdersByUser`），别让上层拼条件。

## 9. 反模式（出现即不合格）

- 手改生产库、不走迁移；迁移不可回滚、一次改一大堆。
- 用 float 存钱；时间不带时区。
- 唯一性只靠应用层查重（并发下重复）。
- 列表接口 N+1；`SELECT *`；无索引的高频查询。
- 事务写在 controller/repository；事务里调外部服务拖长锁。
- 核心业务字段塞 JSON 列无法查询/约束。
- 一把梭硬删除导致无法追溯/合规。

## 10. 最低交付 checklist

- [ ] 主键 + created_at/updated_at(UTC) + 非空/默认/唯一约束 + 外键级联明确。
- [ ] 金额非 float、时间带时区、枚举受约束。
- [ ] 所有 schema 变更走版本化迁移、可回滚、大表在线 DDL/分批回填。
- [ ] 高频查询/外键/排序有索引；列表无 N+1（预加载/批量）。
- [ ] 事务边界在服务层、短事务、关键写幂等+乐观锁、DB 层兜底不变量。
- [ ] 需追溯数据软删除；唯一约束考虑软删除；合规可删可导出。
- [ ] Repository 只做持久化、不含业务、不 commit。

---
**参考**：数据库范式、Expand/Contract 迁移模式、Use The Index Luke、Saga 模式、12-Factor（后端服务无状态、配置外置）。
