---
id: web-framework-best-practices
title: Web 主流框架最佳实践（React/Next.js/Vue · 官方）
domain: frontend
category: 01-standards
difficulty: intermediate
tags: [react, next.js, app-router, server-components, rsc, 缓存, 数据获取, vue, composition-api, 响应式, ssr, 性能, 官方, 商业级]
quality_score: 95
last_updated: 2026-06-19
---

# Web 主流框架最佳实践（React/Next.js/Vue · 官方）

> 纯 Claude/Codex 写新版框架常踩坑（Next App Router 缓存/RSC 用错、React 不必要重渲染、Vue 响应式失效）。本标准是各框架官方最佳实践要点。

## 1. React（通用）

- **Hooks 规则**：只在顶层调用，不在条件/循环里；`useEffect` 依赖完整且做 cleanup；副作用只放 effect，不在渲染期执行。
- **避免不必要重渲染**：稳定 `key`；`memo`/`useMemo`/`useCallback` 用在真有开销处（别滥用）；状态尽量下放/拆分，避免一处 state 触发大树重渲染。
- **派生数据**用计算而非冗余 state；列表长用虚拟化。
- 组件单一职责、props 类型化；逻辑抽自定义 hook；展示/容器分离（见前端架构标准）。

## 2. Next.js App Router（官方，最容易写错）

- **Server vs Client Components**：默认 **Server Component**（取数、组合、重依赖——减小客户端包）；只有需要交互/浏览器 API/`useState`/`useEffect` 时才 `'use client'`，且把 client 边界下推到叶子，别把整页标 client。
- **数据获取在 Server Component 里直接 `await`**（不必客户端 useEffect 取数）；`fetch` 在同一渲染内自动去重(请求记忆化)。
- **三层缓存要懂**：Data Cache（fetch 结果）/ Full Route Cache（静态 HTML/Flight）/ Router Cache（客户端导航）。控制策略：
  - 实时数据 → `fetch(url, {{ cache: 'no-store' }})`。
  - 可接受短暂陈旧 → `fetch(url, {{ next: {{ revalidate: N }} }})`（ISR 定时再生）。
  - 写后按需失效 → `next: {{ tags: [...] }}` + 写操作里 `revalidateTag()` / `revalidatePath()`。
- **流式渲染**：用 `loading.js` + `<Suspense>` 边界先出关键 UI、渐进 hydrate；可用 **Partial Prerendering (PPR)** 静态壳 + 动态洞流式。
- **Server Actions** 处理表单/写操作（带校验+鉴权）；Route Handlers 做 API。
- 不要在 Server Component 里用浏览器 API/事件；不要把密钥泄漏到 client；env 区分 `NEXT_PUBLIC_`。

## 3. Vue 3（官方）

- 用 **Composition API + `<script setup>`**；响应式用 `ref`/`reactive`，注意**解构会丢响应式**（用 `toRefs`）。
- 计算属性 `computed` 缓存派生值；`watch`/`watchEffect` 做副作用并清理；避免在模板里写重逻辑。
- 状态管理用 **Pinia**（官方推荐）；按 feature 组织 store。
- 性能：`v-for` 必带稳定 `key`；大列表虚拟化；`v-once`/`v-memo` 静态内容；异步组件 + 路由懒加载；避免不必要的深层响应式。
- SSR/SSG 用 **Nuxt**；注意服务端/客户端 hydration 一致。

## 4. 通用（无论框架）

- 数据访问统一走 typed API 层（组件不裸 fetch，见前端架构标准）；服务端数据用 React Query/SWR/Vue Query 缓存。
- 代码分割 + 路由懒加载；图片/字体优化 + CDN；关注 Core Web Vitals(LCP/CLS/INP)。
- 严格 TS 类型，不用 any；ESLint + Prettier。

## 5. 反模式（出现即不合格）

- Next：整页标 `'use client'`、在 client 组件里 useEffect 取本可服务端取的数据、不懂缓存导致数据不更新或永远不缓存、密钥泄漏到 client。
- React：滥用/漏用 memo、依赖数组错误致 effect 失控、一处大 state 全树重渲染、列表无 key。
- Vue：解构 reactive 丢响应式、模板写重逻辑、v-for 无 key、不用 computed 重复计算。
- 通用：组件裸 fetch、用 Redux/手动管服务端缓存、不做代码分割、any 满天飞。

## 6. 最低交付 checklist

- [ ] React：Hooks 规则 + effect 依赖/清理 + 合理 memo + 列表 key + 状态拆分。
- [ ] Next App Router：默认 Server Component、client 边界下推、服务端取数、缓存策略(no-store/revalidate/tags)用对、Suspense 流式、Server Actions 带校验鉴权、不泄密钥。
- [ ] Vue：Composition API + `<script setup>`、响应式不丢(toRefs)、computed/watch 正确、Pinia、v-for key + 虚拟化。
- [ ] 通用：统一 API 层 + 服务端数据缓存库 + 代码分割/懒加载 + 资源优化 + Core Web Vitals + 严格 TS。

---
**参考（官方）**：Next.js 官方文档(App Router/Caching/Fetching/Server Components/PPR)、React 官方(Rules of Hooks/性能)、Vue 3 官方(Composition API/响应式/性能)、Pinia、Nuxt。
