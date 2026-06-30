# UmaDev — 企业级架构梳理

> 版本 1.0.x · 10 个 Rust crate · 600+ 测试 · 0 clippy 警告 · 纯 Rust 零外部进程依赖
>
> **权威产品态见 [`PRODUCT_VISION_AND_ROADMAP.md`](PRODUCT_VISION_AND_ROADMAP.md)。**
> 本文按 crate 拆解工程结构(仍现行);产品叙事以 VISION 为准——UmaDev 现在是
> **一个模拟真实开发团队工作的 Agent,指挥你已经在用的 Claude Code / Codex / OpenCode 干活**(产品经理 / 架构师 / 设计师 / 前端 / 后端 / QA / 安全 / DevOps
> 八个角色,在共享黑板上各做各的专业活),靠**智能路由 + 可视计划 + 团队调度 + 固件注入**
> 把它们拧成一个交付系统;一个**协调者席位**负责调度、把关,而非早期的"规范注入 + 事后审计"
> 治理工具(下文"一句话定位"已更新,治理仍是它的地板而非全部)。

## 一句话定位

UmaDev 是**一个模拟真实开发团队来工作的 Coding Agent**——产品经理、架构师、UI/UX 设计师、前端、后端、QA、安全、
DevOps 八个角色像真实团队一样在共享黑板上各做各的专业活;一个**协调者席位**(技术负责人,
不是头牌)负责调度与把关。它加载你已登录的 AI 编码 CLI(一等支持恰好三个底座:Claude Code /
Codex / OpenCode)的大脑当团队的脑子,协调者智能路由你的需求、拆出可视计划、注入团队心法 +
对你代码库的理解、逐步调度这八个角色并每步验收、留下交付证明——而把"AI 不能写什么"的治理
(25 条 clause)作为**背景安全网**而非全部。**你不是雇了一个总监,而是雇了一整支团队。**
**它自己不调任何大模型 API**,吃的是你现有的 CLI 订阅;想覆盖更多模型,是把底座路由到
第三方/本地模型,那是底座自己的事。

---

## 架构全景（10 个 crate，数据自上而下流动）

> 第 10 个 crate `umadev-i18n`（三语 zh-CN / zh-TW / en 文案 + 系统语言检测）为所有用户可见文本供给字符串，未画进下方数据流图。

```
┌─────────────────────────────────────────────────────────────┐
│  umadev（二进制）                                          │
│  clap CLI · hook · install · doctor · init · report         │
└────────────┬───────────────────────────────┬────────────────┘
             │                                │
     ┌───────▼────────┐           ┌──────────▼──────────┐
     │ umadev-tui  │           │ umadev-agent     │
     │ ratatui 实时UI │           │ 协调引擎:router·    │
     │                │           │ plan·firmware·调度  │
     └────────────────┘           │ gates · state ·     │
                                  │ scaffolding ·       │
                                  │ lessons · tech_debt │
                                  └──┬──────┬───────┬───┘
                   ┌──────────────────┘      │       └────────────────┐
           ┌───────▼────────┐      ┌────────▼─────────┐    ┌──────────▼────────┐
           │ umadev-     │      │ umadev-       │    │ umadev-        │
           │ governance     │      │ knowledge        │    │ contract          │
           │ 规则+审计+合规 │      │ BM25+向量 RAG    │    │ OpenAPI 3.1 层    │
           └────────────────┘      └──────────────────┘    └───────────────────┘
                   │
           ┌───────▼────────┐      ┌──────────────────┐
           │ umadev-     │      │ umadev-       │
           │ spec           │      │ host             │
           │ 25 条 clause   │      │ 子进程驱动 claude-code/codex/opencode │
           │ (真相源)       │      └──────────────────┘
           └────────────────┘              │
                                   ┌──────▼──────┐
                                   │umadev-   │
                                   │runtime      │
                                   │Runtime trait│
                                   └─────────────┘
```

## 六大支柱

| 支柱 | Crate | 职责 | 当前状态 |
|---|---|---|---|
| **规范** | umadev-spec | 25 条 clause × 4 层（CODE/FLOW/ART/EVID）+ 9 阶段 | ✅ |
| **治理** | umadev-governance | emoji/颜色/slop 检查 + API 审计 + 合规映射 + 实时 hook | ✅ |
| **知识** | umadev-knowledge | BM25 倒排索引 + 可选向量(hybrid) RRF 融合 + 370 文件语料 | ✅ |
| **契约** | umadev-contract | 类型化 OpenAPI 3.1 + 前端一致性 + PRD 覆盖率校验 | ✅ |
| **证据** | umadev-agent | verify 真测试序列 + `--runtime` 真启动证据 + quality gate 22+ 检查 + SHA-256 哈希 | ✅ |
| **编排** | umadev-agent | 9 阶段流水线 + 2 道人工 gate + 角色裁判团 + 信任分级 + 经验闭环回流 | ✅ |

## 本轮新增能力（10 crate 之上）

这些都已落地在 `umadev-agent`（除 RAG 升级在 `umadev-knowledge`），均为 fail-open：

| 能力 | 模块 | 说明 |
|---|---|---|
| **角色裁判团** | `critics` | 把团队里各角色（PM 立项 / tech-lead 文档评审 / 资深设计评审 / 协调者验收）统一成 `RoleVerdict` schema + `RoleCritic` trait。每个角色在**只读的 fork 会话**上交叉评审共享工件并返回结构化裁决。硬不变量：裁决仅供参考、永不驱动循环终止（循环仍由确定性的 gap-count + stall-counter 兜底）、绝不写盘、不新增模型端点（复用同一借来的脑）。 |
| **brownfield 接管** | `adopt`（`umadev adopt`） | 接管既有仓库：探测技术栈 + 恢复 test/build/lint 命令 + 索引源码到 `.umadev/project-source-index/` + 从既有前端调用反推 API 契约 + 写 `UMADEV.md` 边界简报 + 落 `adopt.json` 基线标记（后续偏向增量改而非重写）。幂等、不改用户源码。 |
| **运行时证据** | `runtime_proof`（`verify --runtime`） | 不止"能编译"——启动 dev server、对路由做 HTTP 探测，把真启动证据写 `.umadev/audit/runtime-proof.json`，并入 proof-pack。 |
| **部署闭环** | `deploy`（`umadev deploy`） | 从工件探测部署目标（Vercel / Netlify / Fly / Cloudflare Pages / 容器镜像 / 静态托管），默认只打印配方；`--run` 经你已登录的平台 CLI 真部署并写 `deploy-proof.json`。UmaDev 不持有任何凭证、不注入任何东西。 |
| **PR 模式** | `pr` / `review` / `security`（`umadev pr` / `report --review`） | `report --review` 跑 pre-PR 安全扫描并生成 PR 级评审报告；`umadev pr` 默认 dry-run（写 body + 打印计划），`--create` 才真正推送并 `gh pr create`。 |
| **信任分级** | `trust` | `TrustMode::Plan`（只读、只研究+规划）/ `Guarded`（默认、每道 gate 暂停）/ `Auto`（全自动）三档，经 `run --mode` 选择。**不可逆动作**（.git / 网络 / 破坏性 shell）即便 `auto` 也始终二次确认（reversibility floor）。 |
| **技能库** | `skills`（`umadev skill`） | 安装 / 列出 / 移除 知识+规则+prompt 技能包。 |
| **自我进化记忆** | `lessons` + `error_kb` + RAG | 见下「自我进化记忆」节。 |

## 自我进化记忆

- **频率驱动的踩坑库**：`error_kb` 把一次原始报错蒸馏成结构化避坑指南，`lessons` 按归一化签名去重落库——**复发即 frequency++**，频率驱动召回优先级。
- **复发反思**：当某踩坑在修复后**仍然复发**（真复发），向底座请求一条更高层的纠正**策略**（"换个不同的简单做法"），把 `Reflection` 快照进 `.umadev/reflections/<signature>.jsonl`（每签名滑动窗口保留最近几条）。
- **双通道重排 + HyDE**：检索默认 BM25↔向量经 RRF（k=60）双通道融合；当底座生成了 HyDE 式"假设答案"扩展时，其 BM25 排名再与原 query 排名 RRF 融合——用答案的词汇召回原 query 漏掉的 chunk。假设答案的生成在 agent 侧，`umadev-knowledge` 只负责融合。

## 9 阶段流水线

```
research → docs → [docs_confirm gate] → spec → frontend → [preview_confirm gate]
    → backend → quality → delivery
```

每阶段：读知识库(BM25/向量) → 组 prompt → 调 Runtime → 写工件 → maybe_verify
两道 gate：暂停等用户 `umadev continue`
质量门：22+ 检查，build/test 失败 = critical，阻断 delivery（UD-EVID-003）

## 知识库 RAG 架构

```
用户需求
  ↓ pre-embed query (async, fail-open to BM25)
  ↓
retrieve_with_vector(project_root, knowledge_dir, cfg, query, phase, qvec)
  ├─ BM25: 倒排索引 over knowledge/ + .umadev/learned/ + ~/.umadev/learned/
  ├─ Vector: 向量存储 .umadev/kb-index/vectors.bin (content-hash 增量缓存)
  ├─ RRF fusion: 1/(60+rank) 合并两路排名
  └─ quality_score 弱加权 → 返回 top-K chunks
```

- 默认 BM25（离线零依赖）
- 配置 `engine = "hybrid"` + `OPENAI_API_KEY` → 真 HTTP embedding (reqwest pooled)
- 每轮失败经验 → sediment → 索引 → 下轮 coach prompt 注入（闭环）

## 治理双轨制

| 轨道 | 机制 | 适用宿主 |
|---|---|---|
| **实时守门** | `umadev install` → Claude Code PreToolUse hook | Claude Code only |
| **硬阻断** | quality gate passed:false → 拒绝推进 delivery | 所有宿主 |

诚实承诺：Codex / OpenCode 无法实时拦截，靠硬阻断兜底。

## verify 真测试序列

```
Node:  install → lint → typecheck → test → build
Rust:  fmt --check → clippy → test → build --release
Python: install → ruff → mypy → pytest
Go:    vet → test → build
Deno:  lint → test → check
```
缺失 binary → skipped（非 fail），build/test 失败 → critical

## 配置体系

```toml
# .umadevrc（umadev init 自动生成）
[quality]
threshold = 90
skip_checks = []

[pipeline]
skip_phases = []
max_review_rounds = 3

[knowledge]
enabled = true
engine = "bm25"     # or "hybrid"
top_k = 6
```

## 运行时健壮性

- 子进程超时 → 显式 kill（防孤儿）
- stdout 256 KiB 截断（防 OOM）
- reqwest 连接池（OnceLock<reqwest::Client>）
- RuntimeError::Timeout 结构化变体（非字符串匹配）
- 所有失败 fail-open 到离线模板（永不阻塞宿主）

## 合规映射

每轮自动生成 `output/<slug>-compliance-mapping.json`：
- 25 条 clause → SOC 2 / ISO 27001:2022 / EU AI Act 映射
- 关键工件 SHA-256 内容哈希（防篡改）
- `umadev report` 输出项目健康度摘要

## 测试覆盖

- **573 单元 + 集成测试**，0 失败
- **0 clippy 警告**（`-D warnings`）
- **4 个 spec vector**（UD-CODE-001/002/003/004 conformance）
- **11 个 e2e 集成测试**（hook / install / report / doctor / pipeline）
- 端到端验证：run → continue → delivery，8 工件 + proof-pack zip
