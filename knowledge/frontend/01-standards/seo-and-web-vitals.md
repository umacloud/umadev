---
id: seo-and-web-vitals
title: SEO 与 Web Vitals 标准（商业级 · 官方）
domain: frontend
category: 01-standards
difficulty: intermediate
tags: [seo, 搜索优化, 元数据, 结构化数据, sitemap, robots, core-web-vitals, ssr, hreflang, og, 可索引, 商业级]
quality_score: 92
last_updated: 2026-06-19
---

# SEO 与 Web Vitals 标准（商业级 · 官方）

> 官网/内容站/电商/SaaS 落地页的流量很大程度靠搜索。纯底座常做出"对搜索引擎不友好"的页面（无 meta、CSR 不可索引、无结构化数据）。本标准给出 SEO 与性能要点。

## 1. 可索引与渲染

- 内容要**可被爬虫抓取**：重要内容用 **SSR/SSG/预渲染**（Next/Nuxt），不要纯客户端渲染(CSR)后才出内容（爬虫可能抓不到/延迟）。
- `robots.txt` 控制抓取；不该索引的页用 `noindex`；规范化用 **canonical** 防重复内容。
- 生成 **sitemap.xml** 并提交搜索引擎；URL 语义化、稳定、可读（不要一堆参数）。

## 1.5 URL 结构与分页（列表/博客必看，高频踩坑）

列表、博客、商品类页面的 **URL 与分页**是 SEO 重灾区。规则：

- **用路径,不用 query 参数**做分类与分页。
  - ❌ `/blog?cat=全部&page=3`（参数化、含中文、把默认态写进 URL）
  - ✅ `/blog`（全部，默认态不进 URL）· `/blog/page/2`（分页）· `/blog/category/tech`（分类）· `/blog/category/tech/page/2`
- **默认筛选态不进 URL**：「全部 / all / 默认排序」就是 `/blog` 本身,别写成 `?cat=全部`。
- **分类用英文 slug**,不用中文/拼音:`/blog/category/design`,中文标题留在 `<h1>` 与 meta。slug 稳定不可变(改了要 301)。
- **canonical 规范化**:第 1 页 canonical 指向 `/blog`(不要 `/blog/page/1`);其余每页**自指 canonical**。
- **分页衔接**:加 `<link rel="prev">`/`<link rel="next">`(或现代做法:每页独立 canonical + 唯一 `<title>` 带"第 N 页")。
- **细碎组合页防膨胀**:「分类 × 分页 × 排序 × 筛选」的长尾组合容易稀释抓取预算 → 非核心组合用 `noindex,follow` 或 canonical 回分类首页。
- **筛选/排序**这类不产生独立内容价值的状态,优先用 query 但 `noindex`,别让它进 sitemap。
- 每个真实文章/分类页进 **sitemap.xml**;分页页可不进 sitemap(靠站内链接被发现即可)。

## 2. 元数据（每页都要）

- 每页唯一的 **`<title>`**（含关键词、≤60 字）与 **`<meta name="description">`**（≤160 字，吸引点击）。
- 语义化 HTML：一个 `<h1>`、合理标题层级、`<main>/<nav>/<article>`，利于理解与可访问性。
- 图片 `alt`；链接有意义文本（非"点这里"）。

## 3. 结构化数据与社交

- 加 **结构化数据 (Schema.org / JSON-LD)**：产品、文章、面包屑、组织、FAQ 等，利于富结果(rich results)。
- **Open Graph / Twitter Card** 标签：分享到社交/IM 有标题、描述、缩略图。
- 多语言用 **hreflang** + 独立 URL（见 i18n 标准）。

## 4. 性能（Core Web Vitals 是排名因素）

- 优化 **LCP**（最大内容绘制 < 2.5s）：首屏关键资源优先、图片优化、SSR/流式。
- **CLS**（布局偏移 < 0.1）：图片/广告占位、字体不闪烁(font-display)。
- **INP**（交互响应）：减少主线程阻塞、代码分割。
- 移动友好（responsive、viewport meta）；HTTPS；快首屏。

## 5. 内容与链接

- 真实有价值内容、关键词自然布局（不堆砌）；标题/正文围绕用户意图。
- 内链结构合理；重要页可达；面包屑导航。

## 6. 反模式（出现即不合格）

- 纯 CSR 出内容、爬虫抓不到；无 title/description 或全站雷同。
- 无 sitemap/robots/canonical；URL 一堆参数不可读。
- 无结构化数据/OG；图片无 alt；多个 h1 或层级混乱。
- 性能差(LCP/CLS/INP 不达标)；不移动友好。
- 关键词堆砌、隐藏文本等黑帽手法。

## 7. 最低交付 checklist

- [ ] 重要内容 SSR/SSG 可索引；robots/canonical/noindex 正确；sitemap.xml 提交；URL 语义化。
- [ ] 每页唯一 title/description + 语义化 HTML(单 h1/层级) + 图片 alt。
- [ ] 结构化数据(JSON-LD) + Open Graph/Twitter Card + 多语言 hreflang。
- [ ] Core Web Vitals 达标(LCP<2.5s/CLS<0.1/INP 良好) + 移动友好 + HTTPS。

---
**参考（官方）**：Google Search Central(搜索基础/结构化数据)、Core Web Vitals(web.dev)、Schema.org、Open Graph、Next/Nuxt SSR/SSG。
