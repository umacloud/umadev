export type Lang = "zh" | "en";
export type View = "home" | "docs" | "gallery" | "changelog";

export type DocBlock =
  | { h: string }
  | { p: string }
  | { c: string }
  | { l: readonly string[] }
  | { cmds: readonly (readonly [string, string])[] };

export const i18n = {
  zh: {
    nav: { product: "产品首页", docs: "文档中心", gallery: "形象相册", changelog: "更新日志" },
    hero: {
      badge: "v1.0.7 · MIT 开源 · Rust 单二进制",
      title1: "把 AI 编码工具，变成",
      titleHi: "真正的项目总监",
      title2: " Agent",
      sub: "用自然语言说你要做什么，UmaDev 驱动你已登录的 Claude Code / Codex / OpenCode 把它做出来：规划工作、写代码、召集角色团队评审、跑质量与治理检查，交还可运行的代码和一份交付证明。一个常驻持续会话，启动即预载。",
      cta1: "快速开始",
      cta2: "阅读文档",
      copy: "复制",
      copied: "已复制",
      stats: [
        ["112", "治理规则"],
        ["8", "角色团队席位"],
        ["418", "知识库文档"],
        ["3", "本机底座"],
      ],
    },
    trust: "驱动你已登录的本机编码底座",
    backends: ["Claude Code", "Codex CLI", "OpenCode"],
    mascots: {
      eyebrow: "Uma 角色阵列",
      title: "每一步交付，都有一个 Uma 在场",
      desc: "把吉祥物从装饰变成产品语言：不同姿态对应不同阶段，让用户一眼看懂 UmaDev 正在做什么。",
      cards: [
        { img: "/assets/umadev/mascot-wave.png", role: "Clarify", title: "澄清官", desc: "先问清需求边界，避免 AI 一上来就乱写。" },
        { img: "/assets/umadev/mascot-hud-panel.png", role: "Research", title: "调研员", desc: "拉起领域资料、竞品和风险，形成可追溯调研。" },
        { img: "/assets/umadev/mascot-laptop-chair.png", role: "Docs", title: "文档员", desc: "沉下来写 PRD、架构和 UI/UX，不只聊天。" },
        { img: "/assets/umadev/mascot-point.png", role: "Plan", title: "执行导演", desc: "拆任务、定顺序、把每一步和交付物对齐。" },
        { img: "/assets/umadev/mascot-sit-code.png", role: "Code", title: "编码搭档", desc: "驱动本机 CLI 读写真实文件，推进实现。" },
        { img: "/assets/umadev/mascot-city-dashboard.png", role: "Review", title: "质量门", desc: "盯住规则、契约、构建结果和证据链。" },
        { img: "/assets/umadev/mascot-run.png", role: "Ship", title: "交付冲刺", desc: "生成 proof pack、成绩单和可审计归档。" },
        { img: "/assets/umadev/mascot-thumb-lead.png", role: "Director", title: "项目总监", desc: "总控流程，决定什么时候暂停、确认、继续。" },
      ],
    },
    flow: {
      eyebrow: "工作方式",
      title: "底座的模型判断这一步，真要构建才上全套系统",
      desc: "借一个常驻持续会话当大脑：它先路由这句话——闲聊 / 解释 / 小改 / 调试 / 构建。非构建快速回复；真实构建（聊天里随手提，或 /run、/goal）则自动拥有可见计划、角色团队评审、设计系统、知识库与交付证明。固定 9 阶段只是重型从零构建时它路由到的「最深打法」，不是每句话都被塞进的漏斗。",
      layers: [
        { k: "TUI / CLI", d: "你和 UmaDev 交流的地方：聊天界面 + 命令入口。" },
        { k: "项目总监 Agent", d: "由底座的模型判断这一步，拥有并驱动可见计划，按步调度角色团队。" },
        { k: "Runtime / 底座", d: "把任务交给 Claude Code / Codex CLI / OpenCode 写真实代码。" },
        { k: "治理 · 质量 · 证据", d: "L0 固件常驻注入设计系统 / 知识库 / 踩坑记忆，每次调用留下审计。" },
      ],
    },
    pipe: {
      eyebrow: "最深打法",
      title: "重型从零构建的最深打法",
      desc: "这条阶段链不是每句话的漏斗，而是总监为重型从零构建路由到、并由计划展开成的「最深打法」。每一步都有产物、确认点和可追溯记录。点击查看每一步做什么。",
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
        { title: "非交互命令驱动", cmd: "subprocess", desc: "UmaDev 作为项目总监，只负责任务编排、阶段门、治理规则和证据链；代码执行交给本机 CLI。" },
      ],
      notes: ["仅支持三种本机 CLI", "继续用你原来的账号与订阅", "底座负责真实读写文件、运行命令", "UmaDev 负责流程、规则、质量门、证据链"],
    },
    demo: { replay: "重新播放" },
    gov: {
      eyebrow: "底座能力",
      title: "治理、质量门、知识库——交付的底座",
      desc: "UmaDev 最早就是治理工具，这部分至今仍是核心。每一次交付，都带着规则、质量门和证据链。",
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
      title: "认识 Uma —— 你的终端总监",
      desc: "一颗会发光的终端头、一身机能风外套。Uma 是 UmaDev 的吉祥物，也是「让 AI 按流程交付」这件事的人格化。",
      cards: [
        { img: "/assets/umadev/code-orbit-agent.png", cap: "代码轨道 · 知识检索" },
        { img: "/assets/umadev/workbench-agent.png", cap: "工作台 · 真实执行" },
        { img: "/assets/umadev/peace-agent.png", cap: "发布现场 · 品牌角色" },
      ],
    },
    cta: {
      title: "免费、开源，一句话就能开始",
      sub: "MIT 许可 · Rust 单二进制 · 本地运行。当前驱动 Claude Code / Codex CLI / OpenCode，不保存你的登录信息。",
      btn1: "在 GitHub 上开始",
      btn2: "阅读文档",
      note: "npm install -g umadev",
    },
    docsPage: { title: "文档中心", sub: "从安装到交付，UmaDev 的完整使用文档。", onThis: "本页内容" },
    galleryPage: { title: "形象相册", sub: "UmaDev 的 IP 形象集 —— 点击任意一张放大查看。" },
    logPage: { title: "更新日志", sub: "UmaDev 各版本的新增、改进与安全更新。", current: "当前版本" },
    footer: {
      blurb: "把「AI 写代码」变成一个完整、可上线、可审计的交付过程。",
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
  },
  en: {
    nav: { product: "Home", docs: "Docs", gallery: "Gallery", changelog: "Changelog" },
    hero: {
      badge: "v1.0.7 · MIT licensed · Single Rust binary",
      title1: "Turn your AI coding tool into a",
      titleHi: "real project director",
      title2: " agent",
      sub: "Tell UmaDev what you want in plain language, and it drives the Claude Code / Codex / OpenCode you already logged into to build it: plan the work, write the code, convene a role team for review, run quality and governance checks, and hand back runnable code plus a delivery proof. One resident session, pre-warmed at launch.",
      cta1: "Get started",
      cta2: "Read the docs",
      copy: "Copy",
      copied: "Copied",
      stats: [
        ["112", "Governance rules"],
        ["8", "Role-team seats"],
        ["418", "Knowledge docs"],
        ["3", "Local backends"],
      ],
    },
    trust: "Drives the local coding CLI you already logged into",
    backends: ["Claude Code", "Codex CLI", "OpenCode"],
    mascots: {
      eyebrow: "Uma roster",
      title: "A different Uma shows up for every delivery phase",
      desc: "The mascots become product language, not decoration: each pose maps to a phase so people understand what UmaDev is doing.",
      cards: [
        { img: "/assets/umadev/mascot-wave.png", role: "Clarify", title: "Requirement scout", desc: "Clarifies scope before the AI starts writing." },
        { img: "/assets/umadev/mascot-hud-panel.png", role: "Research", title: "Research operator", desc: "Pulls domain context, competitors and risks into traceable notes." },
        { img: "/assets/umadev/mascot-laptop-chair.png", role: "Docs", title: "Documentation pilot", desc: "Writes PRD, architecture and UI/UX instead of just chatting." },
        { img: "/assets/umadev/mascot-point.png", role: "Plan", title: "Execution director", desc: "Breaks tasks down and aligns every step with artifacts." },
        { img: "/assets/umadev/mascot-sit-code.png", role: "Code", title: "Coding partner", desc: "Drives local CLIs to read and write real project files." },
        { img: "/assets/umadev/mascot-city-dashboard.png", role: "Review", title: "Quality gate", desc: "Checks rules, contracts, builds and the evidence chain." },
        { img: "/assets/umadev/mascot-run.png", role: "Ship", title: "Delivery sprint", desc: "Packages proof pack, scorecard and audit-ready records." },
        { img: "/assets/umadev/mascot-thumb-lead.png", role: "Director", title: "Project director", desc: "Controls the flow: pause, confirm, continue and ship." },
      ],
    },
    flow: {
      eyebrow: "How it works",
      title: "The brain judges the turn — a real build earns the full systems",
      desc: "Borrow one resident persistent session as the brain: it routes the turn first — chat / explain / quick-edit / debug / build. A non-build turn streams a fast reply; a real build (mentioned in chat, or via /run / /goal) automatically gets a visible plan, the role-team review, the design system, the knowledge base and a delivery proof. The fixed 9-phase chain is just the deepest play the director routes to for a heavyweight greenfield build — not a funnel every message is forced through.",
      layers: [
        { k: "TUI / CLI", d: "Where you talk to UmaDev — a chat interface plus command entry." },
        { k: "Project director agent", d: "Lets the base's model judge the turn, owns and drives a visible plan, schedules the role team step by step." },
        { k: "Runtime / backend", d: "Hands tasks to Claude Code / Codex CLI / OpenCode to write real code." },
        { k: "Governance · quality · evidence", d: "L0 firmware always injects the design system / knowledge / pitfall memory; every call leaves an audit trail." },
      ],
    },
    pipe: {
      eyebrow: "Deepest play",
      title: "The deepest play for a heavyweight greenfield build",
      desc: "This chain is not a funnel for every message — it is the deepest play the director routes to, and the one a plan expands into, for a heavyweight greenfield build. Every step leaves artifacts, gates and traceable records. Tap a step to see what it does.",
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
        { title: "Non-interactive command driving", cmd: "subprocess", desc: "UmaDev acts as the project director for phases, gates, governance and evidence; code execution stays in the local CLI." },
      ],
      notes: ["Only three local CLIs are supported", "Keep your existing account & subscription", "The backend reads/writes real files & runs commands", "UmaDev owns flow, rules, quality gate & evidence"],
    },
    demo: { replay: "Replay" },
    gov: {
      eyebrow: "Platform power",
      title: "Governance, quality gate, knowledge — the floor under delivery",
      desc: "UmaDev started as a governance tool, and that’s still core. Every delivery ships with rules, a quality gate and an evidence chain.",
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
      title: "Meet Uma — your terminal director",
      desc: "A glowing terminal head and a techwear jacket. Uma is UmaDev’s mascot, and the personification of “make the AI deliver by the process.”",
      cards: [
        { img: "/assets/umadev/code-orbit-agent.png", cap: "Code orbit · retrieval" },
        { img: "/assets/umadev/workbench-agent.png", cap: "Workbench · real execution" },
        { img: "/assets/umadev/peace-agent.png", cap: "Launch scene · brand character" },
      ],
    },
    cta: {
      title: "Free, open source, one sentence to start",
      sub: "MIT licensed · single Rust binary · runs locally. Currently drives Claude Code / Codex CLI / OpenCode and stores no logins.",
      btn1: "Start on GitHub",
      btn2: "Read the docs",
      note: "npm install -g umadev",
    },
    docsPage: { title: "Documentation", sub: "From install to delivery — the complete UmaDev guide.", onThis: "On this page" },
    galleryPage: { title: "Mascot gallery", sub: "The UmaDev IP mascot set — click any image to enlarge." },
    logPage: { title: "Changelog", sub: "Every UmaDev release — what was added, improved and secured.", current: "Latest" },
    footer: {
      blurb: "Turn “AI writes code” into a complete, shippable, auditable delivery process.",
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
            { p: "UmaDev 是一个本地运行的 AI 编码项目总监 Agent。推荐用 npm 安装预编译二进制，npm 只是分发壳，真正运行的是 Rust 编译出的 umadev 二进制。" },
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
        { id: "how", title: "UmaDev 如何工作", blocks: [{ p: "整体架构可以理解成四层：TUI/CLI 是你和 UmaDev 交流的地方；项目总监 Agent 决定现在做哪个阶段、何时暂停继续；Runtime/底座把任务交给 Claude Code / Codex CLI / OpenCode 写真实代码；治理/质量/证据检查产物是否合规并打包交付。" }] satisfies DocBlock[] },
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
            { p: "UmaDev is a locally-run AI coding project-director agent. Install the prebuilt binary with npm; npm is just the distribution shell, while the actual binary is Rust-compiled." },
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
        { id: "how", title: "How UmaDev works", blocks: [{ p: "Think of it as four layers: TUI/CLI, project-director agent, runtime/backend, and governance/quality/evidence. The backend writes real code while UmaDev owns flow, gates, rules and delivery evidence." }] satisfies DocBlock[] },
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
    { ver: "1.0.7", date: "2026-06-24", current: true, title: "意图判断 + 真实构建即全套团队评审 + 终端渲染全面重做 + 三底座适配", changes: [["新增", "意图判断与统一构建:默认对话界面由底座自己的模型判断每句话——对话 / 解释 / 小改 / 调试 / 构建;底座连不上就走最轻路径,不用关键词表。取消「对话 vs /run」的分叉:触发完整流程的是「真实构建」本身,不是某条命令——对话里随手提的构建,和 /run 享有同一套系统;底座也会以行动判断,写下第一个文件就把这一回合变成一次构建"], ["新增", "统一的常驻系统:每次真实构建都自动拥有——设计系统 / 反 AI 模板法(每个干活回合都在,无延迟成本)、构建后治理 + 设计扫描、角色团队评审(产品 / 架构 / UI-UX / 前端 / 后端 / QA / 安全,只读分叉、并行、建议性)、知识库摘要、以及从每次运行学习(记录踩坑,在后续工作里召回)。小改动召集精简 UI 团队(设计 + 前端 + QA),完整构建召集全员"], ["新增", "/goal 命令:`/goal <目标>` 驱动一次目标导向的构建,让底座持续工作到目标达成,带完整的统一系统;三个底座(claude / codex / opencode)都可用(UMADEV_NO_GOAL_MODE=1 可退出)"], ["新增", "知识库内置进二进制:完整语料(418 份商业级工程规范 + 设计规则 + 你现有代码的结构图)随包内置,首次运行自动解压到 ~/.umadev/knowledge——零配置下发到每个用户项目,不再是用户机上的空语料"], ["新增", "检索与代码库理解:知识检索用 BM25(中文友好双通道分词)+ 可选向量层(OpenAI 或本地嵌入)+ RRF 融合;另有逐语言符号扫描(repo-map)给底座你现有代码的结构概览"], ["改进", "持续会话提速:对话跑在一个常驻底座会话上,启动时预加载(底座 + MCP + 系统提示只加载一次),首次回复不再扛旧的每条消息 30-60 秒冷启动;claude 现在逐 token 流式(--include-partial-messages),回复实时显示,而不是憋到最后一次性吐出"], ["改进", "终端渲染全面重做(对标 Claude Code):真实 Markdown(CJK 对齐表格 / 标题 / 嵌套列表 / 粗斜体 / 链接显 URL / 任务勾选框 / 分语言高亮代码块);文件改动渲染成实时 diff 卡,词级高亮(只点亮改动的词)、行号边栏、虚线框;干净的工具调用行(只读工具合并 + 长输出折叠,Ctrl+R 展开);构建完成卡列出改动文件 + 运行命令 + 自动浮出可点击的预览地址(http://localhost:PORT);流式打磨(稳定前缀缓存防卡顿 / 粘底 / 带微光的 spinner)"], ["改进", "三底座适配:claude / codex / opencode 三家都逐字流式、改代码都渲染 diff 卡、都进审计与治理;归一化 opencode 的工具形状(write→Write、filePath→file_path),让它的改代码也正常显示"], ["改进", "架构:总监模型——判断请求 → 拥有并驱动一份可见的依赖计划(渲染成实时清单)→ 按步调度角色团队(写代码串行、评审并行)→ 对照确定性底线验证 + 自纠 → 收尾产出交付证明。完整的九阶段链是最完整的那条路径(重型从零构建才走),不是每条消息被迫穿过的漏斗;文档(README 三语)同步全面重写"], ["修复", "对话回复不再「很久没反应、转圈变红冻住、最后一次性吐出」(根因:claude 没开 partial-messages,憋住整段文字);逐字流式时的空白(词间空格 / 段落换行)不再被吞,文字不再粘在一起;opencode 回复不再重复叠加"]] },
    { ver: "1.0.6", date: "2026-06-22", title: "TUI 交互硬化(对标 Claude Code / opencode)", changes: [["修复", "深读 Claude Code / opencode 源码整体修 TUI 交互:解耦「活着感」与吐字;修无滚动 / 无鼠标 / 小终端裁切 / opencode 不流式 / 静默阻塞等 P0 问题"], ["改进", "运行中可在空隙排队输入,ESC 中断当前工作,底座工具调用可见;运行锁单写者收敛"]] },
    { ver: "1.0.5", date: "2026-06-21", title: "Windows on ARM 支持 + 版本号统一", changes: [["平台", "新增 Windows on ARM(win32-arm64)支持 —— 骁龙 / Surface 等 ARM 架构的 Windows 现在会自动安装 x64 二进制,经 Windows 11 内置 x64 模拟运行;`npm install -g umadev` 不再报 unsupported platform"], ["修复", "1.0.4 的底座 .cmd 启动修复随本次全平台重编一并带入,Windows(含 ARM)的 claude / codex 识别 + 运行彻底打通"], ["改进", "全链路版本锁定:Cargo crate、npm 包、二进制 `--version` 三者同号;新增 `bump-version.sh` 一键改版脚本,杜绝「装 1.0.5 终端却显示 1.0.0」"]] },
    { ver: "1.0.4", date: "2026-06-21", title: "Windows 底座启动修复(os error 193)", changes: [["修复", "修复 Windows 下「找到底座却启动失败」的问题 —— 报错 `os error 193 / 不是有效的 Win32 应用程序`。npm 安装的 claude / codex 是 `.cmd` 垫片,不是 PE 可执行文件,CreateProcess 无法直接运行;改为经 `cmd /c` 启动(Rust 官方文档标准做法)"], ["平台", "同一套程序解析统一覆盖底座 CLI、npm 操作(audit / install / uninstall)与构建步骤(npm / tsc / cargo),凡 `.cmd`/`.bat` 一律 `cmd /c`"]] },
    { ver: "1.0.3", date: "2026-06-21", title: "Windows 底座识别修复", changes: [["修复", "修复 Windows 下识别不到底座的问题 —— npm 安装的 Claude Code / Codex / OpenCode 是 .cmd 垫片,此前裸名解析只认 .exe 导致检测失败;现按 PATHEXT 正确解析为 .cmd / .exe / .bat 全路径"], ["平台", "npm 操作(audit / install / uninstall)与构建步骤(npm / tsc / cargo)同步适配 Windows,统一走同一套程序解析"]] },
    { ver: "1.0.2", date: "2026-06-21", title: "安装后可执行修复", changes: [["修复", "修复部分环境安装后二进制无法执行(EACCES)的问题 —— npm 多包分发中转会剥掉可执行位,启动器现在运行前自动恢复 chmod +x,对所有平台兜底"]] },
    { ver: "1.0.1", date: "2026-06-21", title: "全平台一键安装", changes: [["平台", "全平台 npm 一键安装:Windows / Linux / Intel Mac / Apple Silicon Mac 均可 `npm install -g umadev`,按系统自动分发对应预编译二进制"], ["新增", "随包内置离线知识库,无需额外配置即可检索"]] },
    { ver: "1.0.0", date: "2026-06-21", title: "首个公开版本 — AI 编码项目总监 Agent", changes: [["新增", "完整 9 阶段商业交付流水线:research → docs → spec → frontend → backend → quality → delivery,含文档确认、预览确认两道人在环确认门"], ["新增", "三种本机 CLI 底座:Claude Code、Codex CLI、OpenCode —— 直接驱动你已登录的 CLI 并共享它自己的模型与推理强度,UmaDev 不持有任何 API key"], ["新增", "并行扇出:文档阶段并发起草架构与 UI/UX,缩短交付墙钟时间"], ["新增", "UIUX 一致性硬门 + 反 AI-slop 设计法:命名禁令(默认 indigo / 紫渐变 / emoji 图标 / 模板骨架)与设计 token 纪律,不符合声明设计系统的 UI 自动打回重做"], ["新增", "失败开放治理内核:写入前 hook + CI + 质量门补扫,禁 emoji 图标 / 硬编码颜色 / AI 套话;合规映射 SOC 2 · ISO 27001 · EU AI Act"], ["新增", "知识库:416 份工程规范文档,BM25 + 可选向量混合检索(RRF 融合),可接入团队自有知识库"], ["新增", "前后端契约校验:解析架构 API 表 → 渲染 OpenAPI → 校验前端 fetch 调用对齐"], ["新增", "自学习踩坑知识库:自动识别报错,按技术栈指纹在下次同类问题前主动规避"], ["新增", "质量门 + proof pack:scorecard.html 成绩单、proof-pack.zip 交付证明与审计证据链"], ["新增", "三语 TUI(简体 / 繁体 / English)、MCP server 与管理器;纯 Rust 单二进制,十个 crate,零外部进程依赖"]] },
  ],
  en: [
    { ver: "1.0.7", date: "2026-06-24", current: true, title: "Intent routing + a real build earns the full team review + a rebuilt terminal UI + 3-base parity", changes: [["Added", "Intent routing + unified builds: the default chat surface lets the base's own model judge each turn — chat / explain / small edit / debug / build; if the base is unreachable it takes the lightest path, no keyword table. The chat-vs-/run split is gone: what triggers the full flow is a real build, not a typed command — a build mentioned in chat earns the same systems as /run, and the base also decides by acting (its first file write turns the turn into a build)"], ["Added", "One always-on system: every real build automatically gets the design system / anti-AI-template rules (present on every working turn, no latency cost), a post-build governance + design scan, the role-team review (product / architecture / UI-UX / frontend / backend / QA / security — read-only forks, parallel, advisory), the curated knowledge digest, and learning from each run (records pitfalls, recalls them later). A small edit convenes a minimal UI team; a full build the whole roster"], ["Added", "The /goal command: `/goal <objective>` drives a goal-directed build that keeps the base working until the objective is met, with the full system; available on all three bases (UMADEV_NO_GOAL_MODE=1 opts out)"], ["Added", "Knowledge bundled into the binary: the full corpus (418 commercial-grade engineering standards + design rules + a map of your code) ships in the binary and auto-extracts to ~/.umadev/knowledge on first run — zero config, on every project, no longer an empty corpus on a user machine"], ["Added", "Retrieval + code awareness: knowledge retrieval uses BM25 (CJK-friendly dual-channel tokenization) + an optional vector layer (OpenAI or local embeddings) fused with RRF; plus a per-language symbol scan (repo-map) that gives the base an outline of your existing code"], ["Improved", "Persistent-session speed: chat runs on one resident base session pre-loaded at launch (base + MCP + system prompt loaded once), so the first reply no longer pays the old 30-60s per-message cold start; claude now streams token-by-token (--include-partial-messages) so a reply renders live instead of buffering until the end"], ["Improved", "Terminal rendering rebuilt (at Claude-Code parity): real Markdown (CJK-safe aligned tables, headings, nested lists, bold/italic, links that surface their URL, task-list checkboxes, per-language highlighted code); a file edit renders as a real-time diff card with word-level highlighting (only the changed words light up), a line-number gutter and a dashed frame; clean tool-call rows (read-only tools merged, long output folded, Ctrl+R to expand); a build-completion card with changed files + run command + an auto-surfaced clickable preview URL (http://localhost:PORT); streaming polish (stable-prefix cache, sticky-to-bottom, a shimmer spinner)"], ["Improved", "3-base parity: claude / codex / opencode all stream token-by-token, all render diff cards on file edits, all enter the audit + governance trail; opencode's tool shape is normalized (write→Write, filePath→file_path) so its edits display correctly"], ["Improved", "Architecture: the director model — judge the request → own and drive a visible dependency plan (a live checklist) → schedule the role team step by step (writers serial, reviewers parallel) → verify against a deterministic floor + self-correct → finalize with a delivery proof. The full nine-phase chain is the most complete path (only for a heavyweight greenfield build), not a funnel every message goes through; the README (all three languages) was rewritten end to end"], ["Fixed", "Chat replies no longer 'hang, spin red and freeze, then dump all at once' (root cause: claude wasn't streaming partial messages, buffering the whole text); whitespace in token streaming (inter-word spaces / paragraph breaks) is no longer dropped, so words don't mash together; opencode replies no longer duplicate"]] },
    { ver: "1.0.6", date: "2026-06-22", title: "TUI interaction hardening (Claude Code / opencode parity)", changes: [["Fixed", "Read the Claude Code / opencode source closely and overhauled TUI interaction: decoupled the sense of liveness from token streaming; fixed P0 issues — no scrolling, no mouse, small-terminal clipping, opencode not streaming, silent blocking"], ["Improved", "Queue input in the gaps while a run is in flight, ESC to interrupt the current work, base tool calls now visible; the single-writer run lock converged"]] },
    { ver: "1.0.5", date: "2026-06-21", title: "Windows on ARM support + version lock", changes: [["Platform", "Added Windows on ARM (win32-arm64) support — ARM Windows (Snapdragon / Surface) now installs the x64 build and runs it through the OS built-in x64 emulation; `npm install -g umadev` no longer reports an unsupported platform"], ["Fixed", "The 1.0.4 backend .cmd launch fix is rebuilt into every platform binary here, so claude / codex detection and execution work on Windows including ARM"], ["Improved", "End-to-end version lock: the Cargo crate, npm packages and the binary `--version` all carry the same number; a new `bump-version.sh` bumps them in one command, so you never install 1.0.5 and see 1.0.0"]] },
    { ver: "1.0.4", date: "2026-06-21", title: "Windows backend launch fix (os error 193)", changes: [["Fixed", "Fixed a Windows failure where the backend was found but could not launch — `os error 193 / not a valid Win32 application`. npm-installed claude / codex are `.cmd` shims, not PE executables, so CreateProcess cannot run them directly; they now launch via `cmd /c` (the documented standard approach in Rust)"], ["Platform", "One program-resolution path now covers the backend CLI, npm operations (audit / install / uninstall) and build steps (npm / tsc / cargo): any `.cmd`/`.bat` goes through `cmd /c`"]] },
    { ver: "1.0.3", date: "2026-06-21", title: "Windows backend-detection fix", changes: [["Fixed", "Fixed backend detection on Windows — npm-installed Claude Code / Codex / OpenCode are .cmd shims, and bare-name lookup only resolved .exe, so detection failed; now resolved to the full .cmd / .exe / .bat path via PATHEXT"], ["Platform", "npm operations (audit / install / uninstall) and build steps (npm / tsc / cargo) hardened for Windows through the same program-resolution path"]] },
    { ver: "1.0.2", date: "2026-06-21", title: "Post-install executable fix", changes: [["Fixed", "Fixed a post-install binary-not-executable (EACCES) failure on some setups — npm multi-package delivery strips the exec bit, so the launcher now restores chmod +x before running, on every platform"]] },
    { ver: "1.0.1", date: "2026-06-21", title: "Cross-platform one-line install", changes: [["Platform", "Cross-platform one-line install: `npm install -g umadev` on Windows / Linux / Intel Mac / Apple Silicon Mac, with the matching prebuilt binary auto-selected per system"], ["Added", "Offline knowledge base bundled with the package — retrieval works with zero extra setup"]] },
    { ver: "1.0.0", date: "2026-06-21", title: "First public release — AI coding project-director agent", changes: [["Added", "Full 9-phase commercial-delivery pipeline: research → docs → spec → frontend → backend → quality → delivery, with docs-confirm and preview-confirm human-in-the-loop gates"], ["Added", "Three local CLI backends — Claude Code, Codex CLI, OpenCode — driving your already-logged-in CLI and sharing its own model and reasoning effort; UmaDev holds no API key of its own"], ["Added", "Parallel fan-out: the docs phase drafts architecture and UI/UX concurrently to cut delivery wall-clock"], ["Added", "UIUX conformance gate + anti-AI-slop design law: named bans (default indigo / purple gradients / emoji icons / template skeletons) and design-token discipline; UI that drifts from the declared design system is auto-rejected and redone"], ["Added", "Fail-open governance kernel: pre-write hook + CI + quality-gate sweep; blocks emoji icons, hardcoded colors and AI-slop; compliance mapping for SOC 2 · ISO 27001 · EU AI Act"], ["Added", "Knowledge base: 416 engineering-standard docs, BM25 + optional vector hybrid retrieval (RRF fusion), pluggable team knowledge"], ["Added", "Frontend/backend contract validation: parse the architecture API table, render OpenAPI, and check that frontend fetch calls align"], ["Added", "Self-learning pitfall KB: auto-detects errors and proactively avoids the same class of problem next time by tech-stack fingerprint"], ["Added", "Quality gate + proof pack: scorecard.html, proof-pack.zip delivery proof and an audit evidence chain"], ["Added", "Trilingual TUI (Simplified / Traditional Chinese / English), MCP server + manager; pure-Rust single binary, ten crates, zero external process dependencies"]] },
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
