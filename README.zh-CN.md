# umadev

<div align="center">

<img src="docs/assets/umadev-logo.png" alt="umadev" width="760">

### 一个项目总监 Agent，带领一支完整团队，把你的需求交付成可运行产品

**产品 / 架构 / 设计 / 前后端 / 测试 / 安全 / 运维——像真实开发团队那样协作。底座是大脑，umadev 是带队交付的总监。**

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.87%2B-orange)](https://www.rust-lang.org/)
[![Spec](https://img.shields.io/badge/Spec-UMADEV__HOST__SPEC__V1-blue)](spec/UMADEV_HOST_SPEC_V1.md)
[![Version](https://img.shields.io/badge/Version-1.0.7-success)](CHANGELOG.md)

[English](README.md) | 简体中文 | [繁體中文](README.zh-TW.md)

</div>

---

## 目录

- [简介](#简介) · [项目来源](#项目来源) · [它解决什么问题](#它解决什么问题)
- [安装](#安装) · [快速上手](#快速上手) · [一个完整例子](#一个完整例子)
- [umadev 如何工作](#umadev-如何工作) · [团队怎么协作](#团队怎么协作)
- [为什么可信](#为什么可信) · [运行模式](#运行模式) · [流水线设计](#流水线设计) · [质量门是什么](#质量门是什么)
- [治理规则是什么](#治理规则是什么) · [知识库是什么](#知识库是什么) · [交付产物长什么样](#交付产物长什么样)
- [**命令大全**](#命令大全) · [配置](#配置) · [Rust 架构](#rust-架构) · [开发](#开发) · [许可证](#许可证)

## 简介

umadev 是一个命令行工具，它驱动你已经在用的 AI 编码工具——Claude Code、Codex、OpenCode——作为一个持续的会话来工作。它自己不接任何模型：你的底座接入的模型，就是它的大脑。

你用自然语言描述你想要什么，umadev 驱动底座把它做出来：拆成一份计划、写代码、召集一支评审团队（产品、架构、设计、前端、后端、QA、安全、运维），跑完质量与治理检查，然后交还可运行的代码和一份交付证明。它按任务大小自动伸缩：小改动就只是小改动，完整项目才走完整流程。

它是一个 **Rust 单二进制**。npm 只是分发壳。

**总监不亲自敲代码。** 它做三件事：理解你、调度团队、把关交付。底座是干活的工程师（思考、调研、设计、写代码、评审都是底座的认知）；umadev 是带队的总监加确定性工具层（编排阶段、设置确认门、实时治理、跑质量门、留下审计证据）。

它的团队是一组可调度的评审角色，而非写死的判断逻辑：

- **产品经理** — 拆解需求、定范围与验收标准
- **架构师** — 定技术选型、分层分包、接口契约
- **UI/UX 设计师** — 定设计系统、令牌、信息架构，盯住"不像 AI 模板"
- **前端 / 后端工程师** — 真写代码（驱动底座连续用工具产出文件）
- **QA** — 真跑构建测试、查覆盖
- **安全** — 扫漏洞、查攻击面、做 pre-PR 安全检查
- **DevOps** — 管构建、CI、运行时证据、上线
- **总监** — 在每道门汇总裁决、对照计划验收、拍板过或返工

写代码的角色串行驱动主会话；评审角色各自在**只读分叉会话里并行**审，把结构化裁决交回总监。角色之间**不互相聊天**——它们只通过共享的产物文件（黑板）和结构化裁决沟通，避免多 Agent 互聊放大幻觉。总监**确定性地**汇总：把阻断项折成一条返工指令注入主会话，循环由确定性信号（覆盖 / 契约 / 验证 / 硬门）有界终止，不靠模型自评"够不够好"。

umadev 驱动恰好三个一等底座：Claude Code、Codex、OpenCode。想覆盖更多模型，是把底座路由到第三方或本地模型——那是底座自己的事，umadev 不注入、不覆盖、不持有任何模型端点。

> 持续会话是**默认**：整条流水线复用一个底座会话，上下文全程在线。过去"每阶段单发"的旧模型只作为底座会话起不来时的兜底。这让底座连续干活、真写代码，而不是九个"陌生人"各做一段，客套不动手。

## 项目来源

umadev 脱胎于原项目 [shangyankeji/super-dev](https://github.com/shangyankeji/super-dev)。

早期的 `super-dev` 更像一个 AI 编码治理工具：主要关注"AI 生成代码时不能写什么"，例如不要用 emoji 当图标、不要硬编码颜色、不要写不安全代码。

现在的 umadev 在这之上长成了一个带队交付的项目总监 Agent：

- **从单点治理扩展到全流程交付**：不只检查代码，而是把从需求到上线的每个阶段都交给对应角色，并加上门禁与验收。
- **从零散脚本升级为规范驱动系统**：核心是 [UMADEV_HOST_SPEC_V1](spec/UMADEV_HOST_SPEC_V1.md)，所有实现都围绕规范（持续会话与团队模型见 §9.3–§9.4）。
- **使用 Rust 重写**：单二进制、跨平台、启动快、依赖少、适合本地长期运行。
- **从"拦截问题"升级为"带队走完交付"**：Claude Code / Codex / OpenCode 是大脑和手，umadev 是加载这颗大脑、调度整支团队、把关交付的总监。

一句话概括这个演进：

> `super-dev` 关注"AI 不要写烂代码"；`umadev` 关注"一个 AI 项目总监如何带领一支团队，把需求交付成可上线、可审计的商业产品"。

## 它解决什么问题

很多人第一次用 AI 编码工具时都会遇到类似问题：

- AI 一上来就写代码，没有 PRD、没有架构、没有验收标准。
- 前端做完了，后端接口路径对不上。
- UI 看起来像模板，颜色和字体很随意。
- AI 写了占位代码、假数据、TODO，却说"完成了"。
- 修改一次需求后，上下文开始乱，前面约定被忘掉。
- 代码能生成，但没有质量报告、没有证据链，不知道能不能交付。
- 团队有自己的规范和知识库，但每次都要手动复制给 AI。

umadev 的目标就是把这些问题系统化解决——靠的不是更长的提示词，而是一支分工明确的团队，每个角色在该出手的节点出手：

```mermaid
flowchart LR
    A["一句需求"] --> B["产品经理<br/>拆解 + 调研"]
    B --> C["架构师 ‖ 设计师<br/>架构 + UIUX 并行"]
    C --> D{"文档确认门<br/>团队并行评审 + 你拍板"}
    D --> E["技术负责人<br/>Spec / 任务"]
    E --> F["前端工程师<br/>照设计系统实现"]
    F --> G{"预览确认门"}
    G --> H["后端工程师<br/>照契约实现"]
    H --> I["QA · 安全 · 运维<br/>质量门 / 红队"]
    I --> J["交付包<br/>总监验收 + 证据"]
```

## 安装

推荐用 npm 安装预编译二进制：

```bash
npm install -g umadev
```

npm 只是分发壳。真正运行的是 Rust 编译出的 `umadev` 二进制。

安装时还会自动附带一个小型本地嵌入模型（`multilingual-e5-small`，f16，约 224MB，作为可选依赖）并自动接好——它驱动离线向量检索，无需 API key、运行时不联网，**无需手动下载**。若你的镜像源或网络跳过了这个可选下载，umadev 仍可用：检索降级为纯 BM25，重新执行 `npm install -g umadev` 即可恢复向量通道。

支持的平台：

- macOS Apple Silicon
- macOS Intel
- Linux x86_64
- Linux ARM64
- Windows x86_64

也可以从源码构建：

```bash
git clone https://github.com/umacloud/umadev.git
cd umadev
cargo build --release --features vector-local
./target/release/umadev --version
```

> **从源码构建？嵌入模型不在仓库里（太大，约 224MB，git 放不下）。** 普通 `cargo build --release` 出来的是**纯 BM25** 版；本地向量通道需要 `--features vector-local` **加上**磁盘上的模型。预编译二进制和 `npm i` 会自动带齐这两样——源码构建则需手动把 `multilingual-e5-small` 下到 `~/.umadev/embed-model/` 一次：
>
> ```bash
> mkdir -p ~/.umadev/embed-model && cd ~/.umadev/embed-model
> for f in config.json tokenizer.json model.safetensors; do
>   curl -fsSL "https://huggingface.co/intfloat/multilingual-e5-small/resolve/main/$f" -o "$f"
> done
> ```
>
> umadev 会自动发现这个目录（或用 `UMADEV_EMBED_MODEL_DIR` 指向任意放着这三个文件的目录）。没有模型 umadev 仍可用——检索降级为纯 BM25。

你还需要装好并登录一个 AI 编码 CLI——那就是 umadev 驱动的大脑：

| 底座 | 安装 | 登录 |
|---|---|---|
| Claude Code | `npm i -g @anthropic-ai/claude-code` | `claude auth login` |
| Codex | `npm i -g @openai/codex` | `codex login` |
| OpenCode | 见 opencode.ai | `opencode auth login` |

umadev 不往底座里注入任何东西。你的底座配的是什么——官方登录，还是你自己接的第三方 / 本地模型——跑的就是什么。umadev 不保存你的登录信息；它只是把任务作为非交互命令发给底座。

## 快速上手

```bash
umadev                       # 启动对话界面；首次运行会让你选一个底座
```

第一次打开会让你选择：

1. 界面语言。
2. 使用哪个底座：Claude Code、Codex 或 OpenCode（你已经登录的那个）。

然后直接描述你要做什么：

```text
> 给报表页加上 CSV 导出
> 帮我做一个带 Postgres 后端的待办应用
> /goal 做一个能上线的 SaaS 落地页          # 持续干到目标达成
```

也可以非交互地跑一次构建：

```bash
umadev run "给报表页加上 CSV 导出" --backend claude-code
```

umadev 按你的请求自动判断工作量大小——你不用选模式。一次构建跑在独立的 `umadev/<slug>` 分支上：你的工作分支不会被动，umadev 也绝不自动合并或推送。

## 一个完整例子

假设你在一个空项目里运行：

```bash
umadev init
umadev
```

然后输入：

```text
做一个课程预约小程序，用户可以查看课程、选择时间、预约、取消预约，管理员可以管理课程和预约记录。
```

umadev 会做这些事：

1. **理清需求**：补全目标平台、是否需要支付、管理员后台复杂度等合理默认假设（`auto` 模式自动推进、不打断你；`guarded` 模式可逐条确认）。
2. **联网调研**：当底座具备联网能力时，搜索同类小程序、预约系统的竞品功能、定价、设计趋势和真实用户评价；同时检索内置知识库里的预约系统、后台 CRUD、权限、表单校验等规范。两者合并产出调研报告 `output/<slug>-research.md`。
3. 生成 PRD，明确用户角色、功能范围、EARS 可测验收标准。
4. 生成架构文档，定义数据模型、API、鉴权、部署方式。
5. 生成 UI/UX 文档，定义设计方向、颜色 token、字体、组件状态、图标库。
6. 拆成执行计划和任务（每个任务回链到需求 FR 编号）。
7. 驱动底座实现前端，渲染真实 markdown 和逐文件 diff 卡。
8. 暂停让你预览。
9. 驱动底座实现后端和集成。
10. 跑质量门：文档、契约、构建、设计、安全、交付文件全部检查。
11. 生成交付包和成绩单。

整个过程不是"AI 聊完就算完成"，而是会留下真实文件。在聊天界面里输入需求，和用 `/run` 显式发起构建，走的是同一套系统——不存在两条代码路径。

## umadev 如何工作

当你说出一句需求，umadev 不是直接把它丢给底座，而是像一位总监那样先想清楚、把计划摆给你看、带上团队规范和对你代码库的理解去干、逐步调度团队并每步验收、最后留下交付证明。一次输入会流经这几层（每一步向底座的咨询都 fail-open——失败就退回确定性兜底，绝不卡死）：

```mermaid
flowchart TB
    In["你的一句话<br/>对话 / 临时任务 / 完整需求"] --> Router["① 判断意图<br/>底座自己的模型判断：对话 / 小修 / 完整 build<br/>显示意图卡：'小改动，这就做' vs '完整构建，进研发流程'"]
    Router -->|"完整 build"| Plan["② 可视计划<br/>总监解析成依赖 DAG<br/>清单实时勾选 [✓] [~] [ ]"]
    Router -->|"小修 / 对话"| Fast["快路径<br/>不上锁、轻量"]
    Plan --> Firmware["③ 注入规范 + 你的代码库理解<br/>身份 + 反 AI 模板规范 + 知识 + 踩坑 + repo-map"]
    Firmware --> Sched["④ 逐步调度团队 + 每步验收<br/>写代码角色串行 · 评审角色并行 fork<br/>确定性地板判过/返工"]
    Sched --> Final["⑤ 交付证明<br/>PRD / 架构 / UIUX / 成绩单 / proof-pack"]
    Final --> Mem["⑥ 记忆<br/>持久化对话 + 跨会话续接 + 踩坑记录"]
    Mem -.->|"下次更懂你"| Firmware
```

**① 判断意图（router）。** 每条非斜杠输入都先由底座自己的模型判断：一个问题留在对话；"改个文案"走快路径直接做；"做一个订阅后台"升级成完整 build。判断结果**会显示给你**（意图卡），你可以用 `/run` 强制完整流程、`/quick` 强制快路径来覆盖。底座连不上时退回最轻量路径，绝不做关键词猜测。

**② 可视计划（plan）。** 一个完整 build 开工前，总监把目标拆成一份**它自己解析并持有**的依赖 DAG（落 `.umadev/plan.json`），在界面上渲染成一张**实时勾选的清单**（`[✓] 脚手架 · [~] 登录路由 · [ ] 登录表单  3/8`）。`/plan skip|add|veto|up|down <id>` 让你重排 / 跳过 / 否决 / 新增某一步，折回下一条指令同会话生效。

**③ 注入规范 + 你的代码库理解（compose_firmware + repo-map）。** 每条干活路径开跑前，umadev 给底座注入一段精选、有 token 预算的系统提示：团队**身份** + 工程**规范 / 反 AI 模板**品味 + 按需检索的**知识**摘要 + 按技术栈指纹召回的**踩坑**教训 + 你现有代码的 **repo-map 切片**（符号轮廓，按路由命中的文件排序）。知识库（418 份 markdown 文件）已编译进二进制，启动时自动解压到 `~/.umadev/knowledge`，首次回复不需要额外的冷启动时间。底座带着这些规范和对你代码库的理解干活，而不是一颗空白大脑。

**④ 逐步调度团队 + 每步验收（director_loop）。** 总监沿 DAG 一步步驱动：每个就绪的 build 步在主会话上串行执行，并对照它自己的验收标准在**确定性地板**（源码在不在 / 构建测试 / 契约 / 评审）上验证——过则勾掉清单，不过则有界返工。评审步分叉出团队并行交叉审。运行有 30 分钟墙钟预算上限；gap-count 和 stall-count 确保循环有界收敛。

**⑤ 交付证明（finalize）。** 质检通过后，`finalize` 产出 PRD / 架构 / UIUX 文档、成绩单 HTML 和打包的 proof-pack（按深度裁剪——一个小页面不会硬塞一摞企业文档）。

**⑥ 记忆（持久化对话 + 跨会话）。** umadev 每轮把自己的有界对话发给底座，并把对话**持久化**到 `.umadev/chat/<id>.json`；重开 umadev 对话还在，未完成的计划会问你"继续目标 X（第 N/M 步）？"。踩坑沉淀进自学习库，下次注入到规范里**预防**同类错误。

> 这些都是**已实现**的真实行为（见 `crates/umadev-agent` 的 `router.rs` / `plan_state.rs` / `context.rs` / `director_loop.rs` 与 `crates/umadev-knowledge/src/repomap.rs`），不是路线图承诺。权威产品态见 [`docs/PRODUCT_VISION_AND_ROADMAP.md`](docs/PRODUCT_VISION_AND_ROADMAP.md)。

整体架构可以理解成四层：

```mermaid
flowchart TB
    User["用户<br/>输入需求 / 审核 / 预览 / 部署"] --> UI["umadev TUI / CLI<br/>聊天界面 + 命令入口"]

    UI --> Director["umadev-agent<br/>流程引擎：阶段、gate、状态、质量门"]

    Director --> Spec["umadev-spec<br/>UMADEV_HOST_SPEC_V1"]
    Director --> Knowledge["umadev-knowledge<br/>本地知识库检索：BM25 + 向量"]
    Director --> Governance["umadev-governance<br/>112 条治理规则 + 审计"]
    Director --> Contract["umadev-contract<br/>OpenAPI 契约校验"]

    Director --> Runtime["umadev-runtime<br/>统一 Runtime 接口"]
    Runtime --> Host["umadev-host<br/>驱动底座 Claude / Codex / OpenCode<br/>实现代码 · 联网调研竞品"]
    Runtime --> Offline["离线模板<br/>内部 CI / 无底座兜底"]

    Host --> Workspace["项目工作区<br/>源码 / output / release"]
    Offline --> Workspace

    Governance --> Evidence[".umadev/audit<br/>审计日志"]
    Contract --> Quality["quality gate"]
    Evidence --> Quality
    Quality --> Release["proof pack<br/>scorecard"]
```

简单说：

- **TUI/CLI**：你和总监对话的地方。一个问题、一次"审下这段代码"、一个完整需求，都进同一个会话，由底座的模型判断是对话、临时任务还是开工跑完整流程。
- **总监（umadev-agent）**：路由意图、拆出可视计划、注入规范、调度哪些角色、逐步驱动并每步验收、汇总裁决推进或返工、最后 finalize 交付。完整商业级交付时会展开成 9 阶段主链（见下文"流水线设计"）。
- **持续会话 / 底座**：整条流水线复用你登录的底座（Claude Code / Codex / OpenCode）的一个会话——底座用它自己的登录和模型连续干活（调研、设计、真写代码、评审）；umadev 不注入、不覆盖任何模型或 key。
- **治理 / 质量**：底座每写一个文件就实时拦截不合规内容；交付前再跑一遍质量门补扫。
- **知识库（全本地双通道 RAG）**：418 个精选知识文件 + 你现有代码的地图编进二进制，每个工作回合由双通道混合引擎检索——纯 Rust BM25 + 本地向量模型（multilingual-e5-small, f16, candle）RRF 融合 + HyDE；无 key、无网络、零配置，把工程标准、设计系统、领域知识注入给当前阶段的角色。
- **证据**：把每次工具调用、每份裁决、每道门记录下来，最后打包成交付证明。

## 团队怎么协作

umadev 把"质量判断"建模成**一支团队**，而不是单趟检查。一个**确定性总监**主导全程；写代码的角色串行写，评审角色并行只读审；沟通只走"共享文件黑板 + 结构化裁决"。

```mermaid
flowchart TB
    User["你<br/>需求 / 确认"] --> Director["总监<br/>确定性编排核 · 唯一决策者"]
    Director -->|"send_turn · 观察事件 · 纠偏"| Main["主持续会话（底座模型）"]
    Main -->|"写代码角色串行写"| Board["黑板<br/>output/*.md · 源码"]
    Director -->|"fork() 只读分叉"| Critics["评审角色 × 8<br/>产品经理 · 架构师 · UI/UX 设计师 · 前端 · 后端 · QA · 安全 · DevOps"]
    Board -.->|"读黑板"| Critics
    Critics -->|"RoleVerdict<br/>accepts · blocking · advisory · evidence"| Director
    Floor["确定性地板<br/>覆盖 · 契约 · 验证 · 硬门"] --> Director
    Director -->|"阻断项 → 一条带证据的返工指令"| Main
    Director -->|"全过 → 过门推进"| Next["下一阶段"]
```

四条铁律保证这套团队既有力又稳定：

- **单写者**：任一时刻只有主会话在写黑板；评审分叉只读，绝不并行写，并行安全。
- **确定性控环**：循环继续还是终止，由确定性信号（gap-count、退出码、硬门）决定。底座和评审角色都是 advisory，永不驱动循环终止——避免模型"自我感觉良好"放行。
- **fail-open**：评审角色够不到底座时，空裁决等于通过，绝不阻塞；总监退回确定性地板决策。一个评审的 bug 永远不会卡住底座。
- **有界返工**：阻断项折成带上下文的返工指令注入主会话，带上下文修，再复审；gap-count 和 stall-count 确定性收敛（默认最多几轮、无进展即停），残留进自学习库下次规避。

团队规模随任务复杂度缩放：bugfix / 小重构不组队，确定性地板独立把关；完整需求才上全套八个评审角色加总监。简单需求因此能走轻量路径——跳过调研和三文档、保留 Spec 与硬门，几分钟出代码。

## 大概要多久 · 命令怎么发现

**时间预期**（实际取决于你底座的模型、思考强度和需求复杂度——umadev 不持有模型，速度主要由底座决定，这里只给量级）：

| 你说的 | 路由判成 | 大致量级 |
|---|---|---|
| 一个问题 / "这怎么用" | 对话 | 秒级，不进流程 |
| "改个文案" / "重命名这个函数" | 快路径小修 | 一两分钟，单步直接做 |
| "审下这段代码会不会出 bug" / "帮我看这个报错" | 临时任务（带 repo-map / git 锚定） | 几分钟 |
| "给用户模型加个字段" / "修结账的 bug" | bugfix / 小改（不组队） | 几分钟，硬门把关 |
| "做一个订阅管理后台" | 完整 build（展开 9 阶段、全套团队） | 按需求规模从十几分钟到更长；中途在确认门停下等你 |

> 看得见进度：完整 build 全程有**实时勾选的计划清单**和**团队评审面板**，不会"卡在 0/9 不动"；超过几秒无输出时状态栏会染红，绝不让你对着静默猜它是否卡住。`auto` 模式全自动推进，`guarded`（默认）在每道确认门停下等你拍板。

**命令可发现性**：

- TUI 里输入 `/` 弹出**命令补全浮层**，`Tab` 补全、`↑↓` 切换、回车执行。
- `/help`（或 F1）列出**全部命令和快捷键**。
- 不确定下一步时，意图卡和计划清单本身会提示可用动作（`[c] 继续 · /revise 重做 · /plan 调整`）。
- `umadev guide` 是 60 秒上手教程，`umadev examples` 是命令速查表。

## 为什么可信

总监带队听起来像把判断权交给模型——其实恰恰相反。umadev 的可信来自把"模型说了什么"和"硬信号是什么"严格分开：

- **fail-open 治理**：底座每写一个文件，都实时拦截 emoji 当图标、硬编码颜色、AI 模板痕迹、无障碍缺失、前后端契约不符等。治理函数**永远 fail-open**——治理自身出 bug 时放行而非阻断，绝不让一个治理缺陷卡死底座。
- **确定性控环**：底座和评审角色都是 advisory。真正决定"过门 / 返工 / 硬停"的是确定性信号：FR→任务覆盖、前后端契约对照、真跑 verify 的退出码、质量门阈值，以及**零代码硬门**（计划要产出代码却没有真实源码 = 判失败，绝不把空骨架伪装成"完成"）。
- **不持有模型端点**：umadev 用你已登录的底座，不自带模型、不接第三方 API、不存你的 key。底座用谁的模型、什么思考强度，umadev 只读出来显示、绝不覆盖。
- **审计证据**：每次工具调用、每份角色裁决、每道门的状态都落盘（`.umadev/audit/*`、`team-ledger.jsonl`）；交付时打包成 proof-pack + 成绩单 + 合规映射（SOC 2 / ISO 27001 / EU AI Act），可直接发给团队、客户或审计方。
- **自进化记忆**：构建中底座踩的坑带频率信号记到本地存储 → 真复发触发更高层的 reflection（纠正策略）→ 召回进后续提示，同样的坑不再犯，用得越久越懂你的代码库。
- **三语**：zh-CN / zh-TW / en 全程覆盖用户可见文案，按系统语言检测、可随时切换。

## 运行模式

### 模式 A：驱动本机 AI 编码 CLI（默认走持续会话）

这是产品模式。整条流水线复用底座的**一个持续会话**，上下文全程在线；底座连续用工具干活。

| Backend ID | 实际程序 | umadev 怎么驱动 | 适合谁 |
|---|---|---|---|
| `claude-code` | `claude` | 持续会话（stream-json 双向流），跨阶段 `--resume` 续接 | 已经在用 Claude Code 的用户 |
| `codex` | `codex` | 持续会话（`codex app-server`，同一 thread 续接） | 已经在用 Codex CLI 的用户 |
| `opencode` | `opencode` | 持续会话（`opencode serve`，同一 session 续接） | 已经在用 OpenCode 的用户 |

特点：

- umadev 不需要你的模型 API key。
- 继续使用你原来 CLI 的账号、订阅和配置。
- 底座负责真实读写文件和运行命令。
- umadev 负责团队调度、流程、规则、质量门和证据链。

> 单发兜底：底座的持续会话起不来时，自动回退到旧的"每阶段单发"路径（`claude --print` / `codex exec` / `opencode run`），保证不卡死；离线模式与显式 `UMADEV_CONTINUOUS=0` 也走单发。

### 底座自带模型 — umadev 不接外部 API

umadev 不自带模型，也不接第三方 API。底座用它自己的模型——你订阅登录的，或你给底座自己配的第三方 / 本地模型。选底座时 umadev 会读出并显示它当前用的模型和思考强度（`/status` 也能看），但**绝不覆盖**：运行时默认不传 `--model`，底座用它自己的；想换就改底座自己的配置，或用 `/model <id>` 临时覆盖这一会话。

umadev 读取的来源：claude 的 `~/.claude/settings.json`（`model` / `effortLevel`）、codex 的 `~/.codex/config.toml`（`model` / `model_reasoning_effort`）、opencode 的 `opencode.json`（`model`，思考强度内置在模型变体里）。

### 模式 B：离线模板（内部兜底，不是产品）

```text
/offline
```

离线模式不会调用任何模型，也不会访问网络。它**不是一个让你选的产品形态**——产品永远是"驱动你登录的底座"。离线模板只是没有底座可用时的确定性兜底，适合：

- 快速看文件结构。
- CI smoke test。
- 演示 umadev 的流程。

离线产物只是模板（带 TODO 占位），不代表模型完成了真实开发；真实交付必须走底座。第一次启动的底座选择器只列三个底座，不会把离线当成一个选项。

## 流水线设计

**9 阶段主链不是每个需求都走的固定漏斗，而是总监为"完整商业级 greenfield 交付"选择的最完整的交付路径。** 底座的模型先判断意图（上文"umadev 如何工作"①）：对话不进流程、小修走快路径、bugfix 不组队；只有一个完整产品需求，总监才把计划展开成下面这条主链。规范里这条链就是 `standard` profile 的全程（见 [spec §4.1 / §9.5](spec/UMADEV_HOST_SPEC_V1.md)）——是**怎么交付一个完整产品**的契约，不是"每句话都得走九步"。

规范主链是 9 个阶段：

```mermaid
flowchart LR
    R["research<br/>调研"] --> D["docs<br/>PRD / 架构 / UIUX"]
    D --> G1["docs_confirm<br/>文档确认"]
    G1 --> S["spec<br/>执行计划"]
    S --> F["frontend<br/>前端"]
    F --> G2["preview_confirm<br/>预览确认"]
    G2 --> B["backend<br/>后端"]
    B --> Q["quality<br/>质量门"]
    Q --> L["delivery<br/>交付"]
```

当前产品实现还在主链前增加了一个 `clarify` 微阶段：

```mermaid
flowchart LR
    C["clarify<br/>澄清需求"] --> R["research<br/>调研"]
    R --> D["docs"]
    D --> Rest["...后续 7 个规范阶段"]
```

所以你可能先看到 umadev 生成：

```text
output/<slug>-clarify.md
```

你可以回答澄清问题，也可以输入 `c` 跳过。

> 小任务有轻量路径：9 阶段是面向"完整商业级交付"的主链，不是每个需求都得全程走完。用 `/kind`（全栈 / 仅前端 / 仅后端 / bugfix / 重构）声明任务类型后，umadev 会据此裁剪阶段——bugfix、小改动不会强行拉你走 PRD / 架构 / UIUX 全套。

### 每个阶段由谁主导、产出什么

| 阶段 | 主导角色 | 你能理解成 | 主要文件 |
|---|---|---|---|
| `clarify` | 产品经理 | 先把需求问清楚 | `output/<slug>-clarify.md`、`output/<slug>-clarify-answers.md` |
| `research` | 产品经理 | 联网调研竞品、领域、风险、设计趋势 | `output/<slug>-research.md` |
| `docs` | 产品经理 · 架构师 · 设计师 | 写三份核心文档（架构和 UIUX 可并行） | `output/<slug>-prd.md`、`output/<slug>-architecture.md`、`output/<slug>-uiux.md` |
| `docs_confirm` | 团队并行评审 + 你 | 产品经理、架构师、设计师各自只读审，连同你一起确认方向 | `.umadev/workflow-state.json`、`.umadev/team-ledger.jsonl` |
| `spec` | 技术负责人 | 拆任务和执行计划（每任务回链 FR 编号） | `output/<slug>-execution-plan.md`、`.umadev/changes/<id>/tasks.md` |
| `frontend` | 前端工程师 | 照设计系统 / 令牌实现前端 | `output/<slug>-frontend-notes.md` |
| `preview_confirm` | 设计师 + 前端 critic + 你 | 看前端效果、审实现质量 | TUI gate 状态、`.umadev/team-ledger.jsonl` |
| `backend` | 后端工程师 | 照契约实现后端和集成 | `output/<slug>-backend-notes.md` |
| `quality` | QA · 安全 · 后端 · DevOps | 真跑构建测试 + 红队 + 契约 / 覆盖 / 治理扫描 + 硬门 | `output/<slug>-quality-gate.json`、`output/<slug>-quality-gate.md` |
| `delivery` | DevOps / 总监验收 | 对照计划验收、打包交付 | `output/<slug>-delivery-notes.md`、`release/proof-pack-*.zip`、`release/scorecard-*.html` |

## 质量门是什么

质量门可以理解成 umadev 的"交付前验收"。

它不是简单看文件在不在，而是会检查：

- PRD 有没有目标、范围、验收标准。
- 架构文档有没有 API、数据模型、错误处理、鉴权。
- UI/UX 文档有没有设计 token、字体、图标库、组件状态、暗黑模式。
- 前端调用的 API 和后端契约是否一致。
- 是否存在 emoji 图标、硬编码颜色、AI 模板痕迹。
- 是否有构建、测试、lint、typecheck 结果。
- 是否生成 Dockerfile、CI、migration、`.env.example`。
- 是否泄露 API key、密码、连接串。
- 是否有审计日志和合规映射。

输出文件：

```text
output/<slug>-quality-gate.json
output/<slug>-quality-gate.md
```

默认通过线是 90 分，可以在 `.umadevrc` 调整：

```toml
[quality]
threshold = 90
skip_checks = []
```

## 治理规则是什么

umadev 最早来自治理工具，这部分仍然是核心能力。

这些规则是一条**治理基线**，不是绝对真理——每条都可以在 `.umadev/rules.toml` 里禁用、按路径排除或调参（见下文）。它们的作用是给底座的产出兜底，而不是替你做最终的工程判断。

规范层有 25 条 clause，实现层目前有 112 条规则，覆盖：

- UI 质量：不用 emoji 当图标，不写硬编码颜色，不产出模板感 UI。
- 安全：不写密钥，不写危险命令，不写 SQL 注入、SSRF、XXE 等危险代码。
- 前端质量：不用随意 `any`、裸 `fetch`、缺少 a11y、缺少 ErrorBoundary 等。
- 后端质量：鉴权、限流、日志、事务、输入校验、错误处理。
- 多语言危险模式：Rust `unwrap`、Go `panic`、Python `bare except`、Java `System.exit` 等。

治理入口有四个：

```mermaid
flowchart LR
    A["Claude Code hook<br/>写入前拦截"] --> G["umadev-governance"]
    B["umadev ci<br/>提交前/CI 扫描"] --> G
    C["MCP server<br/>给其他 AI 工具调用"] --> G
    D["quality gate<br/>交付前补扫"] --> G
```

项目可以通过 `.umadev/rules.toml` 调整：

```toml
[disabled]
clauses = []

[exclusions]
paths = ["src/legacy/**", "**/*.test.ts"]

[extra]
blocked_domains = ["internal-bad-proxy.corp"]
```

## 知识库是什么

umadev 内置了 418 份 markdown 知识文件，不只是普通文档，而是给底座看的工程标准库。这些文件已编译进二进制，首次运行时自动解压到 `~/.umadev/knowledge`，随后每次注入不再有额外的冷启动时间。

它覆盖：

- 产品和 PRD。
- 架构和 API。
- 前端、后端、数据库。
- 安全、测试、CI/CD、运维。
- 移动端、桌面、小程序、鸿蒙、跨平台。
- 电商、金融科技、医疗、教育、游戏等行业。
- UI/UX、设计系统、设计反模式。
- 产品经理、架构师、前端负责人、后端负责人、QA、DevOps 等专家方法论。

检索方式：

```mermaid
flowchart LR
    A["用户需求"] --> B["分词<br/>中文 + 英文"]
    A --> H["HyDE 查询扩展"]
    B --> C["BM25 检索<br/>纯 Rust, CJK 友好"]
    B --> D["本地向量嵌入<br/>(candle 本地模型, f16, 离线)"]
    H --> C
    H --> D
    C --> E["RRF 融合排序"]
    D --> E
    E --> F["注入当前阶段 prompt"]
```

**双通道本地混合检索。** BM25（纯 Rust，CJK 友好）+ 稠密向量两通道经 RRF 融合，HyDE 先扩展召回再检索。向量通道用 multilingual-e5-small（f16）经 candle **本地运行** — 随安装内置，无需 API key、无网络，毫秒级召回你自己的项目规范与业务文档；模型缺失时自动降级为纯 BM25（fail-open）。它不需要、也不使用任何云端嵌入服务。

**自我进化记忆。** 构建中底座踩的坑会带频率信号记到本地存储；真正复发时 umadev 让底座给出更高层的纠正策略（reflection）；两者都会召回进后续提示，同样的坑不再重复 — 用得越久，在你的代码库上越少犯同样的错。

你也可以添加自己的团队知识：

```bash
umadev knowledge-manage add ./team-docs --name team-docs
umadev knowledge-manage search "支付 webhook 幂等"
```

## 交付产物长什么样

一次完整运行后，目录大致是：

```text
your-project/
  output/
    app-clarify.md
    app-research.md
    app-prd.md
    app-architecture.md
    app-uiux.md
    app-execution-plan.md
    app-frontend-notes.md
    app-backend-notes.md
    app-quality-gate.json
    app-quality-gate.md
    app-compliance-mapping.json
    app-delivery-notes.md

  .umadev/
    workflow-state.json
    plan.json
    audit/
      tool-calls.jsonl
      frontend-api-calls.jsonl
      verify.jsonl
    changes/
    decisions/
    history/
    chat/

  release/
    proof-pack-app-20260620090000.zip
    proof-pack-app-20260620090000.manifest.txt
    scorecard-app-20260620090000.html
```

其中：

- `output/` 是项目过程文档。
- `.umadev/` 是状态、计划和审计记录。
- `release/` 是最终交付包。

## 命令大全

umadev 有两套入口，一一对应：

- **TUI 斜杠命令** — 在 `umadev` 聊天界面里输入 `/`，日常推荐。
- **终端 CLI 子命令** — 脚本 / CI 用，无需进 TUI。

> 提示：TUI 里输入 `/` 会弹出命令补全浮层，`Tab` 补全、`↑↓` 切换；`/help`（或 F1）列出全部命令和快捷键。

### TUI 斜杠命令

**选择底座与模型**

| 命令 | 作用 |
|---|---|
| `/claude` · `/codex` · `/opencode` | 切换驱动的本机底座 CLI（存入 `~/.umadev/config.toml`）；切换时显示该底座当前的模型与思考强度 |
| `/offline` | 切到离线确定性模板（演示 / CI，完全不联网） |
| `/status` | 当前底座、它的**驱动模型**和**思考强度**（读自底座自己的配置，umadev 不覆盖） |
| `/model <id>` | 临时覆盖这一会话用的模型（默认不覆盖，底座用它自己的） |
| `/kind <类型>` | 指定任务类型（全栈 / 仅前端 / 仅后端 / bugfix / 重构），据此裁剪阶段 |

**驱动流程与过门**

| 命令 | 作用 |
|---|---|
| 直接打字 | 底座的模型判断：对话 / 小修 / 完整 build；若有确认门打开，则作为修改意见 |
| `/run <需求>` | 显式开始一次完整流水线（强制走完整 build） |
| `/goal <目标>` | 让底座持续干到目标达成（三个底座都支持；`UMADEV_NO_GOAL_MODE=1` 退出） |
| `/quick <需求>` | 强制走快路径（轻量单点改动，不展开计划） |
| `/plan [skip\|add\|veto\|up\|down <id>]` | 查看 / 步级调度当前可视计划；无参折叠面板 |
| `/continue`（门上也可直接输 `c`） | 通过当前确认门、进入下一阶段 |
| `/revise <反馈>` | 停在当前门，带反馈重做本阶段 |
| `/manual` · `/auto` | 切换"逐门人工确认 / 全自动"（默认 `guarded`；`shift+Tab` 也可切换） |
| `/redo` | 重跑上一个阶段块 |
| `/abort` · `/stop` | 中止当前运行（磁盘工作流状态保留，下次可续跑） |

**预览与交付**

| 命令 | 作用 |
|---|---|
| `/preview` | 启动前端 dev server 并打开浏览器 |
| `/stop-preview` | 停止预览服务 |
| `/deploy` | **预览**部署命令（只看不执行） |
| `/deploy confirm` | 真正执行部署 |

**检查点与回滚**（影子 git，不碰你自己的 `.git`）

| 命令 | 作用 |
|---|---|
| `/checkpoint [标签]` | 给当前工作区文件打快照 |
| `/rewind [id]` | 列出 / 回滚到某个文件检查点 |

**查看产物与状态**

| 命令 | 作用 |
|---|---|
| `/spec` | 查看完整 `UMADEV_HOST_SPEC_V1` 规范 |
| `/diff [名字]` | 查看某个产物（默认 `prd`，也可 `architecture` / `uiux` / …） |
| `/verify` | 工作区合规报告 + 证据链 |
| `/doctor` | 自检（二进制 / manifest / 探针） |
| `/status` | 当前阶段 / 门 / 运行状态 |
| `/history` | 完整对话历史 |
| `/sessions` | 列出本项目持久化的对话会话 |
| `/resume <id>` | 续接某个持久化对话会话 |
| `/usage` | token / 用量统计 |
| `/knowledge` | 本次命中的知识库条目 |
| `/skill` · `/mcp` | 已安装的 Skill / MCP server |
| `/config` | 当前生效配置 |

**设计与项目**

| 命令 | 作用 |
|---|---|
| `/design <方向>` | 锁定设计系统方向（`modern-minimal` / `editorial-clean` / …） |
| `/template <名字>` | 选脚手架模板 |
| `/name <名字>` | 设置项目 slug |
| `/init` | 写入 `umadev.yaml` manifest |

**通用**

| 命令 | 作用 |
|---|---|
| `/help`（或 F1） | 帮助浮层（含全部快捷键） |
| `/compact` | 按 token 预算压缩对话上下文（总结并折叠，替代 FIFO 截断） |
| `/clear` | 清空聊天 |
| `/export` | 导出当前会话 |
| `/quit`（或 Esc） | 退出（工作流状态已保存，可续跑） |

### 终端 CLI 子命令

**工作区生命周期**

| 命令 | 作用 |
|---|---|
| `umadev init` | 脚手架工作区（写 `umadev.yaml` + 设计系统 / 模板 / 知识库种子） |
| `umadev`（无子命令） | 启动聊天 TUI |
| `umadev doctor` | 自检 |
| `umadev verify` | 工作区合规 + 证据链状态 |
| `umadev report` | 合规映射（SOC 2 / ISO 27001 / EU AI Act） |
| `umadev history` | 列出回滚快照 |
| `umadev rollback latest` | 回滚到某快照 |
| `umadev update` | 升级 umadev 到最新版（经 npm） |
| `umadev uninstall` | 完整卸载：确认后删 `~/.umadev` + 本项目治理钩子 + 二进制（加 `--base <claude-code\|pre-commit>` 则仅卸钩子） |

**非交互运行（脚本 / CI）**

| 命令 | 作用 |
|---|---|
| `umadev run "<需求>" --backend <id>` | 跑一次流水线，停在 `docs_confirm` 门 |
| `umadev continue [--backend <id>]` | 通过当前门（自动复用上次的 `--backend`） |
| `umadev revise "<反馈>"` | 停在门，记录修改并重跑本块 |
| `umadev spec [--clauses]` | 打印规范（`--clauses` 看条款表） |

**治理 / CI**

| 命令 | 作用 |
|---|---|
| `umadev ci [--changed-only] [--report-only]` | 对工作区每个源文件跑治理（CI 模式） |
| `umadev install --base <claude-code\|pre-commit\|…>` | 把 pre-write 治理钩子装到底座 CLI 或 git pre-commit |

**平台扩展**

| 命令 | 作用 |
|---|---|
| `umadev mcp serve` | 作为 MCP server 运行——把 `govern_file` / `govern_command` 暴露给 Claude Desktop / Cursor / Continue 等 |
| `umadev mcp-manage <install\|list\|remove>` | 管理底座的 MCP server |
| `umadev skill <install\|list\|remove>` | 管理 Skill（知识 + 规则 + 提示词包） |
| `umadev knowledge-manage <add\|list\|search\|remove>` | 管理自定义知识库文档 |

**帮助**

| 命令 | 作用 |
|---|---|
| `umadev examples` | 命令速查表 |
| `umadev guide` | 60 秒上手教程 |

### 常用环境变量

| 变量 | 作用 | 默认 |
|---|---|---|
| `UMADEV_CLAUDE_BIN` / `UMADEV_CODEX_BIN` | `claude` / `codex` 二进制路径 | `claude` / `codex` |
| `UMADEV_WORKER_TIMEOUT` | 单次 worker 超时（秒） | `300` |
| `UMADEV_VERIFY_TIMEOUT_SECS` | verify 循环单次超时（秒） | `120` |
| `UMADEV_MODEL_PLAN` / `UMADEV_MODEL_BUILD` | 分阶段模型分层（等价 `/model plan\|build`） | — |
| `OPENAI_EMBED_KEY` | 启用远程向量嵌入（否则用内置离线模型 + BM25） | — |
| `XDG_CONFIG_HOME` | `config.toml` 的基目录 | `$HOME` |

## 配置

### 用户级配置

默认路径：

```text
~/.umadev/config.toml
```

示例：

```toml
backend = "claude-code"
lang = "zh-CN"
# model 默认留空 — 底座用它自己配置的模型；只有想覆盖某会话时才填（等价 /model <id>）
# model = "opus"
```

### 项目级配置

默认路径：

```text
.umadevrc
```

示例：

```toml
[quality]
threshold = 90
skip_checks = []

[pipeline]
skip_phases = []
max_review_rounds = 3
auto_approve_gates = true

[knowledge]
enabled = true
engine = "hybrid"
top_k = 6
```

## Rust 架构

umadev 是一个 10 crate Rust workspace。

| Crate | 普通话理解 | 技术职责 |
|---|---|---|
| `umadev` | 主程序 | CLI、TUI 入口、doctor、hook、CI、MCP / Skill / Knowledge 管理 |
| `umadev-spec` | 规则说明书 | `UMADEV_HOST_SPEC_V1` 的 Rust 数据（25 条 clause） |
| `umadev-governance` | 质检和红线 | 112 条治理规则、审计、策略、合规映射 |
| `umadev-agent` | 总监引擎 | 意图路由（`router`）、可视计划 DAG（`plan_state`）、规范注入（`context::compose_firmware`）、团队调度（`director::summon` + `director_loop`）、finalize 交付、gate、质量门、8 个评审角色、验收 / 有界返工、信任分级、自学习记忆；9 阶段主链是最完整的交付路径 |
| `umadev-runtime` | 统一大脑接口 | Runtime trait + 持续会话 `BaseSession` trait、Offline、能力契约 |
| `umadev-host` | 驱动外部 CLI | Claude Code / Codex / OpenCode：持续会话驱动（`*_session.rs`）+ 单发兜底 |
| `umadev-contract` | API 对账员 | OpenAPI 契约、前后端路径校验 |
| `umadev-knowledge` | 知识检索 | 纯 Rust BM25 + CJK-bigram 分词、chunk、**本地向量通道**（multilingual-e5-small, f16, candle，内置离线；远程 `OPENAI_EMBED_KEY` 仅为可选覆盖）、HyDE 查询扩展、RRF 融合；418 份文件编译进二进制 |
| `umadev-tui` | 终端界面 | ratatui 聊天 UI：真实 markdown 渲染、逐词 diff 卡、折叠工具行、构建完成卡（带可点击预览地址） |
| `umadev-i18n` | 多语言 | 简体中文、繁體中文、English |

目录结构：

```text
crates/               Rust workspace
spec/                 UMADEV_HOST_SPEC_V1 规范
knowledge/            内置知识库
docs/                 架构、产品、商业化和设计文档
docs/assets/          图片和 Mermaid 图
npm/                  npm 分发包
tests/spec_vectors/   规范测试向量
```

## 开发

要求：

- Rust 1.87+
- Cargo
- 如果要测试 npm 分发，需要 Node.js 18+

常用命令：

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

推荐阅读顺序：

1. [spec/UMADEV_HOST_SPEC_V1.md](spec/UMADEV_HOST_SPEC_V1.md)
2. [crates/umadev-spec/src/lib.rs](crates/umadev-spec/src/lib.rs)
3. [crates/umadev-agent/src/runner.rs](crates/umadev-agent/src/runner.rs)
4. [crates/umadev-governance/src/rules.rs](crates/umadev-governance/src/rules.rs)
5. [crates/umadev/src/main.rs](crates/umadev/src/main.rs)

## 许可证

MIT，见 [LICENSE](LICENSE)。
