# UmaDev — 企业级架构梳理

> 版本 1.0.x · 12 个 Rust crate · Rust 核心实现 · 五个底座通过受控子进程驱动
>
> 权威规范见 [`../spec/UMADEV_HOST_SPEC_V1.md`](../spec/UMADEV_HOST_SPEC_V1.md)；本文与规范冲突时以规范为准。
>
> 产品方向与实施缘由见 [`PRODUCT_VISION_AND_ROADMAP.md`](PRODUCT_VISION_AND_ROADMAP.md)；
> 该文件中的 gap/wave 表是历史路线，不是当前缺陷清单。当前成熟度快照见
> [`ENTERPRISE_MATURITY_AUDIT_2026-07-14.md`](ENTERPRISE_MATURITY_AUDIT_2026-07-14.md)。
> 本文按 crate 拆解现行工程结构——UmaDev 现在是
> **一个按任务深度模拟开发团队职责的 Agent**：协调者驱动你已经配置好的 AI 编码 CLI，按需调度
> 产品、架构、设计、前端、后端、QA、安全、DevOps 等角色会话。角色通过共享黑板和类型化裁决
> 协作，并不是八个独立的人，也不是每次小改都全员出场。**模型路由 + 可视计划 + 比例化角色调度 + 固件注入**
> 组成交付系统；治理（34 条正式 clause、113 个内容检查）是它的
> 地板安全网。

## 一句话定位

UmaDev 是**一个按风险和深度模拟真实开发团队职责的 Coding Agent**。一个协调者持有计划和验收边界，
按需让产品、架构、UI/UX、前端、后端、QA、安全、DevOps 等隔离会话读取共享黑板并返回产物或 advisory verdict。它加载你已配置的 AI 编码 CLI（恰好五个一等底座：Claude Code /
Codex / OpenCode 使用厂商专属协议驱动，Grok Build 与 Kimi Code 使用各自官方协议实现的 ACP v1 接口)的大脑当团队的脑子,协调者智能路由你的需求、拆出可视计划、注入团队心法 +
对你代码库的理解、按需调度角色并做确定性验收、留下与深度相称的证据；“AI 不能写什么”的治理
（34 条正式 clause）是底层安全网。这是软件角色编排，不等于雇佣了一支真实团队，也不保证模型输出天然达到商业交付标准。
**它自己不调任何大模型 API**,吃的是你现有的 CLI 订阅;想覆盖更多模型,是把底座路由到
第三方/本地模型,那是底座自己的事。

---

## 架构全景（12 个 crate，数据自上而下流动）

> 三个横切基础 crate 未画进下方主数据流：`umadev-i18n` 提供三语文案与系统语言检测；`umadev-state` 为 agent/TUI 提供跨平台安全持久化原语和叶子记忆策略 schema；`umadev-process` 封装 Windows Job Object 等进程树生命周期原语，`umadev-host` 本身继续禁止 unsafe code。

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
           │ 34 条 clause   │      │ 5 个一等深度适配底座         │
           │ (真相源)       │      │ 3 专属协议 + 2 隔离 ACP v1   │
           └────────────────┘      └──────────────────┘
                                            │
                                   ┌──────▼──────┐
                                   │umadev-   │
                                   │runtime      │
                                   │Runtime trait│
                                   └─────────────┘
```

### 五底座驱动层

`umadev-host` 的唯一权威清单是 `BACKEND_IDS`，长度锁定为 5，五个全部是一等深度适配底座。
实现层有两条对等协议路径：Claude Code、Codex、OpenCode 使用各自厂商专属协议驱动；
Grok Build 与 Kimi Code 使用各自厂商正式提供的 ACP v1 接口、共享的有界 JSON-RPC/stdio
协议核心，以及彼此隔离的启动、身份、认证、权限、恢复与扩展 profile。ACP 不是降级兼容层。

产品层的五个驱动都提供逻辑持续、双向的会话界面：用户在 UmaDev TUI 交互，驱动按厂商真实
能力传送后续回合，并呈现已公开或实时协商到的追问、审批、工具事件；未声明能力不会模拟。
文中的 headless / 非交互只描述后台机器协议子进程不渲染厂商自己的 TUI，不代表产品没有用户交互。

| 底座 | 会话传输 | Plan 权限边界 | 恢复语义 |
|---|---|---|---|
| Claude Code | 厂商专属双向 `stream-json` | `--permission-mode plan` | 精确 `--resume <id>` |
| Codex | `app-server` JSON-RPC | `sandbox=read-only` | `thread/resume` |
| OpenCode | loopback HTTP + SSE | 所有版本使用 deny-by-default 运行时规则集（Plan 同时禁止 delegation） | 重启 serve 后按 id 重新挂接并刷新权限 |
| Grok Build | ACP v1 | plan + read-only sandbox + 只读工具白名单 + 禁子代理 | 当前新会话交接；待生效 sandbox 证明与原生启动前校验都可验证后才启用协商出的 resume/load |

Grok 会话若在实时握手中声明 `sessionCapabilities.close`，结束时先发送稳定的
`session/close` 再有界回收子进程；未声明或无响应时直接降级到回收路径，不猜测能力、
也不让退出卡死。

登录、API key、模型和订阅都由底座自身管理。ACP 初始化会实时协商能力；未声明的认证、
权限模式或恢复能力不会被猜测。`umadev install --base ...` 是安装 UmaDev 治理 hook / pre-commit
集成，不是安装或登录底座。

Grok Build 的 headless ACP 只复用已有 cached token 或显式 `XAI_API_KEY`；`initialize` 后的
认证必须选择已存在的非交互方法。UmaDev 不主动选择 OAuth、不打开浏览器，也不代用户处理中间
授权码。交互式 `grok login` 必须由用户在 UmaDev 外自行完成。

UmaDev 自身提供 macOS（Apple Silicon/Intel）、Linux（x86_64/ARM64，glibc >= 2.31 或
musl/Alpine）和 Windows x86_64 产物，Windows on ARM 通过系统 x64 仿真运行。底座仍受各厂商平台边界约束；
Grok Build 的官方安装路径覆盖 macOS/Linux/WSL 和原生 Windows PowerShell。UmaDev 不把自身
可运行的平台冒充成底座支持平台。

## 六大支柱

| 支柱 | Crate | 职责 | 当前状态 |
|---|---|---|---|
| **规范** | umadev-spec | 34 条正式 clause；四个编号层（CODE/FLOW/ART/EVID）+ 横切 META + 9 阶段 | ✅ |
| **治理** | umadev-governance | 113 个内容检查 + API 审计 + 合规映射 + 实时 hook | ✅ |
| **知识** | umadev-knowledge | BM25 倒排索引 + 条件向量 RRF 融合 + HyDE 扩展 + 内置语料库；向量不可用时降级 BM25 | ✅ |
| **契约** | umadev-contract | 类型化 OpenAPI 3.1 + 前端一致性 + PRD 覆盖率校验 | ✅ |
| **证据** | umadev-agent | verify 真测试序列 + 可选 `--runtime` 启动证据 + 多信号 quality gate + SHA-256 哈希 | ✅ |
| **编排** | umadev-agent | 比例化 Director（最深可展开 9 阶段）+ gate + 角色裁判团 + 信任分级 + 证据门控记忆 | ✅ |

### 架构适配度守护

工作区测试会解析普通、build、dev 以及 target 条件依赖，对生产边和开发边执行独立 allowlist，
拒绝生产图成环，也拒绝 `spec/i18n/state/runtime/contract/governance/knowledge/host` 等 foundation crate
反向依赖 `agent/tui/umadev`。新增 crate 或内部依赖边必须显式完成分层评审。

四个历史热点由 `workspace_architecture` 测试中的 `HOTSPOT_LINES` 常量设置 LOC 上限；常量而非
本文数字是权威值。超过上限会使架构测试失败，拆短后应同步下调上限。TUI App 测试、治理规则
测试与完整扫描、Director 恢复和契约测试已经迁到独立子模块，四个历史主文件均已收紧基线；
后续拆分仍必须降低而不能抬高这些上限。

## Model-first 意图与权限边界

普通自然语言输入不会先被关键词强行塞进研发流程。协调者在任何写者动作之前，先让当前所选
底座的大模型在一个**全新、独立、只读**的子会话中输出类型化路由：`Chat / Explain /
QuickEdit / Debug / Build`、复杂度、写入授权、作用域、置信度与是否需要澄清。有效的模型判断
可以双向调整深度：既能识别“做网站”只是被引用的问题，也能识别一句很短的话实际是完整需求。
确定性分类只在子会话不可用、超时或返回无效结构时保守兜底，不和健康模型争夺语义判断权。

模型判断不能越过硬边界：显式只读要求、`plan` 模式、单写者锁、不可逆操作确认和治理规则
始终生效。模型路由只有结构合法且明确返回 `authorization: "mutating"` 才能获得写入面；
模型授权缺失、空白或非法时 fail-closed 到只读 Explain，不能启动写者或团队。独立的确定性
可用性兜底只能从当前用户文本中识别无歧义、窄范围的显式请求并留在常驻路径，绝不继承非法
模型字段的权限。`plan` 是独立的
执行上限，即使模型返回 Build/Debug 或 mutating，也必须先压回只读。若需要澄清，系统会在
获取写锁、创建隔离分支或把请求交给写者之前暂停。历史对话只提供有界上下文；旧计划、TODO、
运行笔记和项目文档不能替当前输入授权新工作。

单写者锁的部署前提是：协同进程使用同一个操作系统身份和稳定的用户级锁命名空间，底层
文件系统提供跨进程一致的 advisory-lock 语义。跨身份使用不受支持，安全性不作保证。一次
运行期间，工作区根路径及进入它所用的全部别名必须保持稳定：不得重命名或替换根目录，也不得
改指入口符号链接/junction。NFS/SMB 还必须具备已验证的跨客户端锁管理器，并保证运行期间
`.umadev` 命名空间不被替换（外层命名空间锁只在本机生效）。单机双进程测试不能证明这两个
网络部署前提。

| 路由 | 执行面 | 固件与验证 |
|---|---|---|
| Chat / Explain | 复用健康的只读意图子会话；不取写锁 | 身份固件；Explain 追加有界只读上下文 |
| QuickEdit / Fast Debug | 常驻单写者会话 + 写锁 | 工程实践固件 + 在最后一次代码写入后观察到成功的定向验证；进度 delta 不结算 FIFO，只有工具终态可验证；未验证写入以 `Failed` 收口，不启动 Director、角色团队、完整 QC 或完整完成卡 |
| 每个 Build / Standard、Deep Debug | Director 工作流 + 单写者会话 | 按比例生成自有计划、gate、团队、机械验收与有界 QC；Fast Build 仍可只是一个精简步骤 |

只有结构有效的健康模型路由（`RouteSource::Brain`）可以越过 Director 入口。模型会话不可用、
超时或结构无效时产生的 `RouteSource::DeterministicFallback` 留在常驻的比例化兜底路径；它不能
仅凭关键词启动 Director、角色团队或完整构建后 QC。兜底保证系统可用，但不把“无法判断”误当成
“授权重型治理”。

`plan` 不只是路由提示，也是显式执行命令的硬边界。`/run`、`/goal` 和执行型恢复在获取 run
lock、创建隔离分支、写入治理/工作流状态或启动底座写者前，统一结算为类型化的 `Planned`
非执行结果；UI 不得把它渲染成 `Done`。普通对话仍可在只读面检查项目并形成计划。

Git 普通提交是宿主独占事务，不属于产品构建：普通对话、`/run`、`/goal`、`/quick` 以及
headless `run/quick` 最终都汇入同一条冻结基线、独占 writer lock、固定 Git 调用和提交后
完整校验的宿主路径，绝不启动底座、计划、团队评审或 Docker。`提交+测试/推送/修改/amend`
等复合动作在入口本地拒绝并要求拆成两条请求；`/revise` 中的提交文本也不会被当作 gate
返工。Plan 模式在宿主事务前停止，Guarded 只请求一次当前操作确认，Auto 直接执行。

五个底座都把 `BaseSession::fork()` 实现为干净上下文，而不是复制写者历史：Claude 新建
`--session-id` 并进入厂商 plan 模式；Codex 在独立 app-server 上执行只读 `thread/start`（不使用
`thread/fork` / `thread/resume`）；OpenCode 新建 deny-by-default 会话；Grok Build 新开带只读
sandbox、只读工具白名单和子代理禁用的 Plan 会话；Kimi Code 新开 ACP 会话并选择 `plan`
模式，同时保留 UmaDev 的只读审批地板。因此“只读”既是模型结论，也是实际执行权限。

运行过程中再次输入也不会一律注入当前写者：明确的当前任务纠偏进入 steer 队列；问题，以及
下一项/含糊任务进入 FIFO 后续对话队列，待当前 run 收敛后重新走正常模型路由。gate 上的问题
由独立、只读的一次性查询回答，gate 保持打开且不会被推进或误当作返工指令；`GateOpened`
即使先到达事件流，也要等当前 writer session 完整结束并释放写者边界后才显示为可交互。
取消当前 run 会清除底座原生 resume/session hand-back，并向对话写入控制边界，避免下一轮模型
沿用已取消任务；此前排队的未来输入仍按 FIFO 各自重新路由。2 秒 cancel-drain 只控制 UI
等待时长：未真实退出的任务仍由原 handle 持有、迟到事件被丢弃、写者边界保持关闭，直到真实
结束才启动后续 FIFO；部署子进程由 drop guard 清理整个进程组。

常驻底座会话带权限快照和单调代际号。取消、权限档切换、`/clear` 与同底座会话恢复都会使旧
代际失效并清空 steer、route 和 FIFO 瞬态状态；压缩成功和失败也携带同一代际。旧异步任务即使
晚到，也不能把旧权限会话重新放入池中、裁剪新对话、推进新熔断器或写入延迟预览提示。

Director 的终态也由客观状态决定：只有计划终结且最终 QC 干净才能 `Done`。任何 Blocked、
Active/Pending/未完成计划、dirty QC，或修复轮次/时间预算耗尽后仍有残留，都必须携带有界
阻塞证据以 `Failed` 结算，不能用底座的完成自述或后续 UI 回执覆盖。防御性 gate 暂停是独立
`Paused`，显示 gate 且不渲染构建完成。

## 本轮新增能力（11 crate 协同）

这些能力已落地在 `umadev-agent`（除 RAG 升级在 `umadev-knowledge`）。检索和治理辅助可
fail-soft；评审意见不替代确定性地板，但已调度的必需评审若超时、启动失败或返回空/不可解析
结果，必须以 `Unavailable` 暂停而不能伪造通过。认证、执行、硬门和验证失败也必须显式失败或
降级，不能伪装成功：

| 能力 | 模块 | 说明 |
|---|---|---|
| **角色裁判团** | `critics` | 把角色评审统一成 `RoleVerdict` schema + `RoleCritic` trait。每个角色在全新 Plan 子会话读取黑板并返回结构化裁决；子会话不被调度为工作区写者，协调者仍会持久化裁决/账本。裁决内容不替代确定性 floor；必需评审不可用时进入可恢复的 `PausedAtOperational`，不生成源码修复、Docker 或重复自动评审任务，也不结束会话。`/continue` 只重试一次该评审边界；源码、测试、清单/锁文件、工具配置、Docker 与 CI 等 QC 输入未变时复用已通过的确定性 QC 回执。恢复后的真实 blocker 以失败证据结算，不自动重开 writer 或二次评审。不新增模型端点。 |
| **brownfield 接管** | `adopt`（`umadev adopt`） | 接管既有仓库：探测技术栈 + 恢复 test/build/lint 命令 + 索引源码到 `.umadev/project-source-index/` + 从既有前端调用反推 API 契约 + 写 `UMADEV.md` 边界简报 + 落 `adopt.json` 基线标记（后续偏向增量改而非重写）。幂等、不改用户源码。 |
| **运行时证据** | `runtime_proof`（`verify --runtime`） | 不止"能编译"——启动 dev server、对路由做 HTTP 探测，把真启动证据写 `.umadev/audit/runtime-proof.json`，并入 proof-pack。 |
| **部署闭环** | `deploy`（`umadev deploy`） | 从工件探测部署目标（Vercel / Netlify / Fly / Cloudflare Pages / 容器镜像 / 静态托管），默认只打印配方；`--run` 经你已登录的平台 CLI 真部署并写 `deploy-proof.json`。UmaDev 不持有任何凭证、不注入任何东西。 |
| **PR 模式** | `pr` / `review` / `security`（`umadev pr` / `report --review`） | `report --review` 跑 pre-PR 安全扫描并生成 PR 级评审报告；`umadev pr` 默认 dry-run（写 body + 打印计划），`--create` 才真正推送并 `gh pr create`。 |
| **信任分级** | `trust` | `TrustMode::Plan`（只读）/ `Guarded` / `Auto` 三档。CLI `run/quick --mode` 与 TUI 默认都是 Guarded；项目配置（`.umadevrc`）不能选择档位，Auto 只能由用户为当前会话开启（`/mode auto`、Shift+Tab）。无论哪档，**不可逆动作**（.git / 网络 / 破坏性 shell）都保留确认地板。 |
| **技能库** | `skills`（`umadev skill`） | 安装 / 列出 / 移除 知识+规则+prompt 技能包。 |
| **证据门控记忆** | `lessons` + `error_kb` + skills/recipes/facts/run-notes + RAG | 见下「证据门控记忆」节。 |

## 证据门控记忆

- **`/pitfalls` 事件账本**：同一隐私安全签名只在独立执行 episode 中增加发生次数；一次 stderr
  的重复行不会膨胀频率。事件保留首次/最近时间、精确修复尝试和验证状态。
- **`/lessons` 规则库**：只有独立复发形成的候选规则，或被同一 verifier 机械验证的规则才进入；
  成功、同错失败、异错/证据不足分别结算为 verified、failed、unknown，不把查看命令当学习证据。
- **复发反思**：当某踩坑在修复后仍然同错复发，向底座请求更高层的纠正策略，把 `Reflection`
  快照进 `.umadev/reflections/<signature>.jsonl`（每签名滑动窗口保留最近几条）。
- **精确知识反馈**：进入最终底座指令的知识块携带内容绑定 memory ID；底座接受后才原子提交
  sent receipt，并由后续机械 Pass/Fail/Unknown 精确结算。取消、未发送或证据不足不奖惩，崩溃后
  的未完成 outcome intent 可幂等回放；完成后立即转为不可重放状态，项目回执与全局 outcome 都
  采用有上限的保留窗口，避免历史轮次越多结算越慢或磁盘无限增长。普通非 pitfall lesson 的被动
  召回仍只读，避免宽泛因果归因。
- **类型化自纠错**：`blocker` 把步骤、团队评审和整体验收发现归为 build/contract/coverage/
  behavior/craft，并附上 `error_kb` 维护的根因与 playbook。逐指纹 tracker 同时比较源码快照；第二次
  无变化转为调查新证据，第三次无变化升级失败，有真实源码变化则重置。
- **条件双通道重排 + HyDE**：配置默认请求 BM25↔向量经 RRF（k=60）融合；只有向量后端产出可用且维度匹配的 query/store 时才真正双通道，否则只用 BM25。当底座生成 HyDE 式“假设答案”扩展时，其 BM25 排名可再与原 query 排名融合。假设答案在 agent 侧生成，`umadev-knowledge` 只负责检索与融合。

这些存储的作用域和证据门槛不同，不能统称为“每轮自动进化”：

| 资产 | 作用域 | 写入与使用边界 |
|---|---|---|
| pitfalls | 项目事故账本 `.umadev/learned/_raw/dev-errors.jsonl` | 只计独立 episode；重复 stderr 行不增频，泛化/未分类条目隔离。 |
| lessons | 可复用纠正规则与隐私安全投影 | 独立复发可形成 pending；精确 repair 后同一 verifier 通过才 validated。 |
| learned skills | 项目本地 `.umadev/memory/learned-skills/skills.jsonl` | 只有非平凡且干净的交付可毕业；检索不等于使用，需精确投递回执和 pass/fail/unknown 结算。旧 `.umadev/skills/` 只作迁移输入。 |
| recipes | 项目本地 `.umadev/memory/recipes/recipes.jsonl` | 严格 stack/kind/shape 与相似度门槛，最多召回一个，始终是 prior 而不是 gate。 |
| facts | 项目本地 `.umadev/memory/facts.jsonl` | 有意义工作后有界只读提取；过滤秘密，过期、路径失效或矛盾事实会降级/墓碑化；只在工作类回合召回。 |
| run notes | 当前 run 的 `.umadev/run-notes.md` | 计划步骤确有进展且通过确定性验收后，UmaDev 才写一条有界笔记；失败、Blocked、空评审不写，底座被明确禁止直写。 |
| open decisions | 项目可见 `docs/decisions/OPEN-DECISIONS.md` | 未决项可进入有界、不可信 prompt block；recall off 只关闭提示词召回，不隐藏登记表或计数/report。 |

run notes 只是后续计划步骤可读的有界、不可信历史，不是完整 transcript、跨项目记忆、当前授权或
完成证明。`umadev skill` 安装的知识/规则/prompt 包与 learned procedural skills 是两类资产。

统一策略由 `umadev-state` 持久化、`umadev-agent::memory_control` 按项目/全局边界解析，并在生产
调用点按叶子存储执行。capture 只控制新的自动写入，recall 只控制进入底座提示词的历史数据；两者
都不删除权威账本、不影响 inventory/report，也不截断已有 receipt settlement、trust/invalidation
治理或 run-note 生命周期轮转。Facts/recipes/复发踩坑反思在可选只读底座 consult 之前先检查
capture，因此关闭后零模型调用。策略缺失使用显式默认值；越界、不可读或损坏策略按隐私保守原则关闭 capture/recall，
而报告与生命周期账务仍可运行。用户通过 `umadev memory inventory|capture|recall|retention` 查看和调整，
只有可重建 cache 能被 `clear-cache` 物理清除。

## 9 阶段交付链（最深一招，非固定漏斗）

> 这条链是协调者为"完整商业级 greenfield 交付"路由到的**最完整路径**，不是每条输入都走的漏斗：对话不进流程，小改与快速、窄范围 Debug 走常驻轻量路径，深度 Debug 只进入与问题相称的 Director 计划；只有完整产品需求才把计划展开成这条链（router→plan→schedule→deliver，见 VISION 与 spec §9.5）。

```
research → docs → [docs_confirm gate] → spec → frontend → [preview_confirm gate]
    → backend → quality → delivery
```

Full 层级阶段：按需读知识库（BM25，向量可选）→ 组 prompt → 调 Runtime → 写工件 → maybe_verify
两道 gate：writer session 结束后才开放交互，暂停等用户 `umadev continue`；gate 上的提问走
独立只读查询，不推进 gate
质量门：多信号检查，build/test 失败 = critical，阻断 delivery（UD-EVID-003）

## 知识库 RAG 架构

```
用户需求
  ↓ pre-embed query (async, fail-open to BM25)
  ↓
retrieve_with_vector(project_root, knowledge_dir, cfg, query, phase, qvec)
  ├─ BM25: 倒排索引 over knowledge/ + 项目 .umadev/learned/ + 隐私审查后的 ~/.umadev/learned/ 安全投影
  ├─ Vector: 向量存储 .umadev/kb-index/vectors.bin (content-hash 增量缓存)
  ├─ RRF fusion: 1/(60+rank) 合并两路排名
  └─ quality_score 弱加权 → 返回 top-K chunks
```

- 配置默认 `engine = "hybrid"`，但这只是请求双通道，不是向量已执行的证明；BM25 始终是降级地板
- 本地向量要求编译 `vector-local` 且磁盘上存在可用、相互兼容的模型文件；官方 npm 启动器下载并校验同版本 Release 资产，普通源码二进制不自动下载
- 只有同时配置专用 `OPENAI_EMBED_KEY` 与 `UMADEV_ALLOW_CLOUD_EMBED=1` 才允许 HTTP embedding；普通 `OPENAI_API_KEY` 不授权上传
- 只有符合证据门槛的事故、修复/验证结果或安全投影进入 learned 检索；捕获失败、未分类错误或一次普通回合不会自动变成 lesson

## 治理双轨制

| 轨道 | 机制 | 适用宿主 |
|---|---|---|
| **原生生命周期 hook** | `umadev install --base claude-code|kimi-code` → PreToolUse/PostToolUse；Kimi 的用户级 hook 命令带绝对项目作用域，离开该根立即 fail-open | Claude Code / 所有官方 Kimi Code 版本（运行时能力协商） |
| **协议内在线裁决** | 厂商专属协议 / ACP 的类型化 permission、approval、question、plan 请求（仅在已协商且可用时） | Codex / OpenCode / Grok Build / Kimi Code |
| **事件审计 + 硬阻断** | 工具事件进入 runner audit；quality gate `passed:false` 拒绝 delivery | 所有五个底座 |

诚实承诺：Claude Code 与跟随官方源码、运行时协商能力的所有 Kimi Code 版本有 UmaDev 可安装的厂商原生
Pre/PostToolUse hook；Kimi 的配置虽在用户目录，但 UmaDev 的每一条命令只对安装时的项目根
生效，并保留用户其它 hook。其余底座能否在工具执行前在线裁决，取决于当前协议帧与握手能力；
没有 pre-apply surface 时只报告事件审计与交付硬门，绝不把 post-hoc 观察写成实时拦截。

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
engine = "hybrid"   # 请求 BM25 + 可用向量；无向量时降级 BM25
top_k = 6
```

## 运行时健壮性

- 子进程超时 → 显式 kill（防孤儿）
- stdout 256 KiB 截断（防 OOM）
- reqwest 连接池（OnceLock<reqwest::Client>）
- RuntimeError::Timeout 结构化变体（非字符串匹配）
- 检索和治理辅助失败可有界降级；advisory 评审内容不越过确定性地板，但必需评审的传输/解析失败会保存检查点并以 `PausedAtOperational` 暂停，不能改写成通过、代码缺陷或完成，也不能触发源码修复/Docker；`/continue` 只恢复该评审边界。底座不可用、认证/协议错误、硬门或验证失败必须返回可操作失败/降级，不能改写成离线模板成功

## 合规映射

运行到适用交付阶段且映射写入成功时，可生成 `output/<slug>-compliance-mapping.json`：
- 34 条正式 clause → SOC 2 / ISO 27001:2022 / EU AI Act 映射
- 关键工件 SHA-256 内容哈希（防篡改）
- `umadev report` 输出项目健康度摘要

## 验证基线

测试数量会随实现变化，因此本文不固化一个很快失真的数字。发布门禁以仓库工作流为准：
`cargo fmt --check`、workspace `clippy -D warnings`、全目标测试、doctest、all-features strict
rustdoc、Rust 1.88 MSRV、三系统测试/五目标构建、npm 分发 smoke，以及 RustSec 依赖审计。

tag 发布顺序是：

```text
版本锁 + Linux 全质量门 + macOS/Windows 测试
  → 发布凭据完整性门
  → 五目标构建、macOS Developer ID/notarization、Windows Authenticode
  → 二进制/npm 字节同一性、sidecar 与 attestation
  → GitHub 草稿上传、18 资产下载回验、公开
  → npm 七个精确 tarball staging、integrity 复核、主包最后提升 latest
  → 官网部署
```

同架构 runner 会经 JS 启动器执行待发布二进制的 `--version`；交叉架构产物只做哈希校验，仍需
真实机器抽检。GitHub、npm 与 Pages 是独立外部系统，没有分布式事务；流程通过草稿、staging、
digest 和幂等重跑缩小暴露窗口；原生代码签名仍不能替代发布后实机验收。任何一次发布的具体
结果应记录在对应 CI run，而不是用本文中的历史绿灯替代。
