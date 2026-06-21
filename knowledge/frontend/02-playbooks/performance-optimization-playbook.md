---
id: performance-optimization-playbook
title: 前端性能优化实战剧本
domain: frontend
category: 02-playbooks
difficulty: advanced
tags: [frontend, performance, optimization]
quality_score: 91
maintainer: frontend-team@umadev.com
last_updated: 2026-03-29
---

# 前端性能优化实战剧本

## 场景 1: Web Vitals 优化

### 目标指标
- LCP (Largest Contentful Paint): < 2.5s
- FID (First Input Delay): < 100ms
- CLS (Cumulative Layout Shift): < 0.1

### 优化 LCP

```html
<!-- 1. 预加载关键资源 -->
<link rel="preload" href="critical.css" as="style">
<link rel="preload" href="hero.webp" as="image">

<!-- 2. 内联关键 CSS -->
<style>
  .hero { background: url(hero.webp); }
</style>

<!-- 3. 延迟非关键资源 -->
<link rel="preload" href="analytics.js" as="script" onload="this.onload=null">
```

### 优化 FID

```javascript
// 1. 代码分割
import('./heavy-module.js').then(module => {
  module.init();
});

// 2. 预连接关键域名
<link rel="preconnect" href="https://api.example.com">
<link rel="dns-prefetch" href="https://cdn.example.com">

// 3. 减少 JavaScript 阻塞
<script defer src="analytics.js"></script>
```

### 优化 CLS

```css
/* 1. 预留图片空间 */
.image-container {
  aspect-ratio: 16 / 9;
  background: #f0f0f0;
}

/* 2. 避免动态注入内容 */
.ad-slot {
  min-height: 250px;
}

/* 3. 使用 transform 动画 */
@keyframes slide {
  from { transform: translateX(0); }
  to { transform: translateX(100px); }
}
```

## 场景 2: JavaScript 性能优化

### 代码分割策略
```javascript
// 1. 路由级分割
const HomePage = lazy(() => import('./Home'));
const AboutPage = lazy(() => import('./About'));

// 2. 组件级分割
const HeavyChart = lazy(() => import('./Chart'));

// 3. 第三方库分割
import('lodash/debounce').then(debounce => {
  window.addEventListener('resize', debounce(handler, 300));
});
```

### 防抖和节流
```javascript
// 防抖: 延迟执行
function debounce(fn, delay) {
  let timeoutId;
  return (...args) => {
    clearTimeout(timeoutId);
    timeoutId = setTimeout(() => fn(...args), delay);
  };
}

// 节流: 定期执行
function throttle(fn, limit) {
  let inThrottle;
  return (...args) => {
    if (!inThrottle) {
      fn(...args);
      inThrottle = true;
      setTimeout(() => inThrottle = false, limit);
    }
  };
}

// 应用
const handleScroll = throttle(() => {
  console.log('Scroll event');
}, 200);
```

## 场景 3: 资源优化

### 图片优化
```html
<!-- 1. 响应式图片 -->
<picture>
  <source media="(min-width: 800px)" srcset="hero-large.webp">
  <source media="(min-width: 400px)" srcset="hero-medium.webp">
  <img src="hero-small.webp" alt="Hero" loading="lazy">
</picture>

<!-- 2. 懒加载 -->
<img src="image.webp" loading="lazy" alt="Lazy loaded">

<!-- 3. WebP 格式 -->
<picture>
  <source srcset="image.webp" type="image/webp">
  <source srcset="image.jpg" type="image/jpeg">
  <img src="image.jpg" alt="Fallback">
</picture>
```

### 缓存策略
```javascript
// Service Worker 缓存
self.addEventListener('fetch', event => {
  event.respondWith(
    caches.match(event.request)
      .then(response => {
        if (response) {
          return response;
        }
        return fetch(event.request)
          .then(response => {
            return caches.open('dynamic').then(cache => {
              cache.put(event.request, response.clone());
              return response;
            });
          });
      })
  );
});
```
