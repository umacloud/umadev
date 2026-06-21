---
id: frontend-architecture-and-layering
title: 前端架构与分层标准（商业级前端必读）
domain: frontend
category: 01-standards
difficulty: intermediate
tags: [前端架构, 分层, 分包, feature-based, feature-sliced, api层, 状态管理, 业务逻辑, container-presentational, react, vue, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# 前端架构与分层标准（商业级前端必读）

> 框架无关（React / Vue / Svelte 通用）的硬性结构标准。商业级前端不是"把组件堆出来能跑"，而是**按功能分包、关注点分层、业务逻辑不写在 JSX/模板里、数据访问统一隔离**。写页面前先定骨架，再填实现。组件里裸 `fetch`、业务逻辑塞进 JSX、把所有东西丢进 `utils/`，都是不合格的。

## 0. 一句话原则

**按 feature 组织代码（features not folders），关注点分层，依赖向内：UI 依赖逻辑、逻辑不依赖 UI；网络/存储是可替换外设。**

## 1. 分层模型与职责

```
路由/页面 Page ─▶ 容器组件 Container ─▶ 展示组件 Presentational(dumb, 纯 props→UI)
                       │
                       ├─▶ 状态层 State (server-cache / app-state / ui-state)
                       ├─▶ 领域逻辑层 Domain (纯函数/hooks，计算、校验、派生)
                       └─▶ 数据访问层 API (typed client，唯一出口)──▶ 后端
```

- **展示组件（Presentational / dumb）**：只接收 props、渲染、向上抛事件；无副作用、不取数、可复用、好测。
- **容器/特性组件（Container）**：组合展示组件，连接状态与数据访问，编排交互。
- **状态层**：见 §2 三类状态分治。
- **领域逻辑层**：纯函数 + 自定义 hook，承载计算/校验/派生/格式化——**不要写在 JSX 里**。
- **数据访问层（API）**：与后端通信的**唯一**出口；组件/状态层通过它取数，绝不在组件里裸 `fetch`/`axios`。

## 2. 状态分三类，别混（关键）

商业前端 80% 的混乱来自把三种状态混在一起。明确分治：

| 状态类型 | 是什么 | 用什么 |
|---|---|---|
| **服务端缓存状态** | 来自后端、需要缓存/失效/重取的数据 | React Query / SWR / TanStack Query（Vue 用 `@tanstack/vue-query`、Pinia colada） |
| **应用全局状态** | 跨页面的客户端状态（登录态、主题、购物车草稿） | Zustand / Redux Toolkit（Vue 用 Pinia）|
| **UI 局部状态** | 仅本组件的开关/输入/hover | `useState` / `ref` / signals |

- **不要**把服务端数据塞进 Redux/Zustand 手动维护——用 React Query 管缓存/loading/error/重试。
- **不要**把只属于一个组件的开关提升到全局 store。
- 全局 store 只放"真正跨组件共享且非服务端"的状态。

## 3. 数据访问层（API 层）隔离

- 组件/页面**禁止**出现裸 `fetch` / `axios`；所有请求经过 **typed API client**。
- 每个 feature 有自己的 `api/` 模块，导出类型化的请求函数（`getOrders(): Promise<OrderDTO[]>`）。
- 前后端契约对齐：请求/响应类型与后端 DTO/OpenAPI 一致；路径集中为常量，不散落字符串。
- 每个数据请求处理 **loading / error / empty** 三态；错误统一拦截（401 跳登录、5xx 提示、网络错误重试）。
- 鉴权 header、baseURL、超时、重试在 client 层统一配置一次。

## 4. 业务逻辑放哪

- 计算/派生/校验/格式化 → **纯函数或自定义 hook**（`useCart()`、`formatMoney()`），可独立单测。
- JSX/模板里只放"声明式渲染 + 简单条件"，不写复杂分支与副作用。
- 副作用（订阅、定时器、取数）放进 hook 并做好清理（cleanup）。

## 5. 分包：feature-based（按功能，不按类型）

**默认按功能分包。** 不要建 `components/`、`hooks/`、`services/` 三个大筐把全项目同类堆一起（改一个需求要翻三处）。`utils/` 不是垃圾场——和计费相关的 helper 就放进 `billing/`。清晰的文件名本身就是架构。

```
src/
├─ app/                      # 应用装配：路由、providers、全局布局、入口
├─ features/                 # 按功能（业务域）分包 ← 推荐
│  ├─ orders/
│  │  ├─ api/               # orders.api.ts（typed 请求，唯一出口）
│  │  ├─ components/        # 该 feature 的展示/容器组件
│  │  ├─ hooks/             # useOrders, useCancelOrder（业务逻辑）
│  │  ├─ stores/           # 该 feature 的局部全局状态（如需）
│  │  ├─ types.ts          # DTO/视图模型类型
│  │  └─ index.ts          # 该 feature 对外的公开 API（barrel）
│  ├─ checkout/
│  └─ auth/
├─ shared/                   # 跨 feature 复用：ui-kit(Button/Input)、lib、hooks、api-client
└─ pages|routes/             # 路由到 feature 的薄装配层（Next.js 用 app/）
```

- 跨 feature 只通过对方 `index.ts` 暴露的公开 API 引用，**禁止深层 import** 对方内部文件。
- `shared/ui-kit` 放无业务的纯展示组件（设计系统落地）；有业务的组件留在各 feature。
- 中型项目（5–20 人）可用 feature + 类型的混合，但 feature 边界优先。

## 6. 组件与可访问性规范

- 容器/展示分离：取数与状态在容器，纯渲染在展示组件。
- props 全部 TypeScript 类型化，不用 `any`；表单受控、有校验与错误提示。
- 可访问性（a11y）：语义化标签、`aria-*`、键盘可达、焦点管理、对比度达标。
- 列表渲染稳定 `key`；`useEffect` 依赖正确并清理；不在渲染期做副作用或改 state。
- 图标来自声明的图标库（Lucide/Heroicons/Tabler），**不用 emoji 当功能图标**；颜色走设计 token，不硬编码 hex。

## 7. 常见反模式（出现即不合格）

- 组件里裸 `fetch`/`axios`，没有 API 层。
- 用 Redux/Zustand 手动管服务端数据（该用 React Query）。
- 业务逻辑、复杂分支、副作用写在 JSX/模板里。
- 按类型分包（components/services/hooks 三大筐），feature 散落各处。
- `utils/` 变成什么都往里塞的垃圾场。
- 跨 feature 深层 import 对方内部文件，耦合成一团。
- 巨型组件（几百行、又取数又渲染又算逻辑）。
- 不处理 loading/error/empty 三态，只画 happy path。

## 8. 最低交付标准（写完后自检 checklist）

- [ ] 按 feature 分包，每个 feature 自带 api/components/hooks/types，对外只经 index 暴露。
- [ ] 所有网络请求走 typed API 层，组件内无裸 fetch；路径集中常量、与后端契约一致。
- [ ] 三类状态分治：服务端数据用 React Query/SWR，全局态用 store，UI 态用本地。
- [ ] 业务逻辑在纯函数/hook，JSX 只做声明式渲染。
- [ ] 每个数据视图处理 loading/error/empty 三态；错误统一拦截。
- [ ] 容器/展示分离；props 类型化；a11y 与设计 token 达标；无 emoji 图标、无硬编码颜色。
- [ ] 跨 feature 不深层 import；shared 只放无业务的复用件。

---
**参考（commercial-grade 前端共识）**：Feature-Sliced Design、Bulletproof React（feature-based）、Clean Architecture（依赖向内、业务逻辑居核心）、TanStack Query（服务端缓存分治）。
