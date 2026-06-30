export type Lang = "zh" | "en";
export type View = "home" | "docs" | "gallery" | "changelog" | "contributors";

export type DocBlock =
  | { h: string }
  | { p: string }
  | { c: string }
  | { l: readonly string[] }
  | { cmds: readonly (readonly [string, string])[] };

export const i18n = {
  zh: {
    nav: { product: "产品首页", docs: "文档中心", gallery: "形象相册", changelog: "更新日志", contributors: "特别贡献" },
    hero: {
      badge: "v1.0.x · MIT 开源 · Rust 单二进制",
      title1: "一句需求",
      titleHi: "一支开发团队",
      title2: "交付商业级应用",
      sub: "产品经理、架构师、设计师、前端、后端、QA、安全、DevOps —— 八个专家角色像真实团队一样分工协作，借你已登录的 Claude Code / Codex / OpenCode 大脑，把一句需求做成能上线、能交付、能审计的商业级应用。独立开发者，也瞬间拥有一整支有工程纪律的团队。",
      cta1: "快速开始",
      cta2: "阅读文档",
      copy: "复制",
      copied: "已复制",
      stats: [
        ["8", "专家角色"],
        ["112", "治理规则"],
        ["418", "知识库文档"],
        ["3", "本机底座"],
      ],
    },
    trust: "驱动你已登录的本机编码底座",
    backends: ["Claude Code", "Codex CLI", "OpenCode"],
    mascots: {
      eyebrow: "认识你的 AI 开发团队 (Your AI Dev Team)",
      title: "八个专家角色，各自交付真实产物",
      desc: "不是一个黑盒，而是一支分工的团队：产品经理拆需求、架构师定契约、设计师立设计系统、前后端真建真测、QA 出运行时证明、安全审攻击面、DevOps 出部署证明。实干角色（Doer）串行写主干，评审角色（Critic）在只读分叉上并行把关，全员只通过黑板交接、绝不互聊——既拿到并行红利，又躲开多 Agent 的脆弱。八个角色共用你已登录的同一个底座大脑，无需多买一份 API Key。",
      deliversLabel: "产物",
      lead: {
        img: "/assets/umadev/mascot-thumb-lead.png",
        role: "技术负责人 · 协调者",
        desc: "团队的协调者（不是头牌）：路由意图、拆计划、按步调度这八个角色、把关每道确认门、留下完整审计证据。",
      },
      cards: [
        {
          img: "/assets/umadev/mascot-wave.png",
          role: "产品经理 / PM",
          type: "critic",
          title: "产品经理 Agent",
          produces: "output/*-prd.md",
          desc: "把需求拆清、写 PRD、定验收标准，防止 AI 需求漂移。",
          details: ["核对 PRD 验收标准", "防范功能范围漂移", "评审用户交互文案"]
        },
        {
          img: "/assets/umadev/mascot-hud-panel.png",
          role: "架构师 / Architect",
          type: "critic",
          title: "系统架构师 Agent",
          produces: ".umadev/contracts/openapi.*",
          desc: "定分层、服务边界与 API 契约——前后端的交接基准，由它锁定。",
          details: ["维护清晰模块化架构", "强校验前后端 API 契约", "依赖树循环导入审计"]
        },
        {
          img: "/assets/umadev/mascot-laptop-chair.png",
          role: "UI/UX 设计师 / Designer",
          type: "critic",
          title: "UI/UX 设计师 Agent",
          produces: "output/*-uiux.md · design tokens",
          desc: "立设计系统（字体 / 令牌 / 组件 / 页面骨架），强制反 AI 模板审美。",
          details: ["强制实施反 AI-slop 设计律", "审核亮暗 Design Tokens", "杜绝 AI 痕迹色彩渐变"]
        },
        {
          img: "/assets/umadev/mascot-point.png",
          role: "前端工程师 / Frontend",
          type: "doer",
          title: "前端工程师 Agent",
          produces: "src/ 组件 · 页面",
          desc: "主会话串行写入。按设计系统与契约真建前端，跑通运行时。",
          details: ["开发交互式前端页面", "绑定 CSS 变量 Tokens", "保障前端编译与静态导出"]
        },
        {
          img: "/assets/umadev/mascot-sit-code.png",
          role: "后端工程师 / Backend",
          type: "doer",
          title: "后端工程师 Agent",
          produces: "API · 数据模型 · 迁移",
          desc: "主会话串行写入。建数据模型、API 与业务逻辑，对齐架构契约。",
          details: ["实现 RESTful/GraphQL API", "设计稳健 DB 迁移脚本", "编写单元与集成测试"]
        },
        {
          img: "/assets/umadev/mascot-city-dashboard.png",
          role: "QA 工程师 / QA",
          type: "critic",
          title: "QA 工程师 Agent",
          produces: "runtime-proof.json",
          desc: "真跑构建 / 测试、核对覆盖、产出运行时证明，卡死 90% 覆盖率门槛。",
          details: ["拦截覆盖率 < 90% 提交", "真跑构建与测试", "启动应用、探测路由 200"]
        },
        {
          img: "/assets/umadev/peace-agent.png",
          role: "安全工程师 / Security",
          type: "critic",
          title: "安全工程师 Agent",
          produces: "安全基线 · SAST",
          desc: "语义攻击面审查：鉴权、越权、注入、密钥，PR 前基线无新高危才放行。",
          details: ["代码漏洞静态扫描 (SAST)", "鉴权 / 越权 / 注入审查", "阻断危险 shell 命令注入"]
        },
        {
          img: "/assets/umadev/mascot-run.png",
          role: "DevOps / DevOps",
          type: "doer",
          title: "DevOps 工程师 Agent",
          produces: "deploy-proof.json",
          desc: "主会话串行写入。构建、发布、部署，沉淀坑位记忆并打包交付证明。",
          details: ["CI 与本地 dev server 拨测", "捕获报错蒸馏 Lessons 记忆", "打包 SOC 2 可审计交付包"]
        }
      ]
    },
    flow: {
      eyebrow: "工作方式",
      title: "底座的模型判断这一步，真要构建才上全套系统",
      desc: "借一个常驻持续会话当大脑：它先路由这句话——闲聊 / 解释 / 小改 / 调试 / 构建。非构建快速回复；真实构建（聊天里随手提，或 /run、/goal）则自动拥有可见计划、角色团队评审、设计系统、知识库与交付证明。固定 9 阶段只是重型从零构建时它路由到的「最深打法」，不是每句话都被塞进的漏斗。",
      layers: [
        { k: "TUI / CLI", d: "你和 UmaDev 交流的地方：聊天界面 + 命令入口。" },
        { k: "团队调度（含协调者）", d: "由底座的模型判断这一步，协调者拥有并驱动可见计划，按步调度八个角色团队。" },
        { k: "Runtime / 底座", d: "把任务交给 Claude Code / Codex CLI / OpenCode 写真实代码。" },
        { k: "治理 · 质量 · 证据", d: "L0 固件常驻注入设计系统 / 知识库 / 踩坑记忆，每次调用留下审计。" },
      ],
    },
    pipe: {
      eyebrow: "最深打法",
      title: "重型从零构建的最深打法",
      desc: "这条阶段链不是每句话的漏斗，而是团队为重型从零构建路由到、并由计划展开成的「最深打法」。每一步都有负责的角色、产物、确认点和可追溯记录。点击查看每一步做什么。",
      filesLabel: "主要产物",
      gate: "确认门",
    },
    stages: [
      { n: "0", key: "clarify", label: "澄清需求", role: "先把需求问清楚，可逐条确认或一键跳过", files: ["output/<slug>-clarify.md", "output/<slug>-clarify-answers.md"], gate: false },
      { n: "1", key: "research", label: "调研", role: "联网调研竞品、领域、风险、设计趋势，叠加本地知识库", files: ["output/<slug>-research.md"], gate: false },
      { n: "2", key: "docs", label: "文档", role: "写 PRD、架构、UI/UX 三份核心文档", files: ["output/<slug>-prd.md", "output/<slug>-architecture.md", "output/<slug>-uiux.md"], gate: false },
      { n: "3", key: "docs_confirm", label: "文档确认", role: "让你确认文档方向后再继续", files: [".umadev/workflow-state.json"], gate: true },
      { n: "4", key: "spec", label: "执行计划", role: "拆任务和执行计划，每个任务回链需求 FR 编号", files: ["output/<slug>-execution-plan.md", ".umadev/changes/<id>/tasks.md"], gate: false },
      { n: "5", key: "frontend", label: "前端", role: "驱动底座实现前端，产出预览说明", files: ["output/<slug>-frontend-notes.md"], gate: false },
      { n: "6", key: "preview_confirm", label: "预览确认", role: "启动 dev server，让你亲眼看前端效果", files: ["TUI gate 状态"], gate: true },
      { n: "7", key: "backend", label: "后端", role: "实现后端、对齐契约并集成", files: ["output/<slug>-backend-notes.md"], gate: false },
      { n: "8", key: "quality", label: "质量门", role: "独立质量检查，默认 90 分通过线", files: ["output/<slug>-quality-gate.json", "output/<slug>-quality-gate.md"], gate: false },
      { n: "9", key: "delivery", label: "交付", role: "打包 proof pack 与成绩单，给团队/客户/审计方", files: ["release/proof-pack-*.zip", "release/scorecard-*.html"], gate: false },
    ],
    modes: {
      eyebrow: "运行模式",
      title: "三种本机 CLI，驱动它写真实代码",
      desc: "当前支持 Claude Code、Codex CLI、OpenCode。UmaDev 复用你已登录的本机工具，不接管账号、不保存登录信息。",
      whoLabel: "适合谁",
      callLabel: "UmaDev 如何调用",
      tabs: [
        { id: "claude-code", name: "Claude Code", bin: "claude", cmd: "claude --print --output-format text", who: "已经在用 Claude Code 的用户" },
        { id: "codex", name: "Codex CLI", bin: "codex", cmd: "codex exec --sandbox workspace-write", who: "已经在用 Codex CLI 的用户" },
        { id: "opencode", name: "OpenCode", bin: "opencode", cmd: "opencode run", who: "已经在用 OpenCode 的用户" },
      ],
      cards: [
        { title: "复用本机登录态", cmd: "/claude · /codex · /opencode", desc: "继续使用你已经登录的 Claude Code、Codex CLI 或 OpenCode，让它们负责真实读写文件与运行命令。" },
        { title: "非交互命令驱动", cmd: "subprocess", desc: "UmaDev 作为团队协调者，只负责分工编排、阶段门、治理规则和证据链；代码执行交给本机 CLI。" },
      ],
      notes: ["仅支持三种本机 CLI", "继续用你原来的账号与订阅", "底座负责真实读写文件、运行命令", "UmaDev 负责流程、规则、质量门、证据链"],
    },
    demo: { replay: "重新播放" },
    gov: {
      eyebrow: "团队凭什么交付商业级",
      title: "治理、质量门、知识库——团队交付的底座",
      desc: "光有分工还不够商业级。团队的每一次交付，都带着规则、质量门和证据链——这是它和「让 AI 随手写一段」的根本区别。",
      blocks: [
        { stat: "112", unit: "条治理规则", title: "治理", desc: "规范层 25 条 clause，实现层 112 条规则。四个入口：写入前 hook、CI 扫描、MCP server、质量门补扫。", bullets: ["不用 emoji 当图标 · 不写硬编码色", "不写密钥 · 危险命令 · 注入代码", "Rust unwrap · Go panic · Python bare except"] },
        { stat: "90", unit: "分默认通过线", title: "质量门", desc: "交付前验收，不只看文件在不在，而是逐项检查证据。", bullets: ["PRD 目标 / 范围 / 验收标准", "前端调用与后端契约是否一致", "构建 / 测试 / lint / typecheck 结果", "审计日志与合规映射"] },
        { stat: "418", unit: "份知识文档", title: "知识库", desc: "给 AI 看的工程标准库，BM25 + 可选向量混合检索，RRF 融合排序。完整语料随二进制内置，首次运行自动解压到 ~/.umadev/knowledge，零配置下发到每个用户项目。", bullets: ["产品 · 架构 · 前后端 · 数据库", "安全 · 测试 · CI/CD · 运维", "可加入团队自有知识库"] },
      ],
      compliance: "合规映射",
      standards: ["SOC 2", "ISO 27001", "EU AI Act"],
    },
    ip: {
      eyebrow: "IP 形象",
      title: "认识 Uma —— 你的 AI 队友",
      desc: "一颗会发光的终端头、一身机能风外套。Uma 是 UmaDev 的吉祥物，也是这支 AI 开发团队每个角色的人格化。",
      cards: [
        { img: "/assets/umadev/code-orbit-agent.png", cap: "代码轨道 · 知识检索" },
        { img: "/assets/umadev/workbench-agent.png", cap: "工作台 · 真实执行" },
        { img: "/assets/umadev/peace-agent.png", cap: "发布现场 · 品牌角色" },
      ],
    },
    cta: {
      title: "免费、开源，一句话召集你的团队",
      sub: "MIT 许可 · Rust 单二进制 · 本地运行。八个角色共用你已登录的 Claude Code / Codex CLI / OpenCode，不保存你的登录信息。",
      btn1: "在 GitHub 上开始",
      btn2: "阅读文档",
      note: "npm install -g umadev",
    },
    docsPage: { title: "文档中心", sub: "从安装到交付，UmaDev 的完整使用文档。", onThis: "本页内容" },
    galleryPage: { title: "形象相册", sub: "UmaDev 的 IP 形象集 —— 点击任意一张放大查看。" },
    logPage: { title: "更新日志", sub: "UmaDev 各版本的新增、改进与安全更新。", current: "最新", more: "展开其余 {n} 项", less: "收起" },
    footer: {
      blurb: "一个模拟真实开发团队工作的 Agent,指挥你已经在用的 Claude Code / Codex / OpenCode 干活，把一句需求做成能上线、可审计的商业级应用。",
      cols: [
        { h: "产品", links: [{ t: "流水线设计" }, { t: "运行模式" }, { t: "治理规则" }, { t: "质量门" }, { t: "知识库" }] },
        { h: "文档", links: [{ t: "快速体验" }, { t: "命令大全" }, { t: "配置" }, { t: "Rust 架构" }, { t: "规范 SPEC" }] },
        { h: "资源", links: [{ t: "更新日志" }, { t: "GitHub", u: "https://github.com/umacloud/umadev" }, { t: "npm", u: "https://www.npmjs.com/package/umadev" }, { t: "许可证 MIT" }, { t: "项目来源 super-dev", u: "https://github.com/shangyankeji/super-dev" }] },
      ],
      rights: "MIT 许可 · 脱胎于 super-dev · © 2026 UmaDev",
    },
    demoScript: [
      { type: "prompt", text: "做一个课程预约小程序，用户预约/取消，管理员管理课程。" },
      { type: "sys", text: "✦ 已澄清需求 · 自动模式 · 底座 claude-code" },
      { type: "stage", text: "[1/9] research   调研竞品 / 领域规范 / 真实评价…" },
      { type: "file", text: "→ output/booking-research.md" },
      { type: "stage", text: "[2/9] docs       生成 PRD · 架构 · UI/UX…" },
      { type: "file", text: "→ output/booking-prd.md · architecture.md · uiux.md" },
      { type: "stage", text: "[5/9] frontend   驱动 Claude Code 实现前端…" },
      { type: "ok", text: "✓ 12 个组件 · 0 emoji 图标 · 0 硬编码色" },
      { type: "stage", text: "[8/9] quality    质量门：契约 / 安全 / 设计 / 交付…" },
      { type: "ok", text: "✓ 质量门 94 / 100 通过（阈值 90）" },
      { type: "stage", text: "[9/9] delivery   打包 proof pack + 成绩单…" },
      { type: "file", text: "→ release/proof-pack-booking-20260620.zip" },
      { type: "done", text: "✓ 交付完成 · 证据链已归档于 .umadev/audit" },
    ],
    partners: {
      eyebrow: "合作与支持社区",
      title: "携手各大开发者与 AI 社区，推动可治理编码交付",
      list: [
        { name: "优码云", role: "AI Coding 社区", logoName: "yoma", url: "#" },
        { name: "硬核派", role: "AIGC、多元化社区", logoName: "yinghepai", url: "#" },
        { name: "跑派社区", role: "AIGC、多元化社区", logoName: "paopai", url: "#" },
        { name: "ClawTime", role: "AIGC、多元化社区", logoName: "clawtime", url: "#" },
        { name: "SEEAI", role: "AIGC、多元化社区", logoName: "seeai", url: "#" },
        { name: "GOPC", role: "AIGC、多元化社区", logoName: "gopc", url: "#" },
        { name: "辛泽", role: "AIGC、多元化社区", logoName: "xinze", url: "#" }
      ]
    },
    contributors: {
      eyebrow: "特别贡献者",
      title: "致敬特别贡献者，共同铸就 UmaDev 核心生态",
      featured: { name: "元宝", role: "核心领航贡献者", avatarKey: "yuanbao" },
      list: [
        { name: "Robin", role: "特别贡献者", avatarKey: "robin" },
        { name: "perfect", role: "特别贡献者", avatarKey: "perfect" },
        { name: "境随AI转", role: "特别贡献者", avatarKey: "jingsuiai" },
        { name: "张楠伟", role: "特别贡献者", avatarKey: "zhangnanwei" },
        { name: "慕怀", role: "特别贡献者", avatarKey: "muhuai" },
        { name: "昭文", role: "特别贡献者", avatarKey: "zhaowen" },
        { name: "枫叶", role: "特别贡献者", avatarKey: "fengye" },
        { name: "海涛", role: "特别贡献者", avatarKey: "haitao" },
        { name: "眼睛", role: "特别贡献者", avatarKey: "yanjing" },
        { name: "马辉", role: "特别贡献者", avatarKey: "mahui" }
      ]
    },
    contributorsPage: {
      title: "特别贡献荣誉殿堂",
      sub: "致敬每一位铸就 UmaDev 核心生态与可治理编码体系的大咖与贡献者"
    },
  },
  en: {
    nav: { product: "Home", docs: "Docs", gallery: "Gallery", changelog: "Changelog", contributors: "Contributors" },
    hero: {
      badge: "v1.0.x · MIT licensed · Single Rust binary",
      title1: "One coding agent",
      titleHi: "a whole dev team",
      title2: "ships products",
      sub: "Product manager, architect, designer, frontend, backend, QA, security, DevOps — eight specialists collaborate like a real team, borrowing the Claude Code / Codex / OpenCode brain you already logged into, to turn one idea into a shippable, deliverable, auditable commercial-grade app. A solo dev gets a full, disciplined team in an instant.",
      cta1: "Get started",
      cta2: "Read the docs",
      copy: "Copy",
      copied: "Copied",
      stats: [
        ["8", "Specialist roles"],
        ["112", "Governance rules"],
        ["418", "Knowledge docs"],
        ["3", "Local backends"],
      ],
    },
    trust: "Drives the local coding CLI you already logged into",
    backends: ["Claude Code", "Codex CLI", "OpenCode"],
    mascots: {
      eyebrow: "Meet Your AI Dev Team",
      title: "Eight specialists, each shipping a real artifact",
      desc: "Not a black box — a team with a division of labor: the PM scopes the need, the architect locks the contract, the designer stands up the design system, frontend and backend really build and test, QA produces the runtime proof, security audits the attack surface, DevOps ships the deploy proof. Doers write the trunk serially; critics review in parallel on read-only forks; everyone hands off through the blackboard and never chats peer-to-peer — so you get the parallel upside without multi-agent fragility. All eight roles share the one base brain you already logged into — no extra API key to buy.",
      deliversLabel: "Ships",
      lead: {
        img: "/assets/umadev/mascot-thumb-lead.png",
        role: "Tech lead · coordinator",
        desc: "The team's coordinator (not the headline): routes intent, breaks down the plan, schedules these eight roles step by step, guards every confirm gate, and leaves a full audit trail.",
      },
      cards: [
        {
          img: "/assets/umadev/mascot-wave.png",
          role: "Product Manager / PM",
          type: "critic",
          title: "Product Manager Agent",
          produces: "output/*-prd.md",
          desc: "Scopes the need, writes the PRD and acceptance criteria, and blocks AI scope creep.",
          details: ["Checks PRD acceptance criteria", "Blocks scope creep", "Audits interactive copy"]
        },
        {
          img: "/assets/umadev/mascot-hud-panel.png",
          role: "Architect / Architect",
          type: "critic",
          title: "System Architect Agent",
          produces: ".umadev/contracts/openapi.*",
          desc: "Sets the layers, service boundaries and API contract — the handoff baseline FE & BE build to.",
          details: ["Maintains modular architecture", "Enforces contract schemas", "Checks dependency tree loops"]
        },
        {
          img: "/assets/umadev/mascot-laptop-chair.png",
          role: "UI/UX Designer / Designer",
          type: "critic",
          title: "UI/UX Designer Agent",
          produces: "output/*-uiux.md · design tokens",
          desc: "Stands up the design system (type / tokens / components / page skeleton) and enforces anti-AI-slop taste.",
          details: ["Enforces anti-slop rules", "Checks css variables & tokens", "Rejects generic AI gradients"]
        },
        {
          img: "/assets/umadev/mascot-point.png",
          role: "Frontend / Frontend",
          type: "doer",
          title: "Frontend Developer Agent",
          produces: "src/ components · pages",
          desc: "Writes serially on the main session: real components against the design system and the contract, runtime verified.",
          details: ["Implements interactive pages", "Aligns CSS Design Tokens", "Ensures build and export success"]
        },
        {
          img: "/assets/umadev/mascot-sit-code.png",
          role: "Backend / Backend",
          type: "doer",
          title: "Backend Developer Agent",
          produces: "API · data model · migrations",
          desc: "Writes serially on the main session: data model, API and business logic, aligned to the architect's contract.",
          details: ["Implements REST/GraphQL APIs", "Designs migration scripts", "Writes unit and integration tests"]
        },
        {
          img: "/assets/umadev/mascot-city-dashboard.png",
          role: "QA / QA",
          type: "critic",
          title: "QA Engineer Agent",
          produces: "runtime-proof.json",
          desc: "Really runs build / tests, checks coverage and produces the runtime proof — and holds the 90% coverage floor.",
          details: ["Blocks coverage < 90%", "Runs real build & tests", "Boots the app, probes routes 200"]
        },
        {
          img: "/assets/umadev/peace-agent.png",
          role: "Security / Security",
          type: "critic",
          title: "Security Engineer Agent",
          produces: "security baseline · SAST",
          desc: "Semantic attack-surface review: auth, privilege escalation, injection, secrets — ships only when the pre-PR baseline has no new highs.",
          details: ["Static vulnerability scan (SAST)", "Auth / privilege / injection review", "Blocks shell command injection"]
        },
        {
          img: "/assets/umadev/mascot-run.png",
          role: "DevOps / DevOps",
          type: "doer",
          title: "DevOps Engineer Agent",
          produces: "deploy-proof.json",
          desc: "Writes serially on the main session: build, release, deploy — distills pitfall memory and packages the delivery proof.",
          details: ["CI + local dev-server pings", "Captures and refines DevErrors", "Assembles SOC 2 Proof Packs"]
        }
      ]
    },
    flow: {
      eyebrow: "How it works",
      title: "The brain judges the turn — a real build earns the full systems",
      desc: "Borrow one resident persistent session as the brain: it routes the turn first — chat / explain / quick-edit / debug / build. A non-build turn streams a fast reply; a real build (mentioned in chat, or via /run / /goal) automatically gets a visible plan, the role-team review, the design system, the knowledge base and a delivery proof. The fixed 9-phase chain is just the deepest play the director routes to for a heavyweight greenfield build — not a funnel every message is forced through.",
      layers: [
        { k: "TUI / CLI", d: "Where you talk to UmaDev — a chat interface plus command entry." },
        { k: "Team orchestration (+ coordinator)", d: "Lets the base's model judge the turn; the coordinator owns and drives a visible plan and schedules the eight-role team step by step." },
        { k: "Runtime / backend", d: "Hands tasks to Claude Code / Codex CLI / OpenCode to write real code." },
        { k: "Governance · quality · evidence", d: "L0 firmware always injects the design system / knowledge / pitfall memory; every call leaves an audit trail." },
      ],
    },
    pipe: {
      eyebrow: "Deepest play",
      title: "The deepest play for a heavyweight greenfield build",
      desc: "This chain is not a funnel for every message — it is the deepest play the team routes to, and the one a plan expands into, for a heavyweight greenfield build. Every step has an owning role, artifacts, gates and traceable records. Tap a step to see what it does.",
      filesLabel: "Key artifacts",
      gate: "Confirm gate",
    },
    stages: [
      { n: "0", key: "clarify", label: "Clarify", role: "Get the requirement clear first — confirm item by item or skip in one keystroke", files: ["output/<slug>-clarify.md", "output/<slug>-clarify-answers.md"], gate: false },
      { n: "1", key: "research", label: "Research", role: "Research competitors, domain, risks and design trends online, layered over the local knowledge base", files: ["output/<slug>-research.md"], gate: false },
      { n: "2", key: "docs", label: "Docs", role: "Write the three core documents: PRD, architecture, UI/UX", files: ["output/<slug>-prd.md", "output/<slug>-architecture.md", "output/<slug>-uiux.md"], gate: false },
      { n: "3", key: "docs_confirm", label: "Docs confirm", role: "You confirm the direction of the docs before moving on", files: [".umadev/workflow-state.json"], gate: true },
      { n: "4", key: "spec", label: "Execution plan", role: "Break down tasks and the execution plan; each task links back to a requirement FR id", files: ["output/<slug>-execution-plan.md", ".umadev/changes/<id>/tasks.md"], gate: false },
      { n: "5", key: "frontend", label: "Frontend", role: "Drive the backend to build the frontend and produce preview notes", files: ["output/<slug>-frontend-notes.md"], gate: false },
      { n: "6", key: "preview_confirm", label: "Preview confirm", role: "Start the dev server so you see the frontend with your own eyes", files: ["TUI gate state"], gate: true },
      { n: "7", key: "Backend", label: "Backend", role: "Implement the backend, align the contract and integrate", files: ["output/<slug>-backend-notes.md"], gate: false },
      { n: "8", key: "quality", label: "Quality gate", role: "Independent quality check; default pass line is 90", files: ["output/<slug>-quality-gate.json", "output/<slug>-quality-gate.md"], gate: false },
      { n: "9", key: "delivery", label: "Delivery", role: "Package the proof pack and scorecard for your team, client or auditor", files: ["release/proof-pack-*.zip", "release/scorecard-*.html"], gate: false },
    ],
    modes: {
      eyebrow: "Run modes",
      title: "Three local CLIs that write real code",
      desc: "Current support is Claude Code, Codex CLI and OpenCode. UmaDev reuses your logged-in local tool; it does not take over accounts or store logins.",
      whoLabel: "Best for",
      callLabel: "How UmaDev calls it",
      tabs: [
        { id: "claude-code", name: "Claude Code", bin: "claude", cmd: "claude --print --output-format text", who: "People already using Claude Code" },
        { id: "codex", name: "Codex CLI", bin: "codex", cmd: "codex exec --sandbox workspace-write", who: "People already using the Codex CLI" },
        { id: "opencode", name: "OpenCode", bin: "opencode", cmd: "opencode run", who: "People already using OpenCode" },
      ],
      cards: [
        { title: "Reuse local login state", cmd: "/claude · /codex · /opencode", desc: "Keep using the Claude Code, Codex CLI or OpenCode account you already logged into; those tools do the real file writes and commands." },
        { title: "Non-interactive command driving", cmd: "subprocess", desc: "UmaDev acts as the team's coordinator for the division of labor, gates, governance and evidence; code execution stays in the local CLI." },
      ],
      notes: ["Only three local CLIs are supported", "Keep your existing account & subscription", "The backend reads/writes real files & runs commands", "UmaDev owns flow, rules, quality gate & evidence"],
    },
    demo: { replay: "Replay" },
    gov: {
      eyebrow: "How the team ships commercial-grade",
      title: "Governance, quality gate, knowledge — the floor under the team's delivery",
      desc: "A division of labor alone isn't commercial-grade. Every delivery from the team ships with rules, a quality gate and an evidence chain — the difference between this and “let the AI knock something out.”",
      blocks: [
        { stat: "112", unit: "governance rules", title: "Governance", desc: "25 spec clauses, 112 implementation rules. Four entry points: pre-write hook, CI scan, MCP server, quality-gate sweep.", bullets: ["No emoji icons · no hardcoded colors", "No secrets · dangerous commands · injection", "Rust unwrap · Go panic · Python bare except"] },
        { stat: "90", unit: "default pass score", title: "Quality gate", desc: "Pre-delivery acceptance — not just “does the file exist”, but evidence checked item by item.", bullets: ["PRD goals / scope / acceptance criteria", "Frontend calls match the backend contract", "Build / test / lint / typecheck results", "Audit logs and compliance mapping"] },
        { stat: "418", unit: "knowledge docs", title: "Knowledge base", desc: "An engineering-standards library for the AI — hybrid BM25 + optional vector retrieval, RRF fused ranking. The full corpus is bundled into the binary and auto-extracted to ~/.umadev/knowledge on first run, so it reaches every user project with zero config.", bullets: ["Product · architecture · FE/BE · database", "Security · testing · CI/CD · ops", "Add your team’s own knowledge"] },
      ],
      compliance: "Compliance mapping",
      standards: ["SOC 2", "ISO 27001", "EU AI Act"],
    },
    ip: {
      eyebrow: "Brand IP",
      title: "Meet Uma — your AI teammate",
      desc: "A glowing terminal head and a techwear jacket. Uma is UmaDev’s mascot, and the personification of every role on this AI development team.",
      cards: [
        { img: "/assets/umadev/code-orbit-agent.png", cap: "Code orbit · retrieval" },
        { img: "/assets/umadev/workbench-agent.png", cap: "Workbench · real execution" },
        { img: "/assets/umadev/peace-agent.png", cap: "Launch scene · brand character" },
      ],
    },
    cta: {
      title: "Free, open source — one sentence to assemble your team",
      sub: "MIT licensed · single Rust binary · runs locally. All eight roles share the Claude Code / Codex CLI / OpenCode you already logged into, and it stores no logins.",
      btn1: "Start on GitHub",
      btn2: "Read the docs",
      note: "npm install -g umadev",
    },
    docsPage: { title: "Documentation", sub: "From install to delivery — the complete UmaDev guide.", onThis: "On this page" },
    galleryPage: { title: "Mascot gallery", sub: "The UmaDev IP mascot set — click any image to enlarge." },
    logPage: { title: "Changelog", sub: "Every UmaDev release — what was added, improved and secured.", current: "Latest", more: "Show {n} more", less: "Show less" },
    footer: {
      blurb: "A coding agent that works like a real dev team, commanding the Claude Code / Codex / OpenCode you already use, turning one idea into a shippable, auditable, commercial-grade app.",
      cols: [
        { h: "Product", links: [{ t: "Pipeline" }, { t: "Run modes" }, { t: "Governance" }, { t: "Quality gate" }, { t: "Knowledge base" }] },
        { h: "Docs", links: [{ t: "Quick start" }, { t: "Command reference" }, { t: "Configuration" }, { t: "Rust architecture" }, { t: "Spec" }] },
        { h: "Resources", links: [{ t: "Changelog" }, { t: "GitHub", u: "https://github.com/umacloud/umadev" }, { t: "npm", u: "https://www.npmjs.com/package/umadev" }, { t: "MIT license" }, { t: "Origin: super-dev", u: "https://github.com/shangyankeji/super-dev" }] },
      ],
      rights: "MIT licensed · evolved from super-dev · © 2026 UmaDev",
    },
    demoScript: [
      { type: "prompt", text: "Build a class-booking app: users book/cancel, admins manage classes." },
      { type: "sys", text: "✦ Requirement clarified · auto mode · backend claude-code" },
      { type: "stage", text: "[1/9] research   competitors / domain specs / real reviews…" },
      { type: "file", text: "→ output/booking-research.md" },
      { type: "stage", text: "[2/9] docs       generating PRD · architecture · UI/UX…" },
      { type: "file", text: "→ output/booking-prd.md · architecture.md · uiux.md" },
      { type: "stage", text: "[5/9] frontend   driving Claude Code to build the UI…" },
      { type: "ok", text: "✓ 12 components · 0 emoji icons · 0 hardcoded colors" },
      { type: "stage", text: "[8/9] quality    gate: contract / security / design / delivery…" },
      { type: "ok", text: "✓ Quality gate 94 / 100 passed (threshold 90)" },
      { type: "stage", text: "[9/9] delivery   packaging proof pack + scorecard…" },
      { type: "file", text: "→ release/proof-pack-booking-20260620.zip" },
      { type: "done", text: "✓ Delivered · evidence chain archived in .umadev/audit" },
    ],
    partners: {
      eyebrow: "PARTNERS & COMMUNITIES",
      title: "Empowering Governable AI Coding with Tech Communities",
      list: [
        { name: "YoMa Cloud", role: "AI Coding Community", logoName: "yoma", url: "#" },
        { name: "YingHePai", role: "AIGC & Diversified Community", logoName: "yinghepai", url: "#" },
        { name: "PaoPai Community", role: "AIGC & Diversified Community", logoName: "paopai", url: "#" },
        { name: "ClawTime", role: "AIGC & Diversified Community", logoName: "clawtime", url: "#" },
        { name: "SEEAI", role: "AIGC & Diversified Community", logoName: "seeai", url: "#" },
        { name: "GOPC", role: "AIGC & Diversified Community", logoName: "gopc", url: "#" },
        { name: "XinZe", role: "AIGC & Diversified Community", logoName: "xinze", url: "#" }
      ]
    },
    contributors: {
      eyebrow: "SPECIAL CONTRIBUTORS",
      title: "Honoring Special Contributors Shaping the Core UmaDev Ecosystem",
      featured: { name: "YuanBao", role: "Core Lead Contributor", avatarKey: "yuanbao" },
      list: [
        { name: "Robin", role: "Special Contributor", avatarKey: "robin" },
        { name: "perfect", role: "Special Contributor", avatarKey: "perfect" },
        { name: "JingSuiAI", role: "Special Contributor", avatarKey: "jingsuiai" },
        { name: "ZhangNanWei", role: "Special Contributor", avatarKey: "zhangnanwei" },
        { name: "MuHuai", role: "Special Contributor", avatarKey: "muhuai" },
        { name: "ZhaoWen", role: "Special Contributor", avatarKey: "zhaowen" },
        { name: "FengYe", role: "Special Contributor", avatarKey: "fengye" },
        { name: "HaiTao", role: "Special Contributor", avatarKey: "haitao" },
        { name: "YanJing", role: "Special Contributor", avatarKey: "yanjing" },
        { name: "MaHui", role: "Special Contributor", avatarKey: "mahui" }
      ]
    },
    contributorsPage: {
      title: "Special Contributors Hall of Fame",
      sub: "Honoring visionary leaders and contributors building the core UmaDev ecosystem"
    },
  },
} as const;

export const docs = {
  zh: [
    {
      cat: "开始使用",
      items: [
        {
          id: "quickstart",
          title: "快速开始",
          blocks: [
            { p: "UmaDev 是一支本地运行的 AI 开发团队 Agent —— 八个专家角色由一个协调者调度。推荐用 npm 安装预编译二进制，npm 只是分发壳，真正运行的是 Rust 编译出的 umadev 二进制。" },
            { c: "npm install -g umadev" },
            { p: "支持 macOS（Apple Silicon / Intel）、Linux（x86_64 / ARM64）、Windows x86_64。也可以从源码构建：" },
            { c: "git clone https://github.com/umacloud/umadev.git\ncd umadev\ncargo build --release\n./target/release/umadev --version" },
            { h: "准备一个 AI 编码底座" },
            { p: "UmaDev 推荐驱动你已经登录的 CLI，三选一即可，然后按它们自己的方式登录。UmaDev 不保存你的登录信息，只是把任务作为非交互命令发给它们。" },
            { c: "npm install -g @anthropic-ai/claude-code\nnpm install -g @openai/codex\nnpm install -g opencode-ai" },
            { h: "初始化项目" },
            { c: "cd your-project\numadev init" },
            { h: "预览和交付" },
            { c: "/preview     # 前端阶段完成后预览\n/deploy      # 交付阶段完成后部署" },
            { p: "最终交付证据在 output/、release/、.umadev/audit/。其中 proof-pack.zip 和 scorecard.html 是给团队、客户或审计方看的交付证明。" },
          ] satisfies DocBlock[],
        },
        {
          id: "example",
          title: "一个完整例子",
          blocks: [
            { p: "在一个空项目里运行 umadev init 然后 umadev，输入：" },
            { c: "做一个课程预约小程序，用户可以查看课程、选择时间、预约、取消预约，管理员可以管理课程和预约记录。" },
            { p: "UmaDev 会依次：理清需求 → 联网调研竞品 → 生成 PRD → 生成架构 → 生成 UI/UX → 拆执行计划 → 实现前端 → 暂停预览 → 实现后端 → 跑质量门 → 生成交付包。" },
          ] satisfies DocBlock[],
        },
      ],
    },
    {
      cat: "核心概念",
      items: [
        { id: "how", title: "UmaDev 如何工作", blocks: [{ p: "整体架构可以理解成四层：TUI/CLI 是你和 UmaDev 交流的地方；团队协调者决定现在哪个角色做哪个阶段、何时暂停继续；Runtime/底座把任务交给 Claude Code / Codex CLI / OpenCode 写真实代码；治理/质量/证据检查产物是否合规并打包交付。" }] satisfies DocBlock[] },
        { id: "quality", title: "质量门是什么", blocks: [{ p: "质量门是交付前验收，不只是看文件是否存在，而是检查 PRD、架构、UI/UX、前后端契约、构建测试结果、密钥泄露风险、审计日志和合规映射。" }, { c: "[quality]\nthreshold = 90\nskip_checks = []" }] satisfies DocBlock[] },
        { id: "knowledge", title: "知识库是什么", blocks: [{ p: "UmaDev 内置 416 份 markdown 知识文件，覆盖产品、架构、前后端、数据库、安全、测试、CI/CD、运维、移动端、行业和专家方法论。" }, { c: "umadev knowledge-manage add ./team-docs --name team-docs\numadev knowledge-manage search \"支付 webhook 幂等\"" }] satisfies DocBlock[] },
      ],
    },
    {
      cat: "配置与能力",
      items: [
        { id: "config", title: "配置文件", blocks: [{ p: "首次运行会写入 ~/.umadev/config.toml(语言 + 默认底座);项目级可用 .umadevrc 覆盖。质量门阈值与跳过项放在项目根配置里。" }, { c: "# ~/.umadev/config.toml\nbackend = \"claude-code\"\nlang = \"zh-CN\"" }] satisfies DocBlock[] },
        { id: "env", title: "环境变量", blocks: [{ cmds: [["UMADEV_WORKER_TIMEOUT", "单次底座调用超时(秒)"], ["UMADEV_MODEL_BUILD", "前端 / 后端阶段用的模型(覆盖)"], ["UMADEV_MODEL_PLAN", "调研 / 文档 / 质量阶段用的模型(覆盖)"]] }] satisfies DocBlock[] },
        { id: "model-share", title: "底座与模型共享", blocks: [{ p: "UmaDev 不持有模型端点。它驱动你已登录的底座 CLI,自动读取并沿用底座当前配置的模型与推理强度——不强加任何 --model。底座用官方登录还是接了第三方 / 本地模型,跑的就是那个。" }] satisfies DocBlock[] },
        { id: "design", title: "不像 AI 生成的 UI", blocks: [{ p: "前端阶段强制使用 UIUX 文档声明的设计系统:图标库、设计 token、字体、组件骨架。一套反 AI-slop 设计法把命名禁令(默认 indigo、紫渐变、emoji 图标、虚构指标、模板骨架)做成硬规则;设计审查对照它,不符合就自动打回重做。" }] satisfies DocBlock[] },
      ],
    },
    {
      cat: "流水线详解",
      items: [
        { id: "phases", title: "九个阶段", blocks: [{ p: "UmaDev 把「AI 写代码」拆成九个有序阶段,每个阶段产出真实文件,关键节点设人在环确认门。" }, { cmds: [["1 research", "联网调研竞品 / 领域规范 / 真实评价"], ["2 docs", "并发生成 PRD · 架构 · UI/UX 三份核心文档"], ["3 docs_confirm", "确认门:你确认文档方向后再继续"], ["4 spec", "拆执行计划与任务清单"], ["5 frontend", "驱动底座实现前端,带设计一致性审查"], ["6 preview_confirm", "确认门:预览前端后再继续"], ["7 backend", "实现后端,带前后端契约校验"], ["8 quality", "质量门:契约 / 安全 / 设计 / 构建测试"], ["9 delivery", "打包 proof pack 与成绩单"]] }] satisfies DocBlock[] },
        { id: "gates", title: "确认门与人在环", blocks: [{ p: "两道确认门(docs_confirm、preview_confirm)让你在文档方向与前端预览两个关键点确认后再继续,而不是 AI 一口气跑完。门处可 /continue 通过,或 /revise 带反馈重做本阶段。" }] satisfies DocBlock[] },
        { id: "revise", title: "重做、修订与回滚", blocks: [{ cmds: [["/revise <反馈>", "带具体反馈重做当前阶段"], ["/continue", "通过当前确认门进入下一阶段"], ["umadev rollback", "回滚到某阶段的文件快照重来"]] }] satisfies DocBlock[] },
      ],
    },
    {
      cat: "交付与合规",
      items: [
        { id: "proofpack", title: "交付证据包", blocks: [{ p: "交付阶段把整个开发过程打包成可审计的证据:产物文档、构建测试结果、治理审计日志、质量门成绩单——给团队 / 客户 / 审计方一份「这是怎么做出来的」的完整证明。" }, { c: "release/proof-pack-<slug>-<date>.zip\nrelease/scorecard-<slug>.html\n.umadev/audit/*.jsonl" }] satisfies DocBlock[] },
        { id: "compliance", title: "合规映射", blocks: [{ p: "治理证据(UD-EVID-004)自动映射到 SOC 2、ISO 27001、EU AI Act 的相关条目,让交付物天然带合规线索,而不是事后补材料。" }] satisfies DocBlock[] },
        { id: "scorecard", title: "质量门成绩单", blocks: [{ p: "质量门不是「文件在不在」,而是逐项打分:PRD / 架构 / UI-UX 完整度、前后端契约对齐、构建测试结果、密钥泄露、审计日志、合规映射。低于阈值(默认 90)不放行,生成 scorecard.html 可视化成绩单。" }] satisfies DocBlock[] },
      ],
    },
    {
      cat: "集成与排查",
      items: [
        { id: "mcp", title: "作为 MCP server", blocks: [{ p: "UmaDev 可作为 MCP server 运行,把治理能力(govern_file / govern_command)暴露给其它支持 MCP 的工具,让它们写文件前也过同一套规则。" }, { c: "umadev mcp serve" }] satisfies DocBlock[] },
        { id: "ci", title: "在 CI 里跑治理", blocks: [{ p: "把治理放进 CI:对改动的源文件跑同一套规则(禁 emoji 图标 / 硬编码颜色 / 密钥泄露 / AI 套话),不合规则 CI 失败。" }, { c: "umadev ci" }] satisfies DocBlock[] },
        { id: "faq", title: "常见问题", blocks: [{ p: "Q:需要 API key 吗? 不需要——UmaDev 驱动你已登录的底座 CLI,用的是它自己的订阅 / 登录。" }, { p: "Q:底座超时 / 没响应? 用 /doctor 自检底座是否在 PATH 且已登录;可用 UMADEV_WORKER_TIMEOUT 调超时,或 /offline 临时切离线模板继续。" }, { p: "Q:产物存在哪? output/(文档与代码笔记)、release/(交付包与成绩单)、.umadev/audit/(审计证据链)。" }] satisfies DocBlock[] },
      ],
    },
    {
      cat: "命令大全",
      items: [
        { id: "tui", title: "TUI 斜杠命令", blocks: [{ cmds: [["/claude · /codex · /opencode", "切换驱动的本机底座 CLI"], ["/continue", "通过当前确认门"], ["/revise <反馈>", "带反馈重做本阶段"], ["/preview", "启动前端 dev server"], ["/verify", "合规报告 + 证据链"]] }] satisfies DocBlock[] },
        { id: "cli", title: "终端 CLI 子命令", blocks: [{ cmds: [["umadev init", "脚手架工作区"], ["umadev", "启动聊天 TUI"], ["umadev doctor", "自检"], ["umadev verify", "合规 + 证据链状态"], ["umadev ci", "对源文件跑治理"], ["umadev mcp serve", "作为 MCP server 运行"]] }] satisfies DocBlock[] },
      ],
    },
  ],
  en: [
    {
      cat: "Getting started",
      items: [
        {
          id: "quickstart",
          title: "Quick start",
          blocks: [
            { p: "UmaDev is a locally-run AI development team agent — eight specialists scheduled by one coordinator. Install the prebuilt binary with npm; npm is just the distribution shell, while the actual binary is Rust-compiled." },
            { c: "npm install -g umadev" },
            { p: "Supports macOS Apple Silicon / Intel, Linux x86_64 / ARM64, and Windows x86_64. Or build from source:" },
            { c: "git clone https://github.com/umacloud/umadev.git\ncd umadev\ncargo build --release\n./target/release/umadev --version" },
            { h: "Prepare an AI coding backend" },
            { p: "UmaDev drives a CLI you already logged into. Pick one of Claude Code, Codex, or OpenCode, then log in their own way." },
            { c: "npm install -g @anthropic-ai/claude-code\nnpm install -g @openai/codex\nnpm install -g opencode-ai" },
            { h: "Preview and deliver" },
            { c: "/preview     # after frontend\n/deploy      # after delivery" },
          ] satisfies DocBlock[],
        },
        { id: "example", title: "A full example", blocks: [{ p: "Enter one requirement, and UmaDev clarifies it, researches competitors, writes PRD / architecture / UI/UX, creates the execution plan, builds frontend and backend, runs the quality gate, and produces a proof pack." }] satisfies DocBlock[] },
      ],
    },
    {
      cat: "Concepts",
      items: [
        { id: "how", title: "How UmaDev works", blocks: [{ p: "Think of it as four layers: TUI/CLI, the team coordinator, runtime/backend, and governance/quality/evidence. The backend writes real code while UmaDev owns the division of labor, gates, rules and delivery evidence." }] satisfies DocBlock[] },
        { id: "quality", title: "What the quality gate is", blocks: [{ p: "The quality gate verifies PRD, architecture, UI/UX, FE/BE contract alignment, build/test/lint/typecheck results, secret leaks, audit logs and compliance mapping." }, { c: "[quality]\nthreshold = 90\nskip_checks = []" }] satisfies DocBlock[] },
        { id: "knowledge", title: "What the knowledge base is", blocks: [{ p: "UmaDev ships 416 markdown knowledge files: product, architecture, frontend, backend, data, security, testing, CI/CD, operations, industries and expert methodologies." }, { c: "umadev knowledge-manage add ./team-docs --name team-docs\numadev knowledge-manage search \"payment webhook idempotency\"" }] satisfies DocBlock[] },
      ],
    },
    {
      cat: "Configuration & capabilities",
      items: [
        { id: "config", title: "Config files", blocks: [{ p: "First run writes ~/.umadev/config.toml (language + default backend); a project-level .umadevrc overrides it. Quality-gate threshold and skips live in the project config." }, { c: "# ~/.umadev/config.toml\nbackend = \"claude-code\"\nlang = \"en\"" }] satisfies DocBlock[] },
        { id: "env", title: "Environment variables", blocks: [{ cmds: [["UMADEV_WORKER_TIMEOUT", "Per backend-call timeout (seconds)"], ["UMADEV_MODEL_BUILD", "Model for the frontend / backend phases (override)"], ["UMADEV_MODEL_PLAN", "Model for the research / docs / quality phases (override)"]] }] satisfies DocBlock[] },
        { id: "model-share", title: "Backends & model sharing", blocks: [{ p: "UmaDev owns no model endpoint. It drives your already-logged-in backend CLI and reuses whatever model and reasoning effort that CLI is configured with — it imposes no --model. Whether the base uses its official login or your own third-party / local model, that is exactly what runs." }] satisfies DocBlock[] },
        { id: "design", title: "UI that doesn't look AI-generated", blocks: [{ p: "The frontend phase binds the design system declared in the UI/UX doc: icon library, design tokens, typography, component skeleton. An anti-AI-slop design law turns named bans (default indigo, purple gradients, emoji icons, invented metrics, template skeletons) into hard rules; the design review checks against it and auto-rejects UI that drifts." }] satisfies DocBlock[] },
      ],
    },
    {
      cat: "The pipeline in detail",
      items: [
        { id: "phases", title: "The nine phases", blocks: [{ p: "UmaDev splits \"AI writes code\" into nine ordered phases. Each produces real files, with human-in-the-loop gates at the key moments." }, { cmds: [["1 research", "Competitors / domain standards / real reviews"], ["2 docs", "PRD · architecture · UI/UX, drafted concurrently"], ["3 docs_confirm", "Gate: you confirm the docs direction"], ["4 spec", "Execution plan + task breakdown"], ["5 frontend", "Backend builds the frontend, with a design-conformance review"], ["6 preview_confirm", "Gate: preview the frontend, then continue"], ["7 backend", "Backend code, with FE/BE contract validation"], ["8 quality", "Quality gate: contract / security / design / build-test"], ["9 delivery", "Package the proof pack + scorecard"]] }] satisfies DocBlock[] },
        { id: "gates", title: "Gates & human-in-the-loop", blocks: [{ p: "Two gates (docs_confirm, preview_confirm) let you confirm the docs direction and the frontend preview before continuing, instead of the AI running end-to-end blind. At a gate: /continue to pass, or /revise with feedback to redo the phase." }] satisfies DocBlock[] },
        { id: "revise", title: "Revise, redo & rollback", blocks: [{ cmds: [["/revise <feedback>", "Redo the current phase with specific feedback"], ["/continue", "Pass the current gate into the next phase"], ["umadev rollback", "Roll back to a phase file snapshot and redo"]] }] satisfies DocBlock[] },
      ],
    },
    {
      cat: "Delivery & compliance",
      items: [
        { id: "proofpack", title: "Delivery proof pack", blocks: [{ p: "The delivery phase packages the whole process into auditable evidence: artifact docs, build/test results, governance audit logs and the quality-gate scorecard — proof of how this was built, for your team, client or auditor." }, { c: "release/proof-pack-<slug>-<date>.zip\nrelease/scorecard-<slug>.html\n.umadev/audit/*.jsonl" }] satisfies DocBlock[] },
        { id: "compliance", title: "Compliance mapping", blocks: [{ p: "Governance evidence (UD-EVID-004) maps automatically to SOC 2, ISO 27001 and EU AI Act clauses, so deliverables carry compliance signals natively rather than as an afterthought." }] satisfies DocBlock[] },
        { id: "scorecard", title: "Quality-gate scorecard", blocks: [{ p: "The quality gate is not \"do files exist\" — it scores each item: PRD / architecture / UI-UX completeness, FE/BE contract alignment, build/test results, secret leaks, audit logs, compliance mapping. Below the threshold (default 90) it does not pass, and it renders a scorecard.html." }] satisfies DocBlock[] },
      ],
    },
    {
      cat: "Integration & troubleshooting",
      items: [
        { id: "mcp", title: "Run as an MCP server", blocks: [{ p: "UmaDev can run as an MCP server, exposing governance (govern_file / govern_command) to other MCP-capable tools so they pass the same rules before writing files." }, { c: "umadev mcp serve" }] satisfies DocBlock[] },
        { id: "ci", title: "Governance in CI", blocks: [{ p: "Put governance in CI: run the same rules over changed source files (no emoji icons / hardcoded colors / secret leaks / AI-slop); CI fails on a violation." }, { c: "umadev ci" }] satisfies DocBlock[] },
        { id: "faq", title: "FAQ", blocks: [{ p: "Q: Do I need an API key? No — UmaDev drives your already-logged-in backend CLI and uses its own subscription / login." }, { p: "Q: The backend times out / hangs? Run /doctor to check it is on PATH and logged in; tune UMADEV_WORKER_TIMEOUT, or /offline to fall back to templates." }, { p: "Q: Where are the outputs? output/ (docs + code notes), release/ (delivery pack + scorecard), .umadev/audit/ (audit evidence chain)." }] satisfies DocBlock[] },
      ],
    },
    {
      cat: "Commands",
      items: [
        { id: "tui", title: "TUI slash commands", blocks: [{ cmds: [["/claude · /codex · /opencode", "Switch local backend CLI"], ["/continue", "Pass the current gate"], ["/revise <feedback>", "Redo with feedback"], ["/preview", "Start the frontend dev server"], ["/verify", "Compliance report + evidence chain"]] }] satisfies DocBlock[] },
        { id: "cli", title: "Terminal CLI subcommands", blocks: [{ cmds: [["umadev init", "Scaffold a workspace"], ["umadev", "Start the chat TUI"], ["umadev doctor", "Self-check"], ["umadev verify", "Compliance + evidence status"], ["umadev ci", "Run governance on files"], ["umadev mcp serve", "Run as an MCP server"]] }] satisfies DocBlock[] },
      ],
    },
  ],
} as const;

export const releases = {
  zh: [
        { ver: "1.0.20", date: "2026-07-01", current: true, title: "Windows 全修 · 定位升级 · 安全 / RAG / 并发硬化", changes: [["变更", "更凝练的定位:UmaDev 指挥你已经在用的 Claude Code / Codex / OpenCode 干活(README / 文档 / 官网 / npm 全量铺开)"], ["修复", "Windows 预览开发服务器能起来了(此前 sh 硬编码、npm.cmd 找不到、彻底死掉);/preview 与 web 构建后的自动预览都通了"], ["修复", "不可逆命令信任地板认得 Windows 动词(del / rd / format / Remove-Item),Auto 档不再跳过确认"], ["修复", "Windows 控制台不再花屏:历史召回 / clear 后强制整帧重绘;/exit 与 /quit 不再让 PowerShell 不可用(完整逆序终端恢复);拖入的图片路径不再被反斜杠当转义吞掉"], ["CI", "新增 PTY 启动冒烟拉起真二进制 + windows 测试转绿,启动崩溃再也无法静默发布"], ["修复", "图片 / 粘贴 chip 作为一个整体删除与编辑(此前逐字删、提交时静默丢图);聊天轮进行中切底座不再泄漏旧会话;未终结的括号粘贴不再卡死输入"], ["修复", "未完成的有意构建不再发出干净的交付证明包,断言被掏空不再当绿过,外加 8 个路由 / 门 / 覆盖正确性修复"], ["修复", "本地 fp16 语义层不再在任何 curated 段超 512 token 时静默全死(截断 + 逐条隔离);向量通道阶段过滤 + 质量分重排 + 围栏感知 chunker,共 7 个 RAG 检索修复"], ["安全", "自有 SAST 抓得到人们真正泄漏的密钥:空格 / JSON-key 赋值 + 真熵兜底、OpenAI sk- / PEM 私钥、.env/config/IaC 文件、更多 token 家族,且扫 0 文件绝不报 Clean;pr --create 只暂存本次产物(此前 git add -A 扫进整个脏树)"], ["修复", "一处 HIGH UB 数据竞争(运行时改 env vs 并发驱动 getenv)换成线程安全共享状态;自学习记忆文件丢失更新竞争用一把共享锁收口;外加 6 个 CLI/MCP 修复(MCP slug 穿越守卫等)"]] },
        { ver: "1.0.19", date: "2026-06-30", current: false, title: "紧急修复 · 启动崩溃(1.0.17/1.0.18 退化)", changes: [["修复", "致命退化:1.0.17/1.0.18 一启动就 panic、应用完全无法运行 —— tokio::select! 的分支表达式每轮都会被求值(if 守卫只控制是否 poll、不阻止求值),取消-drain 分支在 1.0.17 的 M1 修复里被从惰性 async 块改成了直接函数调用 drain_cancelled_task(cancel_drain.as_mut().expect(…), …),于是空闲时 cancel_drain 为 None、启动第一轮循环即 .expect() panic;现改回惰性 async 块、仅在守卫为真真正 poll 时才访问 cancel_drain,并新增 PTY 启动冒烟验证。请所有 1.0.17 / 1.0.18 用户尽快升级到 1.0.19"]] },
        { ver: "1.0.18", date: "2026-06-30", current: false, title: "前沿强化五连 · 用户反馈全修(端口冲突 / 过程日志 / 信任)", changes: [["新增", "每步可证伪的证据契约(前沿 F1):'完成'不再是粗粒度的整仓检查 —— 大脑在计划 JSON 里逐步声明证据(文件存在 / 含某串 / 构建干净 / 测试通过 / 路由响应),UmaDev 解析并拥有,任一 gap 即该步未完成、且精确指出缺哪个文件 / 测试 / 路由;空证据回落既有验收(fail-open)"], ["新增", "不确定即失败关闭的不可逆动作边界 + 连败熔断(前沿 F3):一个躲过 token 扫描的混淆命令(base64 解码管道进 sh、eval $(...)、内联 -c 解释器、\\x 字节串)此前在 Auto/Guarded 被静默放行 —— 现归为 Reversibility::Uncertain、每档都强制升级确认、绝不记忆自动放行;另加连败熔断(同类构建 / 评审验证连失 3 次即收尾,不再磨到 32 步上限,且不假报成功)"], ["改进", "裁判开全新独立只读会话(更深的 F2):此前 claude 经 --resume + --fork-session、codex 经 thread/fork 分叉,只读裁判继承了 doer 的全部推敲(maker-checker 推理泄漏);现 claude 起全新 --session-id(无 resume / fork)、codex 经 thread/start 开只读新线程,裁判在宿主层就在真正干净的上下文上评审,F2 prompt 防火墙变双保险"], ["改进", "注入的记忆增量手册字节有界(前沿 F4):记忆层本就是去重蒸馏的增量手册(非原始 episode、按频率 × 新近排序、聚类成高层规则),但 relevant_lessons_for_prompt 只限条数不限字节 —— 直接注入的调用方(runner / director_loop)会被 3 条肥增量撑爆上下文;现加 3000 字硬预算,高分优先、溢出丢低分、必要时截断单条,对每个调用方按条数 + 字节双重有界"], ["改进", "KV 缓存稳定的固件前缀钉死 + 计划进度复述(前沿 F5):固件本就稳定块在前、易变块在后(逐块确定性排序、前缀里无 HashMap 迭代 / 时间戳),现加模块文档 + STABLE→VOLATILE 边界注释 + 锁测试钉死字节级前缀不变;另加有界的一行计划进度复述('M 步已完成 N 步;接下来:后两步标题')串进每轮 / 每次返工指令,长多步构建里底座不再跑偏"], ["修复", "预览开发服务器在端口被占时不再卡死(用户实测 2899s 卡死 + 6 次重跑 npm run dev):子进程此前 stdout/stderr=null,UmaDev 对'端口 3000 被占 → 改用 3002'/'已有 dev server 在跑'完全失明、还对假定的 3000 端口探测(被陈旧进程秒答即误报 Verified);现捕获并扫描输出('改用端口 Y'重指探测 URL、提取真实绑定端口),单个 READY 截止内文本就绪经 curl 确认,有界启动而非挂死,仅回收 .umadev/preview.pid 里自己记录的陈旧 PID(绝不杀外部进程),外部服务器仍答则复用不重开"], ["修复", "/logs 保留长构建的尾部而非头部:进程日志可见(16KiB 上限)时,长构建的累计输出此前被头部截断 —— 超过上限后每帧钉在同一段前 16KiB(实时流冻住)、最终结果裁掉了报错所在的尾部(用户实测的 Maven/Spring 场景);新增 truncate_preview 在 verbose 下保留最后 max 字符(字符边界安全 + 干净行首 + 尾部标记),流推进且报错幸存,接入三家底座的进程日志路径"], ["修复", "信任档不再误拦 sha256sum / lint 管道 + AskUserQuestion 真接线:'| sh' 此前是 '| sha256sum'/'| shuf'/'| shellcheck' 的子串,使只读的 'cat dist/app.js | sha256sum'(校验 / 发布 / lint)被判 Uncertain → 在 headless Auto/Guarded 被拒;改为把管道目标当整 token 匹配(sh/bash/zsh 等以空白 / 元字符界定),真 '| sh' 仍 Uncertain、$( 也纳入替换检查;另根治 AskUserQuestion 中继死代码 —— 用户回 '1' 此前发裸 '1' 可能被底座误读,现新增 PendingAskHolder 在下一轮把数字解析成选项标签 + 框成用户的明确回答再发"], ["CI", "发布工作流重试 HuggingFace 模型下载:'拉取 + 量化嵌入模型'步骤在 1.0.17 发布时撞上 HuggingFace 429(curl -fsSL 无重试),致使 5 个平台构建 + npm 发布全成功却'publish github release'失败、要手动重跑;三个模型下载改用 curl --retry 5 --retry-delay 15 --retry-all-errors,瞬时限流自愈"]] },
        { ver: "1.0.17", date: "2026-06-30", current: false, title: "用户反馈全修 · 本地 RAG 复活 · 全面硬化", changes: [["修复", "doctor 检测缺失 CLAUDE_CODE_OAUTH_TOKEN(用户反馈 401):claude login 后探测显示已登录,但 UmaDev 非交互跑 claude --print 要的是 headless 凭证、否则运行时 401 —— 现 claude-code 底座无 headless 凭证给 WARN + 指引 claude setup-token,有则 PASS,其它底座不误报"], ["修复", "本地 fp16 语义 RAG 复活:本地模型出 384 维向量但 VectorStore 钉死 1536 维、search 拒掉每个查询且守卫抓不到 —— 每次 npm install 静默只跑 BM25、宣传的本地语义层从不生效;现按真实嵌入宽度打标 + 读本地后端真实 hidden_size,端到端 384 维一致,本地双轨 RAG 真正可用"], ["修复", "复制 / 粘贴 + 输入卡死根治(用户反馈):粘贴结束标记被 50ms 的 lone-ESC flush 劈开时输入框会永久吞键(退格 / 方向 / ESC / 历史全死)—— 现两个分支都在字符边界安全地识别并闭合粘贴;输入框拖选选不中时加一次性三语提示指向 Shift+drag / /mouse"], ["新增", "底座长进程日志可见 · /logs(用户反馈):一个开关串流底座的多分钟构建 / 进程输出 —— 此前 codex 的 item/started·updated 帧被忽略、多分钟 Maven/Spring 构建期间零输出;开启后立即显示运行指示 + 增量串流,完成上限 200 字放宽到 16KiB,默认关"], ["新增", "AskUserQuestion 桥接到用户(用户反馈):底座自己 headless 跑提问、渲染不了选择器会静默自动取消 —— 现渲染问题 + 每个编号选项 + 提示「回复你的选择,会转发给底座、在等你回答而非取消」,回复作为下一轮续进同一会话,三家底座通用"], ["新增", "记忆主动记录(用户反馈):.umadev/memory/facts.jsonl 此前只指示底座写、底座常不写导致文件从不出现 —— 现工作轮后在只读 fork 上让大脑枚举本轮持久事实(key:value)、去重落盘,门控跳过聊天 / 解释且节流省 token,fail-open"], ["新增", "被构建 App 的运行时模型可配(用户反馈):不再把开发底座的厂商硬编码成被构建 App 的运行时 LLM —— 识别 App 是否运行时调模型 + 用户指定的模型(Qwen / DeepSeek / 智谱 / 月之暗面 / 文心 / 豆包 / Gemini / 本地 Ollama 等),按 provider 抽象 + env 注入、默认用户选的模型;另加「中文导出不乱码」知识标准(CSV BOM / xlsx / PDF 嵌字 / Content-Disposition RFC 5987·6266)"], ["修复", "全面自审硬化:宿主每个子进程 await 有界(泄漏的孙进程不再卡死借脑)· 聊天面真 UI/greenfield 构建强落评审团 + 门(不再裸底座零评审发货)· Plan/Guarded 信任漏洞(链式读命令夹带、树外绝对路径写自动放行)· 治理 catch_unwind 兜底 + 颜色 / emoji / slop 假阳修正 + 审计轮转竞态 · 契约门不再对描述性表头空过(零端点假过)"], ["修复", "TUI 生命周期一批:中止排干截止时间(不再永久卡「停止中」)· 排队 steer 错误中止恢复(假亮的 queued N 不再永留)· !-shell OOM / 孤儿(限上限 + 超时杀回收)· tab 粘贴保留缩进 · rewind 截断完整转录"], ["改进", "F2 裁判独立性 + 输入 UX + 官网:角色裁判在干净上下文上评审、不带 doer 推理(评审 fork 此前继承主会话带入 doer 全部思考、自偏好泄漏,现防火墙前置只判产物 + 验收 + 需求)· T7 结构化确认门选择器 · Ctrl+R 反向历史搜索 + fzf 排序 · 软换行感知复制 · 官网简洁大厂风标题 + 修手机端轮播溢出"]] },
        { ver: "1.0.16", date: "2026-06-29", current: false, title: "开发团队架构做实 · 记忆不丢 · 一批强化", changes: [["新增", "记忆不丢(双保险之一·持久事实):新增 .umadev/memory/facts.jsonl —— 底座发现的项目事实(JDK17 在哪个路径、构建 / 测试命令、环境约束)每轮注入固件头部,无论转录被裁剪还是底座自己上下文轮换都还在,从此永不重新查找(根治用户实测的「记了又重查」)"], ["改进", "记忆不丢(双保险之二·智能压缩):token 超预算时把早期轮次在只读 fork 上做结构化摘要(意图 / 涉及文件 / 关键决策 / 错误修复 / 待办 / 当前工作),近期尾巴逐字保留,替掉过去有损的 16 条 FIFO + 160 字 /compact;完整逐字转录始终落盘、/resume 无损还原、连续 3 次摘要失败即熔断 fail-open"], ["修复", "写文档不再烧 token:借底座大脑先判「写一份文档 vs 做文档描述的那个产品」—— 写 PRD / 设计文档 / 调研报告是轻触(至多 1 席产品经理过目),不再上 8 席团队 + 多轮评审 + 完整流程;真做文档平台 / 产品的构建一字不变(has_heavy_signal 守住)。并修了源码存在性地板 —— 它过去对纯文档伪报「无代码失败」、逼底座去写本不需要的代码白烧多轮,现已文档感知。根因是之前脑判了意图却仍用关键词表给构建定规模,现在大脑定规模为主、关键词只兜底"], ["新增", "开发团队架构做实 · Wave A 智能席位建造:完整构建按 router 自动判定逐角色真建造(产品 → 架构 → 设计 → 前后端 → QA → 安全 → DevOps,每角色真建自己那摊),小任务仍走单轮便宜路径 —— 全自动判,不让用户选"], ["新增", "开发团队架构做实 · Wave B 角色真产物:design-tokens 升为一等交付物 + DesignTokensPresent 验收;契约优先 DAG(前后端依赖架构师先定的契约);QA 先写测试(测试作者≠代码作者,结构性去偏)"], ["新增", "开发团队架构做实 · Wave C 团队可见:实时花名册面板(每个席位 + idle/working/reviewing/blocked/done 状态)+ 交接时间线 + 团队章程(/constitution)+ /team;反剧场铁律 —— 没有真实产物的席位不显示"], ["修复", "测试完整性守卫(UD-QA-001):确定性地板检测删测试 / 弱化断言 / 加 skip 或 xfail / 注释掉 / 改测试框架配置骗绿,不再轻信绿色信号、有界打回 —— 反「为了过线而黑掉测试」"], ["新增", "信任档 mode-aware + 自学习:Plan / Guarded / Auto 三档在工具调用级真区分;不可逆动作(.git / 网络 / 破坏性 shell)每档都强制二次确认;「记住此决定」可持久化,同类动作下次免问"], ["改进", "长会话不再发沉 / 卡顿:新增 settled 渲染缓存 + 事件合并,长会话不再每帧重新解析整段历史,治本流式卡顿"], ["改进", "可恢复编辑 + 字素簇光标:kill-ring + yank,Ctrl+U/K/W 删除的内容可恢复、不再不可逆丢字,撤销 / 重做 Ctrl+Z;光标按字素簇移动删除,ZWJ emoji / 组合符当作一个单位、不再被劈裂;大段粘贴折叠成 chip"], ["新增", "一批交互成熟度补齐:重试可见(退避前显示倒计时、空闲挂死自动重驱一次)· 任务持久化(/tasks 重启可重连)· 版本化配置迁移器 · 完成响铃 · Ctrl+F 转录搜索 · 上下文 / 花费仪表 · 双击 Esc 回退重发 · ! 内联 shell · 快捷键速查"], ["新增", "自进化两项:首过验收率(按路由类 / 步骤类记录廉价路径一次过验收、不返工的比例,某类偏低则该类多咨询 / 降自主)· 爆炸半径验证排序(按 DAG 下游依赖数加权排验证与返工 —— 上游 schema / 契约错了会拖垮全部,优先验)"], ["修复", "底座 / 交互一批修复:底座空闲 300s 误杀 → 改活性判断(在跑工具且底座活着就一直等)· 中止后状态同步 · 路由失败后「继续」不再重头查询(底座活着留住会话)· 工作时屏幕闪烁(同步输出 gate)· 中文吞字(宽 emoji 的 turn 标记错位、U+FE0E 钉死)· stderr ANSI 乱码剥离 · 滚轮拖选复制更多 · 多目录串台隔离(config 临时文件加 PID)· API 报错不再静默(限流 / 鉴权 / 网络 / 过载显真实文案 + 可操作提示)· codex /sandbox 可配 · 删掉多余的 /claude-code 别名"], ["新增", "能力扩展:MCP 扩到 6 工具(plan_status / contract_check / lessons_recall / governance_summary,只读 fail-open)· PostToolUse 审计钩子 · 自定义团队角色(.umadev/agents/*.md)· 后台运行 + /tasks 任务注册表"], ["新增", "知识库四波 +32 份商业级标准:EARS 需求 / 契约优先 / 测试纪律 / 反造假 / AI-slop 失败模式目录 / 验证器模式 / 上下文工程 / eval 驱动交付 / 分级就绪记分卡 / 可观测 SLO / 供应链卫生 / 无障碍验收门 / 生产就绪评审 等;并隐私洁净化 —— 清掉散落 79 个文件的个人邮箱 + 77 个垃圾模板标题"], ["变更", "定位重写:全仓库 + 官网统一重定位为「一个模拟真实开发团队来工作的 Coding Agent」—— 八角色各干各的活当主角、协调者只负责调度,不再以单一总监为头牌"]] },
        { ver: "1.0.15", date: "2026-06-28", current: false, title: "API 报错显示 · 渲染自愈 · 交互成熟度第一波", changes: [["修复", "API 报错不再被静默吞掉:底座限流(429)/鉴权/网络/过载等错误会直接显示真实错误文案 + 可操作提示(如\"未登录,运行 claude /login\"\"限流,稍后重试或 /model 切换\"),不再假报\"完成 / 无文件变更\""], ["新增", "codex 沙箱可配:.umadevrc 加 [codex] sandbox_mode,可设 danger-full-access 解锁本地端口(npm start)/git 提交;默认仍 workspace-write 保底,高危模式开机红字警告"], ["修复", "界面跑久了错乱:渲染自愈(同步输出内定期原子重绘 + 睡眠唤醒/resize/SIGCONT 自愈),不用再按 Ctrl+L;resize 不再闪烁"], ["修复", "/status 弹窗可用滚轮滚动(之前小窗口截断滚不动);/plan 现在显示完整团队评审(不再只给 +N)"], ["新增", "命令统一:palette/help/dispatch 同一注册表(/model 等全可搜,以后不再漂移)+ Ctrl+O 一键展开/收起所有折叠内容"], ["改进", "/continue 恢复底座会话带回完整上下文(不再\"不记得上次任务\");Ctrl+C 不再误退"]] },
{ ver: "1.0.14", date: "2026-06-28", current: false, title: "Ctrl+C 不再误退 · /continue 跨会话续接", changes: [["修复", "Ctrl+C 不再退出 UmaDev:它是复制的通用肌肉记忆,误按一下就退出太容易丢会话。现在 Ctrl+C 只清空半截输入 / 提示用 /quit,绝不退出;要退出用 /quit、/exit、Ctrl+D 或双击 Esc"], ["新增", "/continue 跨会话续接:之前 /run 跑过、关掉 TUI 再打开,用 /continue 只会提示重启。现在它会自动加载持久化的 plan.json,重新贴出计划(已完成的步骤打勾)、只跑剩余步骤续上,不用重头再来"]] },
    { ver: "1.0.13", date: "2026-06-28", current: false, title: "渲染健壮性 · 同步输出修界面错乱", changes: [["修复", "界面跑久了显示错乱 / 刷新撕裂(Windows 尤其明显):抄 Claude Code 的做法,用 DEC 2026 同步输出把整帧原子刷新 —— 终端缓冲整帧再一次性交换,刷新中途不再出现半成品的错乱画面;按终端类型检测支持(iTerm2 / WezTerm / ghostty / kitty / Windows Terminal 等,跳过 tmux)"], ["修复", "滚动时输入框出现乱码 + 假的「本轮已中止」:鼠标滚动的 SGR 序列在高负载下偶发被劈碎,ESC 被当退出键、其余进了输入框。现在加了防御式过滤,识别并丢弃被劈碎的鼠标序列,真正的 Esc 和正常输入不受影响"], ["新增", "Ctrl+L / /redraw 强制全屏重绘 —— 万一画面还是花了一键恢复;窗口 resize 时清屏(某些终端 resize 后留残影);长路径 / Tab 裁切到可视宽度,不再溢出串行"]] },
    { ver: "1.0.12", date: "2026-06-27", current: false, title: "修复 1.0.11 滚轮回归 · 计划分步推进 · 一批真漏洞", changes: [["修复", "【1.0.11 回归 · 重要】滚轮滚动不再破坏输入 / 假中止:OSC11 主题探测线程残留抢 stdin、把鼠标滚动序列劈碎,导致输入框出现乱码、假的「本轮已中止」、滚动卡顿。已彻底移除该探测线程"], ["修复", "/status 现在真反映进度:之前只写了状态文件、但 /status 弹窗读的是内存里从不更新的阶段表,所以一直全 pending。现在 /status 对账 workflow-state.json,阶段表推进到真实进度"], ["修复", "计划真正一步步推进:每步硬限定单步、不再一回合做完整个项目;表头显示当前步;单回合到预算就收尾(不再无限跑);长回合有心跳"], ["修复", "底座出错能看到原因(聊天 + /run 构建两条路径都接上):底座配置 / 登录坏了时它的 stderr 报错 + 退出码会显示,不再只是「base session idle」;codex 握手加超时"], ["修复", "团队评审每个 critic 的意见都能看:之前只显示第一个、其余折叠成 +N 无法查看;现在每条完整镜像进可滚动的对话历史,折叠行提示滚动 / /plan 看全部"], ["修复", "计划 / 团队评审面板执行完会清理:新一轮清旧评审,交付 / 中止 / 完成时清理悬挂面板,不再一直显示陈旧状态"], ["修复", "scorecard 通过自家治理:生成的质量报告 HTML 现在带 CSP,满足 UmaDev 自己的 UD-ARCH-013,不用再每次跑补丁脚本"], ["修复", "上方向键召回到 / 命令后能继续往上翻;一批 director 正确性加固(空活不再标完成、死会话不再完成后续步、首步不残留 Active)+ codex 健壮性 + 复制去污染 + Esc 秒停 + 历史召回保留草稿"]] },
    { ver: "1.0.11", date: "2026-06-26", current: false, title: "滚轮滚动 + 鼠标复制都能用 · 一大批交互修复", changes: [["新增", "滚轮滚动 + 鼠标拖拽复制都能用(对标 Claude Code):备用屏上鼠标滚轮直接滚回历史,拖拽选中文字应用自绘高亮、松开即复制 —— 本地走 pbcopy / xclip / wl-copy(任何终端都行,含 macOS Terminal.app),远程才用 OSC52;/mouse 可切回终端原生选择"], ["修复", "/status 现在反映真实进度:之前 /run 跑完写了真实代码,但状态机停在 research、9 阶段全 pending(只有 legacy 路径更新状态文件)。现在 director 循环同步写 workflow-state.json(按角色诚实映射阶段、单调不回退、只在真干净时才报交付),/status 跟着真实进度走"], ["修复", "计划不再卡 0/N:底座以前一个回合就把整个项目做完、计划一小时不动。现在每步提示硬限定单步(别做其它步、本步验收达成就停),计划真正一步步推进;表头显示当前进行的步号 + 长回合有心跳;单回合也加了总时长上限(到预算就收尾,不再无限跑)"], ["修复", "底座出错能看到原因:底座配置 / 登录坏了时它的报错只进 stderr(以前被丢弃),用户只见「base session idle」。现在 idle / 退出时显示底座自己的报错(如 model X not available)+ 进程退出码;codex 持续会话握手加了超时,坏底座不再永久挂起"], ["修复", "/run 需求可带空格 / 中文:之前第一个词被误当 slug,任何带空格或中文开头的需求被拒。现在只有带 - / _ 的纯 ASCII 词才算 slug"], ["修复", "大段粘贴卡顿(O(n²)→O(n))· 上方向键召回到 / 命令后失效(现在继续召回,不被斜杠面板劫持)· Esc 取消冻 UI 2 秒 · 历史召回丢草稿 · 复制带多余符号 / 空格 · 一批越界与状态同步加固"]] },
    { ver: "1.0.10", date: "2026-06-26", current: false, title: "图片输入 · 彻底不管模型 · 一批健壮性增强", changes: [["新增", "图片输入:把图片拖拽 / 粘贴进输入框,自动识别成 [图片 N] 附件,提交时改写成底座能读的 @绝对路径(底座自己读文件,UmaDev 不做 base64);非图片粘贴照常按文本处理"], ["修复", "codex 在 Windows 持续会话不可用:sandbox 枚举发成了 camelCase(workspaceWrite),新版 codex 只认 kebab-case(workspace-write / read-only),报 unknown variant 直接挂掉,已对齐"], ["变更", "彻底不管模型:UmaDev 不再向底座传 / 切换模型,底座永远用它自己配置或登录的模型(官方订阅,或你接入的第三方 / 本地都行);/model 不再切换,只说明模型在底座侧配置"], ["改进", "统一模型下载:之前国内拿 f32(~448MB)、国外拿 fp16(~224MB)大小不一致 —— 现在国内外都从 HuggingFace / hf-mirror.com 下同一个 f32,体积一致;GitHub fp16 降为兜底"], ["修复", "工具命令不再被模型下载阻塞:umadev update / --version / --help / doctor 等不再触发 224MB 模型下载(之前模型没下完时连 update 都卡住);进度条美化(块字符 + 颜色 + 实时 MB/s)"], ["修复", "六个交互 bug:取消(Esc)现在真正停掉底座再显示「已中止」;占位符 / 状态栏不再在正常运行时误显「已中止」;复杂构建不再「正在思考 N 秒」无进度(先报「正在规划」);底座按你的界面语言回复(不再默认英文);Ctrl/Alt+字母不再打出字母;release 改 panic=unwind 让 fail-open 守卫不再形同虚设(markdown 渲染 panic 不再崩整个会话)"], ["修复", "健壮性:某些终端(conhost / 部分 SSH)启动不再永久挂死(OSC11 探测加超时);打开预览地址不再留僵尸进程"], ["改进", "首响应提速:firmware 组装的阻塞 I/O(扫你的代码库 + 知识检索)移出异步线程,冷启动首次回复更快"], ["改进", "输入打磨:斜杠命令模糊匹配(/dpl 能找到 /deploy);Alt/Ctrl+←/→ 按词移动光标"], ["改进", "记忆增强:课程的衰减改成「按是否有用」驱动 —— 被召回且本轮验证通过的课程保鲜抗淘汰,从不被用的正常衰减(闭合 verify 闭环)"], ["修复", "路由:「按需求 / 规格文档实现整个项目」现在判为完整构建(走流水线),不再被误判成小修只做一部分"]] },
    { ver: "1.0.9", date: "2026-06-25", current: false, title: "纯本地双轨 RAG 落地 · 动态状态指示器", changes: [["修复", "本地向量模型分发:1.0.7 时 224MB 的 fp16 模型超过 npm 体积上限被拒(npm 用户只能用 BM25)。现在模型作为 GitHub Release 附件分发,首次启动自动下载到 ~/.umadev/embed-model(带进度条),装完完全本地、运行时无需联网;国内自动走 hf-mirror.com(HuggingFace 国内镜像,免费、快、稳)、全球走 HuggingFace / GitHub,带实时进度条,任一源失败自动换下一个并降级 BM25,UMADEV_MODEL_BASE_URL 可覆盖"], ["新增", "纯本地双轨 RAG 真正落地:向量轨用 multilingual-e5-small(fp16)经 candle 在本地运行——无需 API key、运行时不联网;与纯 Rust BM25 经 RRF 融合 + HyDE 查询扩展;model_dir 自动发现 ~/.umadev/embed-model,零配置生效。摆脱臃肿的云端依赖,让 AI 真正写出最懂你业务的代码"], ["改进", "等待指示器随底座活动动态变化:不再从头到尾死板的「正在思考」——调用工具时显示「正在读取 / 正在编辑 / 正在运行 / 正在搜索 / 正在检索」,工具一结束立刻回到思考"], ["改进", "真实 token 消耗:等待指示器显示底座自己上报的真实累计用量(本次会话),不再是字符估算,格式如 ≈12K token"], ["改进", "三底座适配收尾(F1-F6):opencode 改代码渲染 diff 卡 + 合并工具兜底 + 回复不重复;codex 真实 usage(对照真实协议修正)+ send 不阻塞 + 早期 ESC 不丢;claude 真实 usage"], ["改进", "交互打磨:鼠标拖拽可正常选择复制文本 + 键盘滚动回看历史;双击 Esc 才中断,防手误中断长构建;滚动渲染裁切修复——最新的流式输出行不再被底部提示挤掉"], ["改进", "界面清理:顶部标题栏带上底座、底部状态栏去掉重复的「项目·底座·/help」;公开仓库清理——移除内部 AI 工具配置与开发过程文档,只留用户向文档"]] },
    { ver: "1.0.7", date: "2026-06-24", current: false, title: "智能意图路由 · 团队评审 · 终端渲染重做", changes: [["新增", "意图判断与统一构建:默认对话界面由底座自己的模型判断每句话——对话 / 解释 / 小改 / 调试 / 构建;底座连不上就走最轻路径,不用关键词表。取消「对话 vs /run」的分叉:触发完整流程的是「真实构建」本身,不是某条命令——对话里随手提的构建,和 /run 享有同一套系统;底座也会以行动判断,写下第一个文件就把这一回合变成一次构建"], ["新增", "统一的常驻系统:每次真实构建都自动拥有——设计系统 / 反 AI 模板法(每个干活回合都在,无延迟成本)、构建后治理 + 设计扫描、角色团队评审(产品 / 架构 / UI-UX / 前端 / 后端 / QA / 安全,只读分叉、并行、建议性)、知识库摘要、以及从每次运行学习(记录踩坑,在后续工作里召回)。小改动召集精简 UI 团队(设计 + 前端 + QA),完整构建召集全员"], ["新增", "/goal 命令:`/goal <目标>` 驱动一次目标导向的构建,让底座持续工作到目标达成,带完整的统一系统;三个底座(claude / codex / opencode)都可用(UMADEV_NO_GOAL_MODE=1 可退出)"], ["新增", "知识库内置进二进制:完整语料(418 份商业级工程规范 + 设计规则 + 你现有代码的结构图)随包内置,首次运行自动解压到 ~/.umadev/knowledge——零配置下发到每个用户项目,不再是用户机上的空语料"], ["新增", "检索与代码库理解:知识检索用 BM25(中文友好双通道分词)+ 可选向量层(OpenAI 或本地嵌入)+ RRF 融合;另有逐语言符号扫描(repo-map)给底座你现有代码的结构概览"], ["改进", "持续会话提速:对话跑在一个常驻底座会话上,启动时预加载(底座 + MCP + 系统提示只加载一次),首次回复不再扛旧的每条消息 30-60 秒冷启动;claude 现在逐 token 流式(--include-partial-messages),回复实时显示,而不是憋到最后一次性吐出"], ["改进", "终端渲染全面重做(对标 Claude Code):真实 Markdown(CJK 对齐表格 / 标题 / 嵌套列表 / 粗斜体 / 链接显 URL / 任务勾选框 / 分语言高亮代码块);文件改动渲染成实时 diff 卡,词级高亮(只点亮改动的词)、行号边栏、虚线框;干净的工具调用行(只读工具合并 + 长输出折叠,Ctrl+R 展开);构建完成卡列出改动文件 + 运行命令 + 自动浮出可点击的预览地址(http://localhost:PORT);流式打磨(稳定前缀缓存防卡顿 / 粘底 / 带微光的 spinner)"], ["改进", "三底座适配:claude / codex / opencode 三家都逐字流式、改代码都渲染 diff 卡、都进审计与治理;归一化 opencode 的工具形状(write→Write、filePath→file_path),让它的改代码也正常显示"], ["改进", "架构:总监模型——判断请求 → 拥有并驱动一份可见的依赖计划(渲染成实时清单)→ 按步调度角色团队(写代码串行、评审并行)→ 对照确定性底线验证 + 自纠 → 收尾产出交付证明。完整的九阶段链是最完整的那条路径(重型从零构建才走),不是每条消息被迫穿过的漏斗;文档(README 三语)同步全面重写"], ["修复", "对话回复不再「很久没反应、转圈变红冻住、最后一次性吐出」(根因:claude 没开 partial-messages,憋住整段文字);逐字流式时的空白(词间空格 / 段落换行)不再被吞,文字不再粘在一起;opencode 回复不再重复叠加"]] },
    { ver: "1.0.6", date: "2026-06-22", title: "TUI 交互硬化", changes: [["修复", "深读 Claude Code / opencode 源码整体修 TUI 交互:解耦「活着感」与吐字;修无滚动 / 无鼠标 / 小终端裁切 / opencode 不流式 / 静默阻塞等 P0 问题"], ["改进", "运行中可在空隙排队输入,ESC 中断当前工作,底座工具调用可见;运行锁单写者收敛"]] },
    { ver: "1.0.5", date: "2026-06-21", title: "Windows on ARM 支持 · 版本号统一", changes: [["平台", "新增 Windows on ARM(win32-arm64)支持 —— 骁龙 / Surface 等 ARM 架构的 Windows 现在会自动安装 x64 二进制,经 Windows 11 内置 x64 模拟运行;`npm install -g umadev` 不再报 unsupported platform"], ["修复", "1.0.4 的底座 .cmd 启动修复随本次全平台重编一并带入,Windows(含 ARM)的 claude / codex 识别 + 运行彻底打通"], ["改进", "全链路版本锁定:Cargo crate、npm 包、二进制 `--version` 三者同号;新增 `bump-version.sh` 一键改版脚本,杜绝「装 1.0.5 终端却显示 1.0.0」"]] },
    { ver: "1.0.4", date: "2026-06-21", title: "Windows 底座启动修复", changes: [["修复", "修复 Windows 下「找到底座却启动失败」的问题 —— 报错 `os error 193 / 不是有效的 Win32 应用程序`。npm 安装的 claude / codex 是 `.cmd` 垫片,不是 PE 可执行文件,CreateProcess 无法直接运行;改为经 `cmd /c` 启动(Rust 官方文档标准做法)"], ["平台", "同一套程序解析统一覆盖底座 CLI、npm 操作(audit / install / uninstall)与构建步骤(npm / tsc / cargo),凡 `.cmd`/`.bat` 一律 `cmd /c`"]] },
    { ver: "1.0.3", date: "2026-06-21", title: "Windows 底座识别修复", changes: [["修复", "修复 Windows 下识别不到底座的问题 —— npm 安装的 Claude Code / Codex / OpenCode 是 .cmd 垫片,此前裸名解析只认 .exe 导致检测失败;现按 PATHEXT 正确解析为 .cmd / .exe / .bat 全路径"], ["平台", "npm 操作(audit / install / uninstall)与构建步骤(npm / tsc / cargo)同步适配 Windows,统一走同一套程序解析"]] },
    { ver: "1.0.2", date: "2026-06-21", title: "安装后可执行修复", changes: [["修复", "修复部分环境安装后二进制无法执行(EACCES)的问题 —— npm 多包分发中转会剥掉可执行位,启动器现在运行前自动恢复 chmod +x,对所有平台兜底"]] },
    { ver: "1.0.1", date: "2026-06-21", title: "全平台一键安装", changes: [["平台", "全平台 npm 一键安装:Windows / Linux / Intel Mac / Apple Silicon Mac 均可 `npm install -g umadev`,按系统自动分发对应预编译二进制"], ["新增", "随包内置离线知识库,无需额外配置即可检索"]] },
    { ver: "1.0.0", date: "2026-06-21", title: "首个公开版本 · AI 开发团队 Agent", changes: [["新增", "完整 9 阶段商业交付流水线:research → docs → spec → frontend → backend → quality → delivery,含文档确认、预览确认两道人在环确认门"], ["新增", "三种本机 CLI 底座:Claude Code、Codex CLI、OpenCode —— 直接驱动你已登录的 CLI 并共享它自己的模型与推理强度,UmaDev 不持有任何 API key"], ["新增", "并行扇出:文档阶段并发起草架构与 UI/UX,缩短交付墙钟时间"], ["新增", "UIUX 一致性硬门 + 反 AI-slop 设计法:命名禁令(默认 indigo / 紫渐变 / emoji 图标 / 模板骨架)与设计 token 纪律,不符合声明设计系统的 UI 自动打回重做"], ["新增", "失败开放治理内核:写入前 hook + CI + 质量门补扫,禁 emoji 图标 / 硬编码颜色 / AI 套话;合规映射 SOC 2 · ISO 27001 · EU AI Act"], ["新增", "知识库:416 份工程规范文档,BM25 + 可选向量混合检索(RRF 融合),可接入团队自有知识库"], ["新增", "前后端契约校验:解析架构 API 表 → 渲染 OpenAPI → 校验前端 fetch 调用对齐"], ["新增", "自学习踩坑知识库:自动识别报错,按技术栈指纹在下次同类问题前主动规避"], ["新增", "质量门 + proof pack:scorecard.html 成绩单、proof-pack.zip 交付证明与审计证据链"], ["新增", "三语 TUI(简体 / 繁体 / English)、MCP server 与管理器;纯 Rust 单二进制,十个 crate,零外部进程依赖"]] },
  ],
  en: [
        { ver: "1.0.20", date: "2026-07-01", current: true, title: "Windows fully fixed · sharper positioning · security / RAG / concurrency", changes: [["Changed", "Sharper positioning: UmaDev commands the Claude Code / Codex / OpenCode you already use (rolled out across README / docs / website / npm)"], ["Fixed", "The preview dev-server boots on Windows again — it was dead (sh hardcoded, npm.cmd not found); /preview and the auto-preview after a web build now both work"], ["Fixed", "The destructive-command trust floor now knows Windows verbs (del / rd / format / Remove-Item), so Auto mode no longer skips confirmation"], ["Fixed", "The Windows console no longer garbles: a full repaint on a layout-height change after history-recall / clear; /exit and /quit no longer leave PowerShell unusable (a complete reverse-order terminal restore); a dragged image path is no longer mangled by backslash-as-escape"], ["CI", "A new PTY launch smoke test boots the real binary + the windows test goes green — a startup crash can never silently ship again"], ["Fixed", "An image / paste chip deletes and edits as ONE unit (was per-char, and silently dropped the image on submit); a mid-turn backend switch no longer leaks the old session; an unterminated bracketed paste can no longer wedge input"], ["Fixed", "An incomplete deliberate build no longer ships a clean delivery proof-pack, assertion-neutering no longer passes as green, plus 8 routing / gate / coverage correctness fixes"], ["Fixed", "The bundled local fp16 vector layer no longer silently dies when any curated section exceeds 512 tokens (truncate + per-text isolation); a phase-filtered vector channel + quality-score re-sort + a code-fence-aware chunker, 7 RAG-retrieval fixes in all"], ["Security", "The owned SAST catches the secrets people actually leak: spaced / JSON-key assignments + a real entropy fallback, OpenAI sk- / PEM private keys, .env/config/IaC files, more token families — and never reports Clean having scanned zero files; pr --create now stages only the run's artifacts (was git add -A, sweeping the whole dirty tree)"], ["Fixed", "A HIGH UB data race (runtime env mutation vs a concurrent driver getenv) replaced with thread-safe shared state; the self-learning memory file's lost-update race closed with one shared lock; plus 6 CLI/MCP fixes (an MCP slug-traversal guard, and more)"]] },
        { ver: "1.0.19", date: "2026-06-30", current: false, title: "Critical: fix a startup panic (1.0.17/1.0.18 regression)", changes: [["Fixed", "A critical regression: 1.0.17/1.0.18 panicked on launch and the app would not run — tokio::select! evaluates a branch expression every iteration (the if guard only gates polling, not evaluation), and the cancel-drain branch had been rewritten in the 1.0.17 M1 fix from a lazy async block into a direct drain_cancelled_task(cancel_drain.as_mut().expect(…), …) call, so with cancel_drain == None when idle it panicked on the very first loop turn; reverted to a lazy async block that only touches cancel_drain when the guard holds and the future is polled, plus a PTY launch smoke check. All 1.0.17 / 1.0.18 users should upgrade to 1.0.19"]] },
        { ver: "1.0.18", date: "2026-06-30", current: false, title: "Frontier hardening x5 · every user report fixed (port conflict / process logs / trust)", changes: [["Added", "A typed evidence-contract per plan step (frontier F1): 'done' is no longer a coarse whole-workspace check — the brain declares per-step evidence in the plan JSON (file-exists / file-contains / build-clean / test-passes / route-responds), which UmaDev parses and owns; any gap means the step isn't done and names exactly which file / test / route is missing; empty evidence falls back to the existing acceptance (fail-open)"], ["Added", "A fail-CLOSED irreversible-action boundary on uncertainty + a consecutive-failure breaker (frontier F3): an obfuscated command that evaded the token scan (base64-decode piped into sh, eval $(...), an inline -c interpreter, \\x byte blobs) used to be silently allowed in Auto/Guarded — it is now Reversibility::Uncertain, force-escalated in every mode and never auto-remembered; plus a breaker that finalizes cleanly after 3 same-class build/review-verify failures instead of grinding to the 32-step cap, with no false success"], ["Improved", "The critic forks a fresh independent read-only session (the deeper F2): claude used to fork via --resume + --fork-session and codex via thread/fork, so the read-only critic inherited the doer's full deliberation (a maker-checker reasoning leak); claude now opens a fresh --session-id (no resume / fork) and codex a fresh read-only thread via thread/start, so the critic reviews on a genuinely clean context at the host level and the F2 prompt firewall becomes belt-and-suspenders"], ["Improved", "The injected memory delta-playbook is now byte-bounded (frontier F4): the memory layer is already a deduped, distilled delta-playbook (not raw episodes, ranked by frequency x recency, clustered into higher-level rules), but relevant_lessons_for_prompt capped count and not bytes — the direct callers (runner / director_loop) could be flooded by 3 fat deltas; a hard 3000-char budget now assembles highest-rank-first, drops an overflowing low-rank delta and truncates the top one, bounding the block by both count and bytes for every caller"], ["Improved", "The KV-cache-stable firmware prefix is pinned + a bounded plan-progress recitation (frontier F5): the firmware already emits stable blocks before volatile ones (each deterministically ordered, no HashMap iteration / timestamp in the prefix); a module-doc + a STABLE->VOLATILE boundary comment + lock tests now pin the byte-exact prefix invariant, and a bounded one-line recitation ('N of M plan steps complete; still ahead: next two titles') is threaded into every drive / rework directive so a long multi-step run keeps the base on-track"], ["Fixed", "The preview dev-server boot survives a leftover process holding the port (user-reported 2899s hang + 6 npm-run-dev re-runs): the child was spawned stdout/stderr=null, so UmaDev was blind to 'port 3000 in use -> using port 3002' / 'already running' and curl-polled the assumed 3000 (a stale server answered, falsely Verified); it now captures + scans output (re-points the probe URL on a fallback, extracts the real bound port), boots once within a single READY deadline (text-ready is curl-confirmed) instead of hanging, and reclaims only its own tracked stale PID from .umadev/preview.pid (never a foreign process), reusing a foreign server that still answers"], ["Fixed", "/logs keeps the TAIL of a long build, not the head: with process-log visibility on (16KiB cap) a long build's cumulative output was head-truncated, so every frame past the cap pinned to the same first 16KiB (the live stream froze) and the final result clipped the error tail (the user-reported Maven/Spring case); a new truncate_preview keeps the last max chars in verbose mode (char-boundary-safe + a clean line start + a tail marker) so the stream advances and the error survives, wired into all three bases' process-log paths"], ["Fixed", "The trust floor no longer over-blocks checksum / lint pipes + AskUserQuestion is actually wired: '| sh' was a substring of '| sha256sum' / '| shuf' / '| shellcheck', so a read-only 'cat dist/app.js | sha256sum' (checksum / release / lint) was judged Uncertain -> denied in headless Auto/Guarded; the pipe target is now matched as a whole token (sh/bash/zsh bounded by whitespace/metachar), a real '| sh' stays Uncertain, and '$(' joins the substitution check; plus the dead AskUserQuestion relay is fixed — a user typing '1' used to send a bare '1' the base could misread, a new PendingAskHolder now resolves the reply to the option label + frames it as the explicit answer next turn"], ["CI", "The release workflow retries the HuggingFace model download: the 'fetch + quantize embedding model' step hit a HuggingFace 429 during the 1.0.17 release (curl -fsSL has no retry), so all 5 platform builds + the npm publish succeeded but 'publish github release' failed and had to be re-run by hand; the three model downloads now use curl --retry 5 --retry-delay 15 --retry-all-errors so a transient rate-limit self-heals"]] },
        { ver: "1.0.17", date: "2026-06-30", current: false, title: "Every user report fixed · local RAG revived · a hardening sweep", changes: [["Fixed", "doctor flags a missing CLAUDE_CODE_OAUTH_TOKEN (user-reported 401): after 'claude login' the probe reads logged-in, but UmaDev drives 'claude --print' non-interactively, which needs a headless credential or it 401s at runtime — the claude-code backend with no headless credential now WARNs + points at 'claude setup-token', a present one PASSes, other backends never false-warn"], ["Fixed", "Local fp16 semantic RAG revived: the local model emits 384-dim vectors but VectorStore baked dim=1536, so search rejected every query and the guard couldn't catch it — every npm install silently ran BM25-only and the marketed local semantic layer never contributed; the store is now tagged with the real embedding width and reads the local backend's actual hidden_size, so it's 384-dim end-to-end and the local dual-channel RAG truly works"], ["Fixed", "Copy / paste + the input-wedge root-fix (user-reported): when the 50ms lone-ESC flush splits the bracketed-paste end marker, in_paste stuck true forever and swallowed every later key (backspace / arrows / ESC / history all went dead) — both append branches now detect and close the paste char-boundary-safely; an undraggable input box gains a one-time Shift+drag / /mouse hint"], ["Added", "The base's long-running process logs are visible · /logs (user-reported): a toggle streams the base's multi-minute build / process output — codex's item/started·updated frames were ignored so a multi-minute Maven/Spring build showed nothing; it now shows a running indicator at once + streams incrementally, the 200-char completion cap widens to 16KiB, off by default"], ["Added", "AskUserQuestion bridged to the user (user-reported): the base runs the question headless and can't render its own picker, so it silently auto-cancelled — it now renders the question + every numbered option + 'reply with your choice, it's relayed to the base, awaiting your answer not cancelled', and the reply continues the same session; all three bases"], ["Added", "Active durable-fact recording (user-reported): .umadev/memory/facts.jsonl used to only instruct the base to write it, which it often didn't, so the file never appeared — after a work turn the brain now enumerates the turn's durable facts (key:value) on a read-only fork and records them deduped, gated to skip chat / explain and throttled to save tokens, fail-open"], ["Added", "The built app's runtime model is configurable (user-reported): UmaDev no longer hardcodes the dev base's vendor as the BUILT app's runtime LLM — it detects whether the app calls an LLM at runtime + the user's stated model (Qwen / DeepSeek / Zhipu / Moonshot / ERNIE / Doubao / Gemini / local Ollama) and injects a provider-abstraction directive (model id + base URL + key from env, default the user's model); plus a CJK-in-exports standard (CSV BOM / xlsx / embedded PDF fonts / Content-Disposition RFC 5987·6266)"], ["Fixed", "A comprehensive self-audit hardening sweep: every host subprocess await is bounded (a leaked grandchild can't wedge a brain consult) · the chat surface floors a real UI/greenfield build onto the review-team + gate path (no more un-reviewed shipping) · Plan/Guarded trust holes (chained read-verb smuggling, out-of-tree absolute writes auto-allowed) · a governance catch_unwind backstop + color / emoji / slop false-positive fixes + an audit-rotation race · the contract gate no longer passes vacuously on a descriptive table header (zero-endpoint false pass)"], ["Fixed", "A TUI lifecycle batch: the cancel-drain deadline (no longer wedged forever in 'stopping…') · queued-steer recovery (a falsely-lit 'queued N' no longer lingers) · !-shell OOM / orphan (capped + killed + reaped on timeout) · tab-preserving paste · rewind truncates the full transcript"], ["Improved", "F2 critic independence + input UX + the website: critics review on a clean context, not the doer's reasoning (the review fork used to inherit the main session and carry the doer's full deliberation in — a self-preference leak; a firewall preamble now judges only the artifact + acceptance + requirement) · a T7 structured confirm-gate picker · Ctrl+R reverse history search + fzf ranking · soft-wrap-aware copy · concise big-tech changelog titles + a mobile-carousel overflow fix"]] },
        { ver: "1.0.16", date: "2026-06-29", current: false, title: "Dev team made real · memory that holds · a hardening wave", changes: [["Added", "Memory that doesn't slip (safety net #1, durable facts): a new .umadev/memory/facts.jsonl — project facts the base discovers (where JDK17 lives, the build / test commands, environment constraints) are injected into the firmware head every turn, surviving both a trimmed transcript and the base rotating its own context, so they are never looked up twice (fixing the user-reported 'recorded it, then re-searched anyway')"], ["Improved", "Memory that doesn't slip (safety net #2, smart compaction): when the token budget is exceeded, early turns are structurally summarized on a read-only fork (intent / files touched / key decisions / bug fixes / TODOs / current work) while the recent tail is kept verbatim, replacing the old lossy 16-entry FIFO + 160-char /compact; the full verbatim transcript is always persisted to disk, /resume restores losslessly, and 3 consecutive summary failures trip a fail-open circuit breaker"], ["Fixed", "Writing a doc no longer burns tokens: the borrowed brain first judges 'write a document vs build the product that document describes' — a PRD / design doc / research report is a light touch (at most one PM read), not the full 8-seat team + multi-round review + whole pipeline, while a real doc-platform / product build is unchanged (has_heavy_signal protects it). It also fixes the source-existence floor, which used to falsely report a 'no-code failure' on a pure-doc task and force the base to write unneeded code over wasted rounds — it is now doc-aware. Root cause: the brain judged intent but a keyword table still sized the build; the brain now sizes it, keywords are fallback only"], ["Added", "The dev team made real - Wave A, intelligent seat building: a full build is auto-routed to per-role real building (product to architecture to design to frontend/backend to QA to security to DevOps, each role genuinely building its own slice), while a small task still takes the cheap single-turn path — decided automatically, never a user choice"], ["Added", "The dev team made real - Wave B, real role artifacts: design-tokens become a first-class deliverable + a DesignTokensPresent acceptance check; a contract-first DAG (frontend/backend depend on the architect's contract first); QA writes the tests first (test author != code author, a structural de-biasing)"], ["Added", "The dev team made real - Wave C, the team made visible: a live roster panel (each seat + idle/working/reviewing/blocked/done state) + a handoff timeline + a team constitution (/constitution) + /team; an anti-theater rule — a seat with no real artifact is not shown"], ["Fixed", "Test-integrity guard (UD-QA-001): the deterministic floor now detects deleted tests / weakened assertions / an added skip-or-xfail / commented-out tests / a doctored test-framework config gaming green, no longer trusts the green signal and folds a bounded rework back — anti-reward-hacking"], ["Added", "Trust profile, mode-aware + self-learning: Plan / Guarded / Auto genuinely differ at the tool-call level; an irreversible action (.git / network / destructive shell) is force-confirmed in every mode; 'remember this decision' persists so the same class of action isn't asked again"], ["Improved", "Long sessions no longer bog down: a settled render cache + event coalescing mean a long session no longer re-parses the whole history every frame, fixing streaming stutter at the root"], ["Improved", "Recoverable editing + a grapheme cursor: a kill-ring + yank make Ctrl+U/K/W recoverable instead of an irreversible loss, with undo/redo on Ctrl+Z; the cursor moves and deletes by grapheme cluster so a ZWJ emoji / combining mark is one unit and never split; a large paste folds into a chip"], ["Added", "A wave of interaction-maturity fills: a visible retry (a countdown before back-off, one auto-redrive if idle-wedged) · persisted tasks (/tasks reconnects after a restart) · a versioned config migrator · a completion bell · Ctrl+F transcript search · context / spend gauges · double-Esc to step back and resend · ! inline shell · a keyboard-shortcut cheatsheet"], ["Added", "Two self-evolution levers: a first-pass acceptance rate (per route-class / step-class, the share of cheap-path steps that pass acceptance on the first try without rework — a low class consults more / lowers autonomy) · blast-radius verification ordering (verify and rework weighted by DAG downstream-dependency count — an upstream schema / contract error drags everything down, so verify it first)"], ["Fixed", "A batch of base / interaction fixes: a 300s base-idle false-kill becomes a liveness check (keep waiting while a tool runs and the base is alive) · post-abort state sync · 'continue' after a route failure no longer re-queries from scratch (a live base keeps the session) · screen flicker while working (a synchronized-output gate) · Chinese characters eaten (a wide-emoji turn-marker misalignment, U+FE0E pinned) · stderr ANSI garbage stripped · wheel-drag selection copies more · multi-directory cross-talk isolated (a PID on the config temp file) · API errors no longer silent (rate-limit / auth / network / overload show the real text + an actionable hint) · codex /sandbox configurable · the redundant /claude-code alias removed"], ["Added", "Capability expansion: MCP grown to 6 tools (plan_status / contract_check / lessons_recall / governance_summary, read-only fail-open) · a PostToolUse audit hook · custom team roles (.umadev/agents/*.md) · background run + a /tasks registry"], ["Added", "Knowledge base, four waves + 32 commercial-grade standards: EARS requirements / contract-first / test discipline / anti-fabrication / an AI-slop failure-mode catalog / verifier patterns / context engineering / eval-driven delivery / a tiered-readiness scorecard / observability SLOs / supply-chain hygiene / accessibility acceptance gates / production-readiness review, etc.; plus a privacy scrub — a personal email scattered across 79 files + 77 junk template titles removed"], ["Changed", "Repositioning rewritten: the whole repo + the website now reposition UmaDev as 'a Coding Agent that works like a real dev team' — the eight roles each doing their own specialty are the hero and the coordinator only schedules, no longer a single director as the headline"]] },
        { ver: "1.0.15", date: "2026-06-28", current: false, title: "Surface API errors · rendering self-heal · interaction maturity wave 1", changes: [["Fixed", "API errors are no longer silently swallowed: a base rate-limit (429) / auth / network / overload now shows the real error text + an actionable hint (e.g. 'not logged in — run claude /login', 'rate-limited — retry or /model to switch') instead of a false 'done / no file changes'"], ["Added", "Configurable codex sandbox: .umadevrc [codex] sandbox_mode can be set to danger-full-access to unblock local dev ports (npm start) / git commits; default stays workspace-write, high-risk mode prints a red startup warning"], ["Fixed", "Screen garbling over time: rendering self-heal (periodic atomic repaint inside synchronized output + sleep-wake/resize/SIGCONT recovery) — no more Ctrl+L; resize no longer flickers"], ["Fixed", "The /status overlay scrolls with the mouse wheel (was truncated in small windows); /plan now shows the full team review (not just +N)"], ["Added", "Unified commands: palette/help/dispatch read one registry (/model etc. all searchable, no more drift) + Ctrl+O expands/collapses ALL folded content at once"], ["Improved", "/continue resumes the base session with full context (no longer 'forgets the task'); Ctrl+C no longer quits by accident"]] },
{ ver: "1.0.14", date: "2026-06-28", current: false, title: "Ctrl+C no longer quits · /continue resumes across sessions", changes: [["Fixed", "Ctrl+C no longer quits UmaDev: it is universal muscle-memory for COPY, so a stray Ctrl+C dropped the session too easily. Ctrl+C now only clears a half-typed input / hints to use /quit, and never exits; quit deliberately with /quit, /exit, Ctrl+D, or a double-Esc"], ["Added", "/continue resumes across sessions: previously, after a /run you closed and reopened the TUI, /continue only told you to restart. It now loads the persisted plan.json, re-posts the checklist (done steps checked) and drives ONLY the remaining steps, so you continue instead of starting over"]] },
    { ver: "1.0.13", date: "2026-06-28", current: false, title: "Render robustness · synchronized output fixes the garbled screen", changes: [["Fixed", "The screen garbles / tears after running a while (esp. on Windows): copying Claude Code, each frame is now wrapped in DEC 2026 synchronized output so the terminal buffers the whole frame and swaps it atomically, so a half-drawn garbled frame can no longer appear mid-paint; terminal support is detected by type (iTerm2 / WezTerm / ghostty / kitty / Windows Terminal, etc., skipping tmux)"], ["Fixed", "Scrolling leaked raw mouse codes into the input box + a false aborted: under load the wheel SGR sequence was occasionally split, so its ESC fired as a stray keypress and the rest leaked as text. A defensive filter now recognizes and drops a split mouse sequence; a real Esc and normal typing are unaffected"], ["Added", "Ctrl+L / /redraw force a full repaint to recover if the screen ever garbles; a window resize now clears stale cells some terminals leave; long no-space paths / tabs are clipped to the viewport width so they cannot overflow and bleed across rows"]] },
    { ver: "1.0.12", date: "2026-06-27", current: false, title: "Fix the 1.0.11 wheel regression · step-by-step plan · real-gap fixes", changes: [["Fixed", "[1.0.11 regression - important] Mouse-wheel scroll no longer corrupts input / false-aborts: the OSC 11 theme probe left a worker thread blocked reading stdin that raced the event loop and split mouse SGR bursts, producing garbage in the input box, a false aborted, and scroll stutter. That probe thread is removed entirely"], ["Fixed", "/status now truly reflects progress: it previously wrote the state file but the /status overlay read an in-memory phase table that the plan path never updates, so it stayed all-pending. /status now reconciles with workflow-state.json and advances the phase table to reality"], ["Fixed", "The plan really walks step-by-step: each step is hard-scoped to ONE step (no more whole-project-in-one-turn); the header shows the active step; a single turn settles at the budget; a long turn has a heartbeat"], ["Fixed", "Base errors are diagnosable on BOTH the chat and /run build paths: a broken base config surfaces its own stderr + exit code instead of a blind base session idle; the codex handshake is now bounded"], ["Fixed", "Every team-review critic verdict is readable: only the first showed and the rest collapsed to +N with no way to view them; each is now mirrored in full into the scrollable transcript, and the collapsed line hints scroll up or /plan"], ["Fixed", "The plan / team-review panels are cleaned up after they run: a new round clears the old verdicts, and a terminal state clears the lingering panels instead of showing stale state"], ["Fixed", "The generated scorecard passes UmaDev own governance: the quality-report HTML now ships a CSP satisfying UD-ARCH-013, so no per-run patch script is needed"], ["Fixed", "Up-arrow keeps recalling history after a /command; a batch of director correctness hardening (no Done over zero work, a dead session cannot complete later steps, the first step is never stranded Active) + codex robustness + copy de-pollution + instant Esc + history recall keeps the draft"]] },
    { ver: "1.0.11", date: "2026-06-26", current: false, title: "Wheel-scroll & mouse-copy both work · a big interaction-fix batch", changes: [["Added", "Mouse-wheel scrollback AND drag-to-select-copy both work (the Claude Code model): on the alt screen the wheel scrolls back through history, a left-drag selects text the app highlights itself and copies on release — locally via pbcopy / xclip / wl-copy (works in EVERY terminal incl. macOS Terminal.app), OSC 52 only as the remote fallback; /mouse toggles back to native terminal selection"], ["Fixed", "/status now reflects real progress: a /run build wrote real code but the state machine stayed at research with all 9 phases pending (only the legacy path updated the state file). The director loop now syncs workflow-state.json (honest seat to phase mapping, monotonic, delivery only on a genuinely clean finish), so /status tracks reality"], ["Fixed", "The plan no longer freezes at 0/N: the base used to build the whole project in one turn while the checklist sat still for an hour. Per-step directives now hard-scope the base to ONE step (do not build others; stop when this step acceptance is met) so the plan walks step-by-step; the header shows the active step number + a long turn has a heartbeat; a single turn is now bounded by the run budget (it settles at the budget instead of running unbounded)"], ["Fixed", "Base errors are now visible: when a base config/login is broken its error only went to stderr (previously discarded) and the user saw a blind base session idle. The idle/exit settle now surfaces the base OWN stderr (e.g. model X not available) + exit code; the codex continuous-session handshake is now bounded so a wedged base cannot hang forever"], ["Fixed", "/run accepts a requirement with spaces / Chinese: the first word was mistaken for a slug, so any spaced / Chinese-first requirement was rejected. Now only an ASCII word with a - / _ separator is treated as a slug"], ["Fixed", "Large-paste lag (O(n²) to O(n)) · up-arrow stopped working after recalling a /command (now keeps recalling instead of being hijacked by the slash palette) · Esc froze the UI for 2s · history recall lost the draft · copy carried stray glyphs / padding · a batch of bounds + state-sync hardening"]] },
    { ver: "1.0.10", date: "2026-06-26", current: false, title: "Image input · UmaDev manages no model · robustness upgrades", changes: [["Added", "Image input: drag / paste an image into the prompt — it becomes an [Image N] attachment and is rewritten on submit to an @<abs-path> the base reads as an image (the base reads the file; UmaDev never base64-encodes). A non-image paste is treated as text as before"], ["Fixed", "codex continuous session broken on Windows: the sandbox enum was sent camelCase (workspaceWrite); newer codex only accepts kebab-case (workspace-write / read-only) and rejected it with unknown variant, killing the session. Now aligned"], ["Changed", "UmaDev manages NO model: it no longer sends / switches the model — the base always runs whatever it is configured or logged in with (an official subscription, or your own third-party / local model). /model no longer switches; it just explains the model lives in the base"], ["Improved", "Unified model download: China users used to get f32 (~448MB) and international users fp16 (~224MB) — inconsistent. Now everyone pulls the SAME f32 from HuggingFace / hf-mirror.com; the GitHub fp16 is a last-resort fallback"], ["Fixed", "Utility commands no longer block on the model download: umadev update / --version / --help / doctor no longer trigger the 224MB model fetch (before, update itself hung while the model downloaded); progress bar beautified (block glyphs + color + live MB/s)"], ["Fixed", "Six interaction bugs: Cancel (Esc) now genuinely stops the base before showing aborted; the placeholder / status no longer falsely read aborted during a normal run; a complex build no longer shows a bare thinking-Ns with no progress (a planning note leads); the base replies in your UI language (not English by default); Ctrl/Alt+letter no longer types the letter; release panic=unwind restores the fail-open guards (a markdown-render panic no longer crashes the whole session)"], ["Fixed", "Robustness: some terminals (conhost / some SSH) no longer hang forever at launch (the OSC11 probe is now bounded); opening a preview URL no longer leaks a zombie process"], ["Improved", "Faster first response: the firmware blocking I/O (scanning your repo + knowledge retrieval) moved off the async worker, so the cold-start first reply is quicker"], ["Improved", "Input polish: fuzzy slash-command matching (/dpl finds /deploy); Alt/Ctrl+arrow-keys move the caret by word"], ["Improved", "Memory: lesson decay is now usage-driven — a lesson recalled into a turn whose verify gate then PASSED stays fresh and resists eviction, while a never-helpful one decays normally (closing the loop with the verify step)"], ["Fixed", "Routing: implementing a whole project from a requirements / spec doc now triages as a full build (the pipeline), not a small edit that only does part"]] },
    { ver: "1.0.9", date: "2026-06-25", current: false, title: "Fully-local dual-channel RAG · a live activity indicator", changes: [["Fixed", "Local vector model distribution: in 1.0.7 the 224MB fp16 model exceeded npm's size limit and was rejected (npm users got BM25-only). It now ships as a GitHub Release asset and auto-downloads on first launch into ~/.umadev/embed-model (with a progress bar) — fully local + offline afterwards. China users automatically use hf-mirror.com (HuggingFace's free, fast China mirror), everyone else uses HuggingFace / GitHub, with a live progress bar, automatic source failover, and a BM25 degrade on failure; UMADEV_MODEL_BASE_URL overrides the source"], ["Added", "Fully-local dual-channel RAG, for real: the vector channel runs multilingual-e5-small (fp16) locally via candle — no API key, no runtime network — fused with pure-Rust BM25 via RRF plus HyDE query expansion; model_dir auto-discovers ~/.umadev/embed-model, zero config. No cloud dependency"], ["Improved", "The waiting indicator reflects the base's LIVE activity instead of a static 'thinking' — it shows reading / editing / running / searching / fetching while a tool runs, reverting to thinking the instant the tool finishes"], ["Improved", "Real token usage: the indicator shows the base's OWN reported cumulative usage for the session (e.g. ≈12K token), not a character estimate"], ["Improved", "3-base wrap-up (F1-F6): opencode renders diff cards on edits + coalesced-tool back-fill + no duplication; codex real usage (fixed against the real protocol) + non-blocking send + early-ESC honored; claude real usage"], ["Improved", "Interaction polish: native click-drag text selection/copy works again + keyboard scrollback; double-press Esc to interrupt (so a stray key can't nuke a long build); scroll-clip fixed — the newest streaming row is no longer pushed off the bottom"], ["Improved", "UI cleanup: the top title bar now carries the base, the bottom status row dropped the duplicate 'project · base · /help'; the public repo was cleaned of internal AI-tool configs + development-process docs"]] },
    { ver: "1.0.7", date: "2026-06-24", current: false, title: "Intent routing · team review · terminal UI rebuilt", changes: [["Added", "Intent routing + unified builds: the default chat surface lets the base's own model judge each turn — chat / explain / small edit / debug / build; if the base is unreachable it takes the lightest path, no keyword table. The chat-vs-/run split is gone: what triggers the full flow is a real build, not a typed command — a build mentioned in chat earns the same systems as /run, and the base also decides by acting (its first file write turns the turn into a build)"], ["Added", "One always-on system: every real build automatically gets the design system / anti-AI-template rules (present on every working turn, no latency cost), a post-build governance + design scan, the role-team review (product / architecture / UI-UX / frontend / backend / QA / security — read-only forks, parallel, advisory), the curated knowledge digest, and learning from each run (records pitfalls, recalls them later). A small edit convenes a minimal UI team; a full build the whole roster"], ["Added", "The /goal command: `/goal <objective>` drives a goal-directed build that keeps the base working until the objective is met, with the full system; available on all three bases (UMADEV_NO_GOAL_MODE=1 opts out)"], ["Added", "Knowledge bundled into the binary: the full corpus (418 commercial-grade engineering standards + design rules + a map of your code) ships in the binary and auto-extracts to ~/.umadev/knowledge on first run — zero config, on every project, no longer an empty corpus on a user machine"], ["Added", "Retrieval + code awareness: knowledge retrieval uses BM25 (CJK-friendly dual-channel tokenization) + an optional vector layer (OpenAI or local embeddings) fused with RRF; plus a per-language symbol scan (repo-map) that gives the base an outline of your existing code"], ["Improved", "Persistent-session speed: chat runs on one resident base session pre-loaded at launch (base + MCP + system prompt loaded once), so the first reply no longer pays the old 30-60s per-message cold start; claude now streams token-by-token (--include-partial-messages) so a reply renders live instead of buffering until the end"], ["Improved", "Terminal rendering rebuilt (at Claude-Code parity): real Markdown (CJK-safe aligned tables, headings, nested lists, bold/italic, links that surface their URL, task-list checkboxes, per-language highlighted code); a file edit renders as a real-time diff card with word-level highlighting (only the changed words light up), a line-number gutter and a dashed frame; clean tool-call rows (read-only tools merged, long output folded, Ctrl+R to expand); a build-completion card with changed files + run command + an auto-surfaced clickable preview URL (http://localhost:PORT); streaming polish (stable-prefix cache, sticky-to-bottom, a shimmer spinner)"], ["Improved", "3-base parity: claude / codex / opencode all stream token-by-token, all render diff cards on file edits, all enter the audit + governance trail; opencode's tool shape is normalized (write→Write, filePath→file_path) so its edits display correctly"], ["Improved", "Architecture: the director model — judge the request → own and drive a visible dependency plan (a live checklist) → schedule the role team step by step (writers serial, reviewers parallel) → verify against a deterministic floor + self-correct → finalize with a delivery proof. The full nine-phase chain is the most complete path (only for a heavyweight greenfield build), not a funnel every message goes through; the README (all three languages) was rewritten end to end"], ["Fixed", "Chat replies no longer 'hang, spin red and freeze, then dump all at once' (root cause: claude wasn't streaming partial messages, buffering the whole text); whitespace in token streaming (inter-word spaces / paragraph breaks) is no longer dropped, so words don't mash together; opencode replies no longer duplicate"]] },
    { ver: "1.0.6", date: "2026-06-22", title: "TUI interaction hardening", changes: [["Fixed", "Read the Claude Code / opencode source closely and overhauled TUI interaction: decoupled the sense of liveness from token streaming; fixed P0 issues — no scrolling, no mouse, small-terminal clipping, opencode not streaming, silent blocking"], ["Improved", "Queue input in the gaps while a run is in flight, ESC to interrupt the current work, base tool calls now visible; the single-writer run lock converged"]] },
    { ver: "1.0.5", date: "2026-06-21", title: "Windows on ARM support · version lock", changes: [["Platform", "Added Windows on ARM (win32-arm64) support — ARM Windows (Snapdragon / Surface) now installs the x64 build and runs it through the OS built-in x64 emulation; `npm install -g umadev` no longer reports an unsupported platform"], ["Fixed", "The 1.0.4 backend .cmd launch fix is rebuilt into every platform binary here, so claude / codex detection and execution work on Windows including ARM"], ["Improved", "End-to-end version lock: the Cargo crate, npm packages and the binary `--version` all carry the same number; a new `bump-version.sh` bumps them in one command, so you never install 1.0.5 and see 1.0.0"]] },
    { ver: "1.0.4", date: "2026-06-21", title: "Windows backend launch fix", changes: [["Fixed", "Fixed a Windows failure where the backend was found but could not launch — `os error 193 / not a valid Win32 application`. npm-installed claude / codex are `.cmd` shims, not PE executables, so CreateProcess cannot run them directly; they now launch via `cmd /c` (the documented standard approach in Rust)"], ["Platform", "One program-resolution path now covers the backend CLI, npm operations (audit / install / uninstall) and build steps (npm / tsc / cargo): any `.cmd`/`.bat` goes through `cmd /c`"]] },
    { ver: "1.0.3", date: "2026-06-21", title: "Windows backend-detection fix", changes: [["Fixed", "Fixed backend detection on Windows — npm-installed Claude Code / Codex / OpenCode are .cmd shims, and bare-name lookup only resolved .exe, so detection failed; now resolved to the full .cmd / .exe / .bat path via PATHEXT"], ["Platform", "npm operations (audit / install / uninstall) and build steps (npm / tsc / cargo) hardened for Windows through the same program-resolution path"]] },
    { ver: "1.0.2", date: "2026-06-21", title: "Post-install executable fix", changes: [["Fixed", "Fixed a post-install binary-not-executable (EACCES) failure on some setups — npm multi-package delivery strips the exec bit, so the launcher now restores chmod +x before running, on every platform"]] },
    { ver: "1.0.1", date: "2026-06-21", title: "Cross-platform one-line install", changes: [["Platform", "Cross-platform one-line install: `npm install -g umadev` on Windows / Linux / Intel Mac / Apple Silicon Mac, with the matching prebuilt binary auto-selected per system"], ["Added", "Offline knowledge base bundled with the package — retrieval works with zero extra setup"]] },
    { ver: "1.0.0", date: "2026-06-21", title: "First public release · AI dev-team agent", changes: [["Added", "Full 9-phase commercial-delivery pipeline: research → docs → spec → frontend → backend → quality → delivery, with docs-confirm and preview-confirm human-in-the-loop gates"], ["Added", "Three local CLI backends — Claude Code, Codex CLI, OpenCode — driving your already-logged-in CLI and sharing its own model and reasoning effort; UmaDev holds no API key of its own"], ["Added", "Parallel fan-out: the docs phase drafts architecture and UI/UX concurrently to cut delivery wall-clock"], ["Added", "UIUX conformance gate + anti-AI-slop design law: named bans (default indigo / purple gradients / emoji icons / template skeletons) and design-token discipline; UI that drifts from the declared design system is auto-rejected and redone"], ["Added", "Fail-open governance kernel: pre-write hook + CI + quality-gate sweep; blocks emoji icons, hardcoded colors and AI-slop; compliance mapping for SOC 2 · ISO 27001 · EU AI Act"], ["Added", "Knowledge base: 416 engineering-standard docs, BM25 + optional vector hybrid retrieval (RRF fusion), pluggable team knowledge"], ["Added", "Frontend/backend contract validation: parse the architecture API table, render OpenAPI, and check that frontend fetch calls align"], ["Added", "Self-learning pitfall KB: auto-detects errors and proactively avoids the same class of problem next time by tech-stack fingerprint"], ["Added", "Quality gate + proof pack: scorecard.html, proof-pack.zip delivery proof and an audit evidence chain"], ["Added", "Trilingual TUI (Simplified / Traditional Chinese / English), MCP server + manager; pure-Rust single binary, ten crates, zero external process dependencies"]] },
  ],
} as const;

/** Prefix a /public asset path with the deploy base path ("/umadev" on GitHub
 *  Pages, "" locally). next/image does not apply basePath to a string `src`
 *  under static export, so every /assets path must be wrapped with this. */
export const asset = (p: string) => `${process.env.NEXT_PUBLIC_BASE_PATH ?? ""}${p}`;

export const gallery = Array.from({ length: 45 }, (_, index) => {
  const n = String(index + 1).padStart(2, "0");
  return asset(`/assets/umadev/ip/uma-ip-${n}.png`);
});
