---
id: application-layering-and-packaging
title: 应用分层与分包标准（商业级后端必读）
domain: backend
category: 01-standards
difficulty: intermediate
tags: [分层架构, 服务层, 分包, 依赖倒置, clean-architecture, layered, service-layer, repository, dto, domain, transaction, 事务边界, package-by-feature, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# 应用分层与分包标准（商业级后端必读）

> 这是**框架无关**的硬性结构标准。任何商业级后端——无论用 NestJS / Spring Boot / FastAPI / Express / Go——都必须遵守分层、依赖方向与分包规则。写代码前先按本标准定下骨架，再填实现。把所有逻辑塞进 controller、或 service 里直接写 SQL、或 controller 直接调 repository，都是**不合格**的，质量审查会打回。

## 0. 一句话原则

**分层隔离关注点，依赖一律向内（朝业务核心）流。** 业务逻辑不依赖框架、数据库、传输协议；它们是可替换的外设。

## 1. 四层模型与各层职责（MUST / MUST NOT）

```
HTTP/gRPC/MQ ─▶ ① 接口层 Interface ─▶ ② 应用层 Application(Service) ─▶ ③ 领域层 Domain
                                              │
                                              └─▶ ④ 基础设施层 Infrastructure(Repository/Adapter) ─▶ DB/缓存/第三方
依赖方向：① → ② → ③（向内）；④ 实现 ② / ③ 定义的接口（依赖倒置，箭头也朝内）
```

### ① 接口层 / Interface（controller、handler、route、resolver、consumer）
**只做传输相关的事**：解析请求、调用一个应用服务方法、把结果/异常映射成响应。
- MUST：参数/请求体反序列化 → DTO；鉴权与限流（中间件）；把领域/应用异常映射为 HTTP 状态码；只依赖应用层。
- MUST NOT：写业务规则；直接调用 repository / ORM；直接拼 SQL；持有事务；返回 ORM entity 给客户端。
- 标准动作链：`parse → authorize → call service → map response`。

### ② 应用层 / Application（service、use-case、interactor）
**编排用例**：协调领域对象、仓储、基础设施服务，完成一个业务用例。**这是"服务层"，见 §2 重点展开。**
- MUST：无状态；一个 public 方法 = 一个用例 = 一个事务边界（Unit of Work）；输入/输出都是 DTO；编排顺序、调用领域方法、提交/回滚事务、处理应用级失败（如外部服务不可用的补偿）。
- MUST NOT：写 HTTP 细节（状态码、header）；写持久化细节（SQL、ORM 查询构造）；承载本应属于领域的不变量规则（那是贫血模型反模式）。

### ③ 领域层 / Domain（entity、value object、domain service、领域事件）
**业务的真相**：表达业务概念、规则与不变量，**有行为不只是数据**。
- MUST：把不变量（invariant）封装进实体方法（如 `order.cancel()` 内部校验状态机），而非散落在 service 的 if；用值对象（Value Object）表达 Money/Email/PhoneNumber 等带约束的概念；纯内存、零框架依赖、可独立单测。
- MUST NOT：依赖框架注解以外的运行时框架、依赖 DB、依赖 HTTP。
- 反模式：**贫血领域模型**（entity 只有 getter/setter，所有逻辑堆在 service）——禁止。

### ④ 基础设施层 / Infrastructure（repository 实现、ORM、HTTP 客户端、消息、缓存、文件存储）
**与外部世界对接**：实现内层定义的接口。
- MUST：repository 把领域对象 ↔ 持久化模型互转；封装一切 SQL/ORM/网络；实现领域/应用层声明的端口接口（依赖倒置）。
- MUST NOT：把业务规则写进 repository；让领域层 import 基础设施的具体类。

## 2. 服务层（Service Layer）怎么写——重点

服务层是商业项目最容易写烂的一层。规则：

1. **无状态 + 用例粒度**：每个 public 方法对应一个明确用例（`registerUser`、`placeOrder`、`cancelSubscription`），命名是动词+领域名词，不是 `UserService.handle()` 这种万能方法。
2. **方法即事务边界（Unit of Work）**：在服务方法的开始/结束界定事务；用例内要么全成功要么全回滚。事务**绝不**下沉到 repository（repository 不该 commit），也**绝不**上浮到 controller。
3. **收发 DTO，绝不泄露 entity**：服务方法接收 input DTO（已在边界校验），返回 output DTO；**永远不要把 ORM entity 直接返回给上层/客户端**（会泄露内部结构、产生懒加载陷阱、破坏契约）。
4. **编排，不堆砌领域逻辑**：服务负责"调用顺序/事务/跨聚合协调/调外部服务"；单个聚合内部的规则放进领域实体方法。判断标准：如果一段 if 是"这个业务对象在什么状态下允许做什么"，它属于领域层；如果是"先扣库存再创建订单再发消息"，它属于服务层。
5. **依赖抽象**：服务依赖 `OrderRepository`（接口）、`PaymentGateway`（接口），不依赖 `TypeOrmOrderRepository`、`StripeClient` 具体类——便于替换与单测（依赖倒置 + 注入）。
6. **应用级失败处理**：外部支付超时、消息发送失败等，由服务层决定补偿/重试/标记，不让它冒泡成 500。

```text
// 用例：下单（服务层方法的标准骨架）
placeOrder(input: PlaceOrderDTO): OrderDTO {
  开启事务 {                                  // ② 事务边界在服务层
    cart   = cartRepo.findOpen(input.userId)  // ④ 取数据
    order  = Order.create(cart, input.address) // ③ 领域方法内部校验不变量
    inventory.reserve(order.items)            // ③/④ 领域+基础设施
    orderRepo.save(order)                     // ④ 持久化
  } 提交/回滚
  paymentGateway.authorize(order)             // ④ 外部服务（事务外，失败可补偿）
  events.publish(OrderPlaced(order.id))       // 领域事件
  return OrderDTO.from(order)                 // 收发 DTO，不返回 entity
}
```

## 3. DTO / Entity / Domain Model / VO 的区分与映射

| 概念 | 作用 | 出现在哪层 |
|---|---|---|
| **Input DTO** | 入参契约 + 边界校验（Zod/Joi/Pydantic/Bean Validation） | 接口层接收，传入服务层 |
| **Output DTO / View Model** | 出参契约，只暴露该接口需要的字段 | 服务层产出，接口层返回 |
| **Domain Entity / Aggregate** | 带行为与不变量的业务对象 | 领域层（内存中）|
| **Value Object** | 不可变、带约束的小概念（Money、Email） | 领域层 |
| **Persistence Model / ORM Entity** | 数据库表映射 | 基础设施层 |

映射规则：
- **input DTO → 领域对象：手写映射**（显式构造，校验业务不变量），不要用自动映射工具一把梭。
- **领域对象 → output DTO：可用自动映射**（结构简单、方向安全）。
- **领域对象 ↔ persistence model：在 repository 内互转**，领域层对 ORM 无感知。
- 切忌"一个类贯穿所有层"（ORM entity 同时当 DTO 当领域模型）——这是商业项目腐化的头号原因。

## 4. 校验放哪

- **边界校验（格式/必填/范围）**：在接口层用 schema 校验 input DTO，失败返回 422 + 字段级错误。
- **业务不变量（状态机/跨字段规则/唯一性）**：在领域层实体方法内强制（如不能取消已发货订单），或在服务层做需要查库的规则（如邮箱唯一）。
- 永远不要只信前端校验。

## 5. 错误处理跨层策略

```
领域层：抛领域异常（OrderAlreadyShippedError 等，带业务语义，不带 HTTP）
应用层：不吞异常；需要补偿的应用级失败在此处理；其余向上抛
接口层：集中异常映射 → HTTP（NotFound→404, Forbidden→403, Conflict→409, Validation→422, 未知→500 且不泄露细节、记录 requestId+stack）
```

## 6. 依赖注入与依赖倒置

- 内层（领域/应用）**定义接口**（端口）；外层（基础设施）**提供实现**（适配器）。
- 通过构造函数注入装配；禁止在领域/应用层 `new` 具体的基础设施类。
- 收益：可替换数据库/第三方、领域逻辑可纯单测（mock 端口）。

## 7. 分包（Package / 模块结构）——优先 package-by-feature

**默认按功能分包（package-by-feature），而不是按层分包（package-by-layer）。** 业界共识：按功能分包可扩展性、封装性、可定位性更好，新增功能不动其它结构，且天然支持"模块化单体 → 拆微服务"的演进路径。经验法则：**先按功能分包，复杂度上来再在功能内部按层细分**。

```
src/
├─ modules/                      # 按功能（限界上下文）分包 ← 推荐
│  ├─ orders/
│  │  ├─ interface/             # controller / route / dto(in,out)
│  │  ├─ application/           # order.service.ts（用例、事务）
│  │  ├─ domain/                # order.entity.ts, order-status.vo.ts, ports(repo接口)
│  │  └─ infrastructure/        # order.repository.ts(实现), orm-models
│  ├─ payments/
│  └─ users/
├─ shared/                       # 跨功能复用：errors, result, base-entity, value-objects
└─ platform/                     # 框架装配：config, db, http server, di, middleware, logging
```

- 每个 feature 自带 interface/application/domain/infrastructure，内部遵守依赖方向。
- feature 之间**通过应用层接口/领域事件通信**，不要跨 feature 直接调对方 repository 或 entity。
- 尽量用 package-private/模块私有可见性，只导出该 feature 的公开 API（service 接口 + DTO）。
- 不要过早上微服务："复杂度不够时不配拥有 Clean Architecture"；先模块化单体，边界清晰后再按 feature 抽服务。

## 8. 常见反模式（出现即不合格）

- Fat Controller：业务逻辑写在 controller。
- Controller 直接调 repository / 直接写 SQL，跳过服务层。
- 服务层返回或接收 ORM entity（泄露持久化结构）。
- 贫血领域模型：entity 只有数据、规则全在 service。
- 一个 `God Service` 几千行，方法不是用例粒度。
- 事务写在 repository 或 controller 里。
- 领域层 import ORM/HTTP/框架具体类。
- 按层分包到极端（controllers/、services/、repositories/ 三个大目录），功能散落、改一个需求要翻三处。
- 一个类（ORM entity）同时充当 DTO + 领域模型 + 持久化模型。

## 9. 最低交付标准（写完后自检 checklist）

- [ ] controller 不含业务规则，只 parse→authorize→call service→map response。
- [ ] 每个 service 方法是一个用例，是事务边界，收发 DTO 而非 entity。
- [ ] 领域不变量封装在 entity/VO 方法里，不在 service 用裸 if 散落。
- [ ] repository 只做持久化，不含业务规则，不 commit 事务。
- [ ] 内层只依赖接口（端口），具体实现靠注入；领域层零框架/DB 依赖。
- [ ] 按 feature 分包，feature 内部按层；跨 feature 不互相调 repository/entity。
- [ ] 跨层错误处理：领域抛语义异常→接口层集中映射 HTTP，500 不泄露细节。
- [ ] input DTO 在边界校验；业务不变量在领域/服务校验。

---
**参考（commercial-grade 工程共识）**：Layered/Clean/Hexagonal Architecture（依赖向内）、Martin Fowler PoEAA（Service Layer、DTO）、DDD（聚合、值对象、避免贫血模型）、Package-by-Feature（模块化单体演进）。
