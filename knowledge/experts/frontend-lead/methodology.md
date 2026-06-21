---
id: methodology
title: Frontend Lead — Development Methodology
domain: experts
category: frontend-lead
difficulty: intermediate
tags: [architecture, client, component, error, experts, handling, management, methodology]
quality_score: 70
last_updated: 2026-06-15
---
# Frontend Lead — Development Methodology

## 前端/客户端标准库速查（按需查阅，`<platform>/01-standards/<id>`）

**先看架构文档声明的目标平台**，再查对应标准照着做：

- Web 通用：frontend-architecture-and-layering（feature 分包/数据访问层/状态分治）· web-framework-best-practices（React/Next App Router/Vue 官方）· forms-and-validation · admin-dashboard-and-crud（后台/CRUD）· i18n-and-localization · accessibility-standard（无障碍）· seo-and-web-vitals
- 移动：mobile/mobile-app-standard + 设计 mobile/{ios-design-hig, android-material-design}
- 鸿蒙：harmony/{harmonyos-arkts-standard, harmonyos-design}
- 小程序：miniprogram/{miniprogram-standard, miniprogram-design}
- 桌面：desktop/{desktop-app-standard, desktop-design}
- 跨平台：cross-platform/{platform-selection-and-architecture, cross-platform-frameworks}
- **UI 务必遵循目标平台的官方设计规范**（iOS HIG / Android Material 3 / HarmonyOS Design / 微信 WeUI / macOS·Windows），不要套 web 范式。

## 结构第一：按功能分包 + 关注点分层（动手前先定骨架）

商业级前端的第一要务是结构。详见标准《前端架构与分层标准》(`frontend/01-standards/frontend-architecture-and-layering`)，硬性底线：

- **按 feature 分包**（不按类型）：`features/<x>/{api,components,hooks,stores,types,index.ts}`；跨 feature 只经对方 `index.ts` 引用，禁止深层 import；`utils/` 不是垃圾场，feature 相关的 helper 留在 feature 内。
- **分层**：展示组件(dumb，纯 props→UI) ↔ 容器组件(取数+编排) ↔ 数据访问层(typed API，唯一出口) ↔ 领域逻辑(纯函数/hook)。
- **数据访问隔离**：组件内**禁止裸 fetch/axios**，统一走 typed API 层；路径集中常量、与后端契约一致；每个视图处理 loading/error/empty 三态。
- **状态三类分治**：服务端数据用 React Query/SWR（别用 Redux 手动管），全局态用 Zustand/Redux/Pinia，UI 态用本地 useState/ref。
- **业务逻辑下沉**到纯函数/hook，JSX/模板只做声明式渲染，副作用在 hook 里并清理。
- **红线**：组件裸 fetch、业务逻辑写在 JSX、按类型分大筐、巨型组件、只画 happy path 不处理三态、emoji 当功能图标、硬编码颜色。

## Component Architecture

### Component Categories
1. **Primitives** (atoms): Button, Input, Badge, Avatar, Icon
   - No business logic, only presentation
   - Accept variants via props (size, color, disabled)
   - Fully accessible (keyboard, ARIA)

2. **Composites** (molecules): SearchBar, FormField, Card, Modal
   - Combine 2-3 primitives
   - May have internal state (open/closed, input value)
   - Still reusable across features

3. **Features** (organisms): LoginForm, DashboardHeader, UserList
   - Business logic lives here
   - Connect to API / state management
   - Specific to one feature

4. **Pages** (templates): /dashboard, /settings, /auth/login
   - Compose features into a full layout
   - Handle routing, auth guards, data fetching

### Component File Structure
```
components/
  Button/
    Button.tsx        # component
    Button.test.tsx   # unit tests
    Button.stories.tsx # storybook (if used)
    index.ts          # re-export
```

### Props Design Rules
- Use interface, not inline types
- Required props first, optional last
- Sensible defaults for optional props
- Event handlers: `onX` naming (onClick, onChange, onSubmit)
- Children for composition, not deep prop drilling
- No more than 7 props (split into smaller components if needed)

## State Management

### Where State Lives
| State type | Storage | Example |
|---|---|---|
| UI state (local) | useState / ref | modal open, input value, accordion expanded |
| Form state | form library | field values, validation, dirty/touched |
| Server state | query cache (React Query / SWR) | API data, loading/error states |
| Global app state | context / store | auth user, theme, locale |
| URL state | search params / path | current page, filters, sort order |

### Rules
- Default to local state. Only lift when two+ components need it.
- Server state is NOT client state — use a cache library, not Redux/Zustand for API data.
- URL state for anything the user might bookmark or share.
- Never store derived values — compute them on render.

## API Client Pattern

### Centralized fetch wrapper
```typescript
// lib/api.ts
const API_BASE = process.env.NEXT_PUBLIC_API_URL;

export async function apiFetch<T>(path: string, options?: RequestInit): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    headers: {
      'Content-Type': 'application/json',
      ...getAuthHeader(),
      ...options?.headers,
    },
    ...options,
  });
  if (!res.ok) {
    const error = await res.json().catch(() => ({ message: res.statusText }));
    throw new ApiError(res.status, error.message, error.details);
  }
  return res.json();
}
```

### Per-resource API functions
```typescript
// api/users.ts
export const usersApi = {
  list: (params?: ListParams) => apiFetch<User[]>('/users', { params }),
  get: (id: string) => apiFetch<User>(`/users/${id}`),
  create: (data: CreateUser) => apiFetch<User>('/users', { method: 'POST', body: JSON.stringify(data) }),
  update: (id: string, data: Partial<User>) => apiFetch<User>(`/users/${id}`, { method: 'PATCH', body: JSON.stringify(data) }),
  delete: (id: string) => apiFetch<void>(`/users/${id}`, { method: 'DELETE' }),
};
```

## Error Handling

### Error Boundary (global)
Catches rendering errors, shows fallback UI, reports to error tracking.

### API Error Handling (per-request)
```typescript
try {
  const data = await usersApi.create(formData);
  // success: redirect or show toast
} catch (error) {
  if (error instanceof ApiError) {
    if (error.status === 422) {
      // validation: show field-level errors
      setFieldErrors(error.details);
    } else if (error.status === 409) {
      // conflict: "email already exists"
      showToast('error', error.message);
    } else {
      // other API error
      showToast('error', 'Something went wrong');
    }
  } else {
    // network error
    showToast('error', 'Unable to connect to server');
  }
}
```

### Loading States
- Skeleton screens for initial load (not spinners)
- Inline loading for mutations (button shows spinner, text changes to "Saving...")
- Optimistic updates for fast-feeling UI (update UI first, then sync with server)

### Empty States
Every list/table/grid must have:
- First-time empty: "No items yet. Create your first X."
- Filtered empty: "No results match your filters."
- Error empty: "Failed to load. [Retry button]"

## Performance Checklist

- [ ] Images: lazy loaded, responsive sizes, modern format (WebP/AVIF)
- [ ] Fonts: preloaded, `font-display: swap`, subset if possible
- [ ] JavaScript: code-split by route, tree-shaken, no unused dependencies
- [ ] CSS: purged unused styles, critical CSS inlined
- [ ] API calls: deduplicated (cache library), prefetched on hover
- [ ] Lists: virtualized if > 100 items (react-virtual / tanstack-virtual)
- [ ] Bundle size: < 200KB gzipped for initial load
- [ ] Core Web Vitals: LCP < 2.5s, FID < 100ms, CLS < 0.1
