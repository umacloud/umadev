//! Coach mode — writes a self-contained instruction file per phase
//! that the selected base (Claude Code / Codex / OpenCode) executes.
//!
//! UmaDev does not need an API key when running inside a host that
//! already has model access. Instead of calling the model itself, the
//! coach writes a deterministic prompt file the host can read and
//! follow. After the host produces the required artifact, the user
//! runs `umadev continue` and UmaDev verifies + advances.
//!
//! This module is the *single source of truth* for what each phase
//! tells the host to do. Each `coach_<phase>` returns a complete
//! markdown document with:
//!
//! 1. The spec preamble (non-negotiable rules from
//!    `UMADEV_HOST_SPEC_V1`).
//! 2. The expert role definition.
//! 3. The task description.
//! 4. The required output path + section structure.
//! 5. The input context (requirement, knowledge digest, prior artifacts).
//! 6. The next step (`umadev continue`).

use std::fs;
use std::io;
use std::path::PathBuf;

use umadev_spec::Phase;

use crate::runner::RunOptions;

/// Subdirectory under `.umadev/` where coach prompts live.
pub const COACH_DIR: &str = ".umadev/coach";

/// Write the coach prompt for `phase` to `.umadev/coach/<NN>-<phase>.md`.
///
/// Returns the absolute path of the written file. The phase number
/// prefix matches `PHASE_CHAIN` ordering so the directory listing
/// reads top-to-bottom in pipeline order.
pub fn write_coach_prompt(opts: &RunOptions, phase: Phase) -> io::Result<PathBuf> {
    write_coach_prompt_with_vector(opts, phase, None)
}

/// Write the coach prompt with an optional pre-embedded query vector. The
/// async runner calls this after pre-embedding the requirement so every
/// phase's expert-knowledge section gets true BM25+vector fusion. When
/// `query_vec` is `None`, behaves identically to [`write_coach_prompt`].
pub fn write_coach_prompt_with_vector(
    opts: &RunOptions,
    phase: Phase,
    query_vec: Option<&[f32]>,
) -> io::Result<PathBuf> {
    let dir = opts.project_root.join(COACH_DIR);
    fs::create_dir_all(&dir)?;
    let body = render_coach_prompt_with_vector(opts, phase, query_vec);
    let path = dir.join(coach_filename(phase));
    fs::write(&path, body)?;
    // Mirror to CURRENT.md so the host's CLAUDE.md can point at one
    // stable path without needing to know the phase number.
    let current = dir.join("CURRENT.md");
    let header = format!(
        "<!-- Symlink-equivalent: the active phase is `{}`. See {}. -->\n",
        phase.id(),
        coach_filename(phase),
    );
    let mut current_body = header;
    current_body.push_str(&render_coach_prompt_with_vector(opts, phase, query_vec));
    fs::write(&current, current_body)?;
    Ok(path)
}

fn coach_filename(phase: Phase) -> String {
    let n = match phase {
        Phase::Research => 1,
        Phase::Docs => 2,
        Phase::DocsConfirm => 3,
        Phase::Spec => 4,
        Phase::Frontend => 5,
        Phase::PreviewConfirm => 6,
        Phase::Backend => 7,
        Phase::Quality => 8,
        Phase::Delivery => 9,
    };
    format!("{n:02}-{}.md", phase.id())
}

/// Pure renderer — exposed for tests.
#[must_use]
pub fn render_coach_prompt(opts: &RunOptions, phase: Phase) -> String {
    render_coach_prompt_with_vector(opts, phase, None)
}

/// Pure renderer with an optional pre-embedded query vector. Threads the
/// vector into [`phase_knowledge_digest_with_vector`] so the expert-knowledge
/// section gets RRF fusion when hybrid + vectors are available.
#[must_use]
pub fn render_coach_prompt_with_vector(
    opts: &RunOptions,
    phase: Phase,
    query_vec: Option<&[f32]>,
) -> String {
    let slug = opts.effective_slug();
    let req = &opts.requirement;
    let preamble = spec_preamble();
    let expert_knowledge =
        crate::phases::phase_knowledge_digest_with_vector(opts, phase, query_vec);
    let lessons =
        crate::lessons::relevant_lessons_for_prompt(&opts.project_root, &opts.requirement);
    let body = match phase {
        Phase::Research => render_research(&slug, req, opts, query_vec),
        // Docs writes the UIUX *spec* — it needs the archetype tokens + design
        // direction, but NOT the full implementation-time anti-slop hard-specs
        // (those belong in the frontend phase that writes code). Frontend gets
        // the full contract. Scoping per phase keeps each prompt on-task.
        Phase::Docs => render_docs(&slug, req, &load_design_system_inject(opts, phase)),
        Phase::DocsConfirm | Phase::PreviewConfirm => render_gate(phase, &slug),
        Phase::Spec => render_spec(&slug, req),
        Phase::Frontend => render_frontend(&slug, req, &load_design_system_inject(opts, phase)),
        Phase::Backend => render_backend(&slug, req),
        Phase::Quality => render_quality(&slug),
        Phase::Delivery => render_delivery(&slug),
    };
    let mcp_tools = load_mcp_tools(&opts.project_root);
    format!(
        "# UmaDev coach — phase `{}`\n\n\
         > Read this file and produce the required output. Then run \
         `umadev continue` to advance.\n\n\
         {preamble}\n\n\
         {body}\n\
         {expert_knowledge}\n{lessons}\n{mcp_tools}",
        phase.id()
    )
}

/// Load MCP tool information from `.mcp.json` so the coach prompt can tell
/// the host which external MCP tools are available (GitHub, DB, Search, etc.).
/// Returns an empty string if no MCP servers are configured.
pub(crate) fn load_mcp_tools(project_root: &std::path::Path) -> String {
    let path = project_root.join(".mcp.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return String::new();
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
        return String::new();
    };
    let Some(servers) = val.get("mcpServers").and_then(|s| s.as_object()) else {
        return String::new();
    };
    if servers.is_empty() {
        return String::new();
    }
    let mut lines = String::from("\n## Available MCP tools\n\n");
    lines.push_str(
        "The following MCP servers are configured and available. Use them when appropriate:\n\n",
    );
    for (name, config) in servers {
        let detail = if let Some(cmd) = config.get("command").and_then(|c| c.as_str()) {
            let args = config
                .get("args")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            format!("{cmd} {args}")
        } else if let Some(url) = config.get("url").and_then(|u| u.as_str()) {
            url.to_string()
        } else {
            "(configured)".to_string()
        };
        lines.push_str(&format!("- **{name}**: {detail}\n"));
    }
    lines.push('\n');
    lines
}

fn spec_preamble() -> String {
    "\
## Spec preamble (non-negotiable)\n\n\
You are operating inside a UmaDev pipeline run governed by \
`UMADEV_HOST_SPEC_V1`. Every artifact you produce MUST follow these \
rules:\n\n\
### Technical rules\n\
- Use a declared icon library (Lucide / Heroicons / Tabler). NEVER \
  emoji as functional icons.\n\
- Use design tokens (CSS vars / theme keys). NEVER hardcoded hex / \
  rgb / hsl colors in production UI.\n\
- Frontend `fetch` URLs MUST match an API path declared in the \
  architecture document.\n\
- Wait for explicit user approval at `docs_confirm` and \
  `preview_confirm` gates.\n\
- Output goes into structured markdown sections — do not invent new \
  top-level sections, fill the ones requested below.\n\n\
### Visual quality rules\n\
- Typography drives hierarchy: set a type scale before touching layout.\n\
- Generous whitespace: spacing tokens, not cramped divs.\n\
- Real-looking placeholder content, not \"Lorem ipsum\" or \"Example\".\n\
- Every interactive element needs hover + focus + disabled states.\n\
- No purple/pink gradient hero sections unless the product domain demands it.\n\
- No default-system-font-only designs; always declare a font stack.\n\
- No \"Welcome to [App]\" giant centered headings with no actual content.\n\
- No AI-chatbot shell layouts unless the product IS a chatbot.\n\
- Dark mode is REQUIRED. Every UIUX doc MUST include a `@media (prefers-color-scheme: dark)` \
  block that overrides surface/text/border tokens. Every frontend MUST wire it.\
"
    .to_string()
}

fn render_research(slug: &str, req: &str, opts: &RunOptions, query_vec: Option<&[f32]>) -> String {
    let knowledge =
        crate::phases::phase_knowledge_digest_with_vector(opts, Phase::Research, query_vec);
    format!(
        "## MUST-DO (read first)\n\n\
         1. **Print the FULL research brief as your text reply.** Do NOT use Edit / Write \
            tools to create the file — UmaDev captures your stdout.\n\
         2. **Include a `## Discovery` section** with Target audience, Visual tone, \
            Design direction (pick ONE of 5), Brand constraints, Platform, Complexity.\n\
         3. **Include `## Design system recommendation`** with color/typography/spacing choices.\n\
         4. Every section listed below is REQUIRED. Do not skip any.\n\n\
         ## Role\n\nSenior product researcher + design strategist.\n\n\
         ## Task\n\nProduce a research brief that grounds PM / architect / UI work. \
         This is the FOUNDATION — every later phase reads this document.\n\n\
         ## Sections (in order, ALL required)\n\n\
         - `# Research — {slug}`\n\
         - `## Requirement` (echo verbatim)\n\
         - `## Discovery`:\n\
           - **Target audience**: developers / consumers / enterprise / internal team\n\
           - **Visual tone**: professional / playful / technical / editorial / bold\n\
           - **Design direction**: ONE of: Modern Minimal / Editorial Clean / Tech Utility / Soft Warm / Bold Geometric\n\
           - **Brand constraints**: existing colors/fonts/logos, or \"greenfield\"\n\
           - **Platform**: web / mobile / desktop / CLI companion\n\
           - **Complexity**: simple (1-3) / medium (4-8) / complex (9+)\n\
         - `## Similar products` — 5 real products with design takeaways\n\
         - `## Domain risks` — 5 risks with mitigation strategies\n\
         - `## UI / UX must-haves` — 5 non-negotiable patterns\n\
         - `## Design system recommendation` — palette direction, typography, spacing, signature detail\n\
         - `## Open questions`\n\n\
         ## Self-check before submitting\n\n\
         - [ ] Discovery section present with all 6 fields answered?\n\
         - [ ] Design direction chosen (ONE of the 5 archetypes)?\n\
         - [ ] 5 similar products cited with design-specific (not feature-specific) takeaways?\n\
         - [ ] Content is in your text reply, NOT written to a file on disk?\n\n\
         ## Input\n\n\
         ### Requirement\n\n{req}\n\n\
         ### Local knowledge files available\n\n{knowledge}\n",
    )
}

/// The built-in design archetypes (seeded `knowledge/design-systems/*.md`).
/// `recommend_design_system` maps a requirement onto one of these so the
/// design system is ON BY DEFAULT — the user never has to run `/design`.
const DESIGN_ARCHETYPES: &[&str] = &[
    "modern-minimal",
    "editorial-clean",
    "tech-utility",
    "soft-warm",
    "bold-geometric",
    "brutalist-bold",
    "glass-aurora",
    "premium-luxury",
];

/// Pick the best-fit design archetype for a requirement by product-type
/// keyword reasoning. Returns `(archetype_id, product_label)`. This is what
/// makes the design system **default-on**: the frontend always gets a binding
/// token contract, even when the user never touches `/design`.
fn recommend_design_system(req: &str) -> (&'static str, &'static str) {
    let r = req.to_lowercase();
    // (archetype, human product label, trigger keywords). First hit wins, so
    // the more specific product types come before the versatile default.
    const RULES: &[(&str, &str, &[&str])] = &[
        (
            "premium-luxury",
            "高端/奢侈/财富/汽车",
            &[
                "奢侈",
                "luxury",
                "高端",
                "premium",
                "私行",
                "私人银行",
                "财富",
                "wealth",
                "腕表",
                "watch",
                "珠宝",
                "jewelry",
                "汽车",
                "automotive",
                "豪车",
                "会员",
                "membership",
                "精品",
                "high-end",
                "高定",
                "旗舰",
            ],
        ),
        (
            "brutalist-bold",
            "创意/作品集/文化/时尚",
            &[
                "作品集",
                "portfolio",
                "机构",
                "agency",
                "工作室",
                "studio",
                "时尚",
                "fashion",
                "音乐",
                "music",
                "艺术",
                "art",
                "文化",
                "culture",
                "创意",
                "creative",
                "宣言",
                "manifesto",
                "展览",
                "策展",
            ],
        ),
        (
            "glass-aurora",
            "AI/生成式/web3",
            &[
                "大模型",
                "llm",
                "gpt",
                "生成式",
                "aigc",
                "agent",
                "copilot",
                "chatbot",
                "聊天机器人",
                "对话式",
                "web3",
                "crypto",
                "区块链",
                "blockchain",
                "未来感",
                "科技感",
                "neural",
            ],
        ),
        (
            "editorial-clean",
            "内容/出版/媒体",
            &[
                "博客",
                "blog",
                "内容",
                "content",
                "文章",
                "article",
                "新闻",
                "news",
                "杂志",
                "magazine",
                "出版",
                "publish",
                "写作",
                "writing",
                "文档站",
                "知识库",
                "媒体",
                "media",
                "专栏",
                "newsletter",
                "教程",
                "wiki",
                "docs site",
            ],
        ),
        (
            "tech-utility",
            "开发者/数据后台/运维",
            &[
                "开发者",
                "developer",
                "dev tool",
                "开发工具",
                "监控",
                "monitor",
                "数据平台",
                "data platform",
                "analytics",
                "数据分析",
                "admin",
                "后台",
                "管理系统",
                "dashboard",
                "仪表盘",
                "infra",
                "基础设施",
                "终端",
                "terminal",
                "日志",
                "log",
                "运维",
                "devops",
                "api 平台",
                "数据库",
                "database",
                "ci/cd",
                "可观测",
            ],
        ),
        (
            "soft-warm",
            "消费/教育/生活/社区",
            &[
                "教育",
                "education",
                "学习",
                "learn",
                "课程",
                "儿童",
                "kids",
                "健康",
                "wellness",
                "健身",
                "fitness",
                "社交",
                "social",
                "社区",
                "community",
                "生活",
                "lifestyle",
                "记账",
                "旅行",
                "travel",
                "美食",
                "food",
                "宠物",
                "冥想",
                "亲子",
                "母婴",
                "情感",
                "约会",
                "dating",
            ],
        ),
        (
            "bold-geometric",
            "营销/品牌/金融",
            &[
                "落地页",
                "landing",
                "营销",
                "marketing",
                "品牌",
                "brand",
                "金融",
                "fintech",
                "支付",
                "payment",
                "钱包",
                "发布",
                "launch",
                "官网",
                "活动",
                "发布会",
                "保险",
                "证券",
                "投资",
                "游戏",
                "gaming",
            ],
        ),
        (
            "modern-minimal",
            "SaaS/工具/AI/效率",
            &[
                "saas",
                "b2b",
                "软件",
                "software",
                "productivity",
                "效率",
                "工具",
                "tool",
                "ai",
                "人工智能",
                "智能",
                "平台",
                "platform",
                "订阅",
                "subscription",
                "协作",
                "collaboration",
                "crm",
                "项目管理",
                "automation",
                "工作流",
                "助手",
                "assistant",
            ],
        ),
    ];
    for (archetype, label, kws) in RULES {
        if kws.iter().any(|k| r.contains(k)) {
            return (archetype, label);
        }
    }
    // Versatile, premium-but-neutral fallback — never "no design system".
    ("modern-minimal", "通用产品")
}

/// If the UIUX doc declared a `## Visual direction`, respect the archetype the
/// model committed to (it may have had good reason to override the
/// recommendation). Returns the declared archetype ONLY when it is unambiguous.
///
/// Deliberately scans just the `## Visual direction` SECTION, not the whole
/// doc: a doc that merely *mentions* another archetype (a comparison, or the
/// copied family-picker list) must not hijack the binding. And it requires
/// EXACTLY ONE archetype in that section — zero or several → `None`, so the
/// caller falls back to the deterministic product-type recommendation rather
/// than guessing wrong.
fn detect_declared_archetype(opts: &RunOptions) -> Option<String> {
    let path = opts
        .project_root
        .join("output")
        .join(format!("{}-uiux.md", opts.effective_slug()));
    let content = fs::read_to_string(&path).ok()?;
    let lower = content.to_lowercase();
    let section = heading_section(&lower, "visual direction")?;
    let mentioned: Vec<&str> = DESIGN_ARCHETYPES
        .iter()
        .filter(|a| section.contains(**a))
        .copied()
        .collect();
    match mentioned.as_slice() {
        [only] => Some((*only).to_string()),
        _ => None, // zero or ambiguous → use the recommendation
    }
}

/// Return the body text under the first markdown heading whose text contains
/// `keyword` (case-insensitive caller), up to the next `#`/`##` heading. Used
/// to scope archetype detection to the declared `## Visual direction` section.
fn heading_section(lower: &str, keyword: &str) -> Option<String> {
    let mut lines = lower.lines();
    // Advance to the heading line.
    let found = lines.by_ref().any(|l| {
        let t = l.trim_start();
        t.starts_with('#') && t.contains(keyword)
    });
    if !found {
        return None;
    }
    // Collect until the next heading.
    let mut out = String::new();
    for l in lines {
        if l.trim_start().starts_with('#') {
            break;
        }
        out.push_str(l);
        out.push('\n');
    }
    Some(out)
}

/// Load the active design system markdown + seed template from the
/// knowledge directory. **Default-on**: if the user did not pick a system
/// via `/design`, one is auto-recommended from the requirement's product
/// type (or read from the UIUX doc's declared direction) and injected as a
/// binding token contract anyway. Returns a ready-to-inject block.
pub(crate) fn load_design_system_inject(opts: &RunOptions, phase: Phase) -> String {
    let mut inject = String::new();

    // Resolve the EFFECTIVE design system, in priority order:
    //   1. explicit user choice (`/design <name>`)
    //   2. the archetype the UIUX doc already declared (frontend phase)
    //   3. an auto-recommendation by product type (always available)
    let (ds_name, source_note): (String, String) = if !opts.design_system.is_empty() {
        (
            opts.design_system.clone(),
            format!("用户通过 `/design {}` 选定", opts.design_system),
        )
    } else if let Some(declared) = detect_declared_archetype(opts) {
        let note = format!("沿用 UIUX 文档已声明的视觉方向 `{declared}`");
        (declared, note)
    } else {
        let (rec, label) = recommend_design_system(&opts.requirement);
        (
            rec.to_string(),
            format!(
                "**默认自动选定**(产品类型: {label}) → `{rec}`。无需用户手动 /design —— \
                 UmaDev 的设计系统默认就生效"
            ),
        )
    };

    let path = opts
        .project_root
        .join("knowledge/design-systems")
        .join(format!("{ds_name}.md"));
    if let Ok(content) = fs::read_to_string(&path) {
        inject.push_str("\n\n## 设计系统(绑定契约 · BINDING DESIGN CONTRACT)\n\n");
        inject.push_str(&source_note);
        inject.push_str(
            "。\n\n这是你的**强制设计契约**(默认生效,非可选):\n\
             - 逐字使用下面的 `:root` 设计 token、字体族、间距刻度、组件样式;不要另起一套颜色/字号。\n\
             - 在 UIUX 文档里用 `## Visual direction` 声明此方向并说明为何契合本产品。\n\
             - 只有有充分理由才可改档,改了也必须**仍是一套完整 token 契约**,绝不退回 generic(无 token、系统默认字体、紫渐变、emoji 图标)。\n\n",
        );
        inject.push_str(&content);

        // The taste / anti-AI-slop rules. They matter most at IMPLEMENTATION
        // time, so the full hard-spec file is inlined for the FRONTEND phase
        // (which writes code). The DOCS phase (writing the UIUX *spec*) gets a
        // concise direction summary + a pointer, so its prompt stays on-task
        // and isn't 75% implementation rules.
        let slop_path = opts
            .project_root
            .join("knowledge/design-systems/anti-ai-slop.md");
        if phase == Phase::Frontend {
            if let Ok(slop) = fs::read_to_string(&slop_path) {
                inject.push_str("\n\n");
                inject.push_str(&slop);
            }
        } else if slop_path.exists() {
            inject.push_str(
                "\n\n> 设计原则(精简)：先 commit 一个方向(Motif)+真实参照，反 generic —— \
                 distinctive 字体(非 Inter/Roboto 当主字)、OKLCH、主色+sharp accent、contextual 阴影、\
                 atmospheric 背景、motion 自证存在。在 UIUX 文档里把这些落成具体 token 与规则；\
                 完整反 AI-slop 硬规格(letter-spacing/缓动/字号等)见 `knowledge/design-systems/anti-ai-slop.md`，实现阶段严格执行。\n",
            );
        }
        // Point at the concrete per-product-type palette/font starter table so
        // the worker doesn't start from a blank page and reflex to generic.
        if opts
            .project_root
            .join("knowledge/design-systems/product-type-design-map.md")
            .exists()
        {
            inject.push_str(
                "\n\n> 起步调色板与字体对：先查 `knowledge/design-systems/product-type-design-map.md` \
                 取最接近本产品类型的一行(已做 WCAG 调整的 Primary/Accent/Background + 字体对)，再据所选档位细化。\n",
            );
        }
        if opts
            .project_root
            .join("knowledge/design-systems/aesthetic-families.md")
            .exists()
        {
            inject.push_str(
                "> 若上面的默认档位不完全契合，可查 `knowledge/design-systems/aesthetic-families.md` \
                 从全光谱家族里**具名 commit 一个**(neumorphism/cyberpunk/bento/spatial…)，绝不回退 generic。\n",
            );
        }
    }

    if !opts.seed_template.is_empty() {
        let path = opts
            .project_root
            .join("knowledge/seed-templates")
            .join(format!("{}.md", opts.seed_template));
        if let Ok(content) = fs::read_to_string(&path) {
            inject.push_str("\n\n## Active seed template\n\n");
            inject.push_str("The user selected this template via `/template ");
            inject.push_str(&opts.seed_template);
            inject.push_str("`. Follow its page structure and quality gates.\n\n");
            inject.push_str(&content);
        }
    }
    inject
}

fn render_docs(slug: &str, req: &str, design_inject: &str) -> String {
    format!(
        "## MUST-DO (read first)\n\n\
         1. **Print ALL THREE documents as your text reply.** Do NOT use Edit/Write tools.\n\
         2. **Read `output/{slug}-research.md` FIRST** — especially the Discovery section \
            and Design system recommendation. Bind your UIUX tokens to the direction chosen there.\n\
         3. **UIUX MUST include `@media (prefers-color-scheme: dark)` block** — dark mode is REQUIRED.\n\
         4. **Separate the three documents with `---` on its own line.**\n\n\
         ## Role\n\nThree domain experts in sequence:\n\n\
         1. **Senior Product Manager** — testable acceptance criteria, no ambiguity.\n\
         2. **Senior Software Architect** — consistent API surface, justified tech choices.\n\
         3. **Senior UI/UX Designer** — full design system (tokens + dark mode + components + states).\n\n\
         ## Task\n\nProduce the three core documents. Read the research brief FIRST, \
         then build each doc on the previous one.\n\n\
         ## Required outputs\n\n\
         ### 1. PRD — `output/{slug}-prd.md`\n\
         - `# PRD — {slug}`\n\
         - `## Goal` (2-4 sentences)\n\
         - `## Scope` (`in` and `out` bullets)\n\
         - `## User stories` (3-7 stories in `As a … I want … so that …` form)\n\
         - `## Acceptance criteria` (testable checkbox list, 5-10 items)\n\
         - `## Risks & open questions`\n\n\
         ### 2. Architecture — `output/{slug}-architecture.md`\n\
         - `# Architecture — {slug}`\n\
         - `## System overview` (1-3 paragraphs)\n\
         - `## Target platform & tech stack` (REQUIRED) — 商业开发不只 web。明确目标平台: \
           web / 移动 App(iOS/Android/鸿蒙) / 桌面(Win/macOS/Linux) / 小程序(微信/支付宝/…) / \
           或多端。声明每端的技术栈与理由(原生 vs 跨平台框架 Flutter/RN/uni-app/Taro/Tauri/KMP), \
           哪些代码共享(业务逻辑/数据/契约)、哪些按平台(UI/交互遵循各端设计规范 HIG/Material/HarmonyOS Design/小程序指南)。\
           后续客户端实现 MUST 按声明的平台与其平台标准来,不要默认只做 web。\n\
         - `## Architecture & layering` (REQUIRED, 商业级结构决策) — declare a \
           layered / clean architecture: interface(controller, 仅传输) → \
           application(service, 编排+事务, 一个用例=一个方法=一个事务边界, 收发 DTO 不泄露 entity) \
           → domain(entity/VO, 业务规则与不变量, 不要贫血) → infrastructure(repository/adapter, 持久化与外部); \
           dependencies flow inward. THEN list the **feature-module decomposition** \
           (package-by-feature: e.g. `orders` / `payments` / `users` / `auth`, each \
           with its own interface/application/domain/infrastructure; cross-module via \
           service interfaces / domain events). This section is binding — the backend \
           phase MUST build exactly these modules and layers.\n\
         - `## API surface` (markdown table `| Method | Path | Auth | Purpose |` with ≥5 rows; \
           every path starts with `/`; mark which endpoints require auth)\n\
         - `## Data model` (entities + fields + relations + key constraints/indexes; \
           金额非 float, 时间带时区)\n\
         - `## Tech-stack rationale` (one sentence per choice)\n\
         - `## Open trade-offs`\n\n\
         ### 3. UI/UX — `output/{slug}-uiux.md`\n\n\
         **IMPORTANT**: Before writing, check if `knowledge/design-systems/` exists in \
         the workspace. If it does, read the design system that matches the direction \
         chosen in the research brief's Discovery section (e.g. if \"Modern Minimal\" → \
         read `knowledge/design-systems/modern-minimal.md`). Use its color palette, \
         typography, spacing, and component patterns as your BINDING CONTRACT — copy \
         the CSS `:root` tokens verbatim, don't reinvent them.\n\n\
         If no matching design system file exists, create tokens from scratch following \
         the direction chosen below.\n\n\
         - `# UI/UX — {slug}`\n\
         - `## Visual direction` — pick ONE direction that fits the product domain. \
           Consider these archetypes:\n\
           - **Editorial Clean** — magazine-like, serif-accent headings, generous whitespace, \
             photography-driven. Best for: content sites, blogs, portfolio.\n\
           - **Modern Minimal** — geometric sans-serif, precise spacing, monochrome with one accent. \
             Best for: SaaS, dev tools, dashboards.\n\
           - **Tech Utility** — monospace accents, dense information, dark-mode-native. \
             Best for: CLI companions, code platforms, data tools.\n\
           - **Soft Warm** — rounded corners, warm palette, friendly illustrations. \
             Best for: consumer apps, education, wellness.\n\
           - **Bold Geometric** — high contrast, oversized type, asymmetric grid. \
             Best for: creative agencies, portfolios, brand launches.\n\
           State which you chose and ONE sentence why it fits this product. \
           Then define ALL tokens deterministically from that choice — the frontend \
           phase COPIES these tokens, it does not reinvent them.\n\
         - `## Color palette` — a `:root` CSS block with semantic tokens. \
           Require at minimum: `--color-bg`, `--color-surface`, `--color-text`, \
           `--color-text-secondary`, `--color-primary`, `--color-primary-hover`, \
           `--color-accent`, `--color-border`, `--color-error`, `--color-success`. \
           NO purple/pink gradients unless the product domain demands it.\n\
         - `## Dark mode` (put this RIGHT AFTER color palette) — a complete \
           `@media (prefers-color-scheme: dark)` block that overrides \
           bg/surface/text/border/shadow tokens. This is NOT optional.\n\
         - `## Typography system` — font stack (2 families max: one for headings, \
           one for body), type scale (7 steps: `--text-xs` through `--text-3xl`), \
           line-height tokens, font-weight tokens. NO system-font-only.\n\
         - `## Spacing scale` — mathematical progression (4px base), at least 8 \
           steps from `--space-1` (4px) to `--space-12` (48px).\n\
         - `## Icon library` — declare exactly ONE: Lucide / Heroicons / Tabler.\n\
         - `## Page hierarchy` — nested list with route paths.\n\
         - `## Component inventory` — list every component the frontend needs, with \
           states: default / hover / active / disabled / loading / error.\n\
         - `## Motion guidelines` — transition durations + easing functions as tokens \
           (`--transition-fast: 150ms ease-out`).\n\
         - `## Anti-patterns` — 5 things this design explicitly avoids \
           (e.g. \"no decorative hero gradients\", \"no AI-chat-shell layout\", \
           \"no emoji as functional icons\").\n\
         - `## Self-critique` — score this design on 5 dimensions (1-10 each): \
           Hierarchy clarity / Visual distinctiveness / Detail polish / \
           Functional completeness / Innovation. If any ≤ 6, revise before submitting.\n\
         - `## Accessibility notes` — contrast ratios, focus rings, ARIA landmarks.\n\n\
         ## Input\n\n\
         ### Requirement\n\n{req}\n\n\
         ### Research brief\n\n\
         Read `output/{slug}-research.md` for context.\n\n\
         ## Self-check before submitting\n\n\
         - [ ] All 3 documents present in your reply (PRD, Architecture, UIUX)?\n\
         - [ ] UIUX has `:root` CSS block with 10+ semantic color tokens?\n\
         - [ ] UIUX has `@media (prefers-color-scheme: dark)` block?\n\
         - [ ] UIUX has typography system (font stack + 5+ size levels)?\n\
         - [ ] Architecture has API surface table with ≥5 rows?\n\
         - [ ] PRD has ≥5 testable acceptance criteria?\n\
         - [ ] Content is in your text reply, NOT written to files?\n\
         - [ ] Three docs separated by `---` on its own line?\n\n\
         ## After you finish\n\n\
         Run `umadev continue` and approve the `docs_confirm` gate only after the user has reviewed.\n\
         {design_inject}",
    )
}

fn render_gate(phase: Phase, slug: &str) -> String {
    let (artifact_block, headline) = match phase {
        Phase::DocsConfirm => (
            format!(
                "- `output/{slug}-prd.md`\n\
                 - `output/{slug}-architecture.md`\n\
                 - `output/{slug}-uiux.md`"
            ),
            "Wait for the user to review the three core documents.",
        ),
        Phase::PreviewConfirm => (
            format!("- `output/{slug}-frontend-notes.md`\n- Live preview"),
            "Wait for the user to verify the frontend preview against the UI/UX spec.",
        ),
        _ => unreachable!("render_gate only handles DocsConfirm / PreviewConfirm"),
    };
    format!(
        "## Role\n\nGate keeper.\n\n\
         ## Task\n\n{headline}\n\n\
         ### Artifacts under review\n\n{artifact_block}\n\n\
         ## What to do now\n\n\
         - Wait. Do NOT advance.\n\
         - When the user is satisfied, they will run `umadev continue` themselves.\n\
         - If they request revisions, they will run `umadev revise \"<text>\"`. \
           In that case, re-execute the previous phase with the requested changes.\n",
    )
}

fn render_spec(slug: &str, req: &str) -> String {
    format!(
        "## Role\n\nSenior engineering manager / tech lead.\n\n\
         ## Task\n\nTranslate the approved PRD + Architecture + UIUX into a \
         concrete execution plan that a dev team can follow sprint-by-sprint.\n\n\
         ## Required outputs\n\n\
         ### 1. Execution plan — `output/{slug}-execution-plan.md`\n\n\
         - `# Execution plan — {slug}`\n\
         - `## Goal recap` — 2-3 sentences\n\
         - `## Module & layer plan` (商业级结构落地) — restate the feature modules \
           from the architecture doc's `## Architecture & layering` section, and for \
           each module list the work **by layer** in dependency order: \
           domain(entity/VO/rules) → repository(persistence) → application(service/use-case, \
           transactions) → interface(controller/API) → frontend(feature folder: api/components/hooks). \
           This is the binding skeleton the implementation phases build against.\n\
         - `## Tech stack setup` — exact commands to scaffold the project:\n\
           ```bash\n\
           npx create-next-app@latest {slug} --typescript --tailwind\n\
           cd {slug} && npm install ...\n\
           ```\n\
         - `## Sprint breakdown` — 2-3 sprints, each with:\n\
           - Sprint goal (one sentence)\n\
           - Tasks (numbered, each scoped to < 4 hours), **grouped by feature module + layer**\n\
           - Deliverable (what's shippable at sprint end)\n\
         - `## Coding standards` — the rules the dev (worker) MUST follow:\n\
           - Layering: controller(传输) → service(用例/事务/收发DTO) → domain(规则/不贫血) → repository(只持久化); deps inward\n\
           - Package-by-feature module structure\n\
           - File naming convention\n\
           - API client pattern (centralized typed client, no raw fetch in components)\n\
           - State management approach (server-cache / app-state / ui-state 分治)\n\
           - Error handling pattern (统一错误信封, 领域异常→接口层映射 HTTP)\n\
           - Test requirements (金字塔: 领域单元/服务mock编排/仓储集成/接口契约)\n\
         - `## Definition of done` — a PR is not done until:\n\
           - [ ] Every functional requirement it claims has its acceptance \
             scenario passing (the PRD's EARS `WHEN … SHALL …` IS the test oracle)\n\
           - [ ] UI matches UIUX design tokens\n\
           - [ ] API matches Architecture surface table\n\
           - [ ] No TypeScript errors\n\
           - [ ] No lint warnings\n\
           - [ ] Tested on mobile + desktop\n\
         - `## Risk register` — risks with mitigation\n\n\
         ### 2. Task list — `.umadev/changes/<change-id>/tasks.md`\n\n\
         Each task cites the functional requirement(s) it satisfies, so progress \
         and rework are traceable to the spec (Kiro-style):\n\
         `- [ ] [Sprint N] <description> _(FR-001, FR-003)_ (est: Xh)`\n\
         Every P0/P1 functional requirement MUST be covered by at least one task — \
         do not silently drop a requirement.\n\n\
         ## Input\n\n{req}\n\n\
         Read all three approved docs before writing.\n",
    )
}

fn render_frontend(slug: &str, req: &str, design_inject: &str) -> String {
    format!(
        "## Role\n\nSenior frontend engineer + visual craftsperson.\n\n\
         ## Task\n\nImplement a production-grade frontend that looks \
         professionally designed — not like an AI template.\n\n\
         ## Platform FIRST (按架构文档声明的目标平台来)\n\n\
         - Read the architecture doc's `## Target platform & tech stack`. Build the CLIENT for \
           THAT platform with its stack and follow its platform standard — web, mobile \
           (iOS/Android/鸿蒙), desktop (Tauri/Electron), or mini-program (微信/支付宝/uni-app). \
           Do NOT default to a web SPA if another platform was declared. UI/交互遵循该端设计规范 \
           (HIG / Material / HarmonyOS Design / 小程序指南). The scaffolding/build/run commands \
           below assume web — adapt them to the declared platform's toolchain.\n\n\
         ## Structure FIRST (商业级客户端架构)\n\n\
         - Organize **by feature** (`features/<x>/{{api,components,hooks,types}}`), not by type. \
           Cross-feature only via each feature's public index; no deep imports.\n\
         - **Data access isolated**: NO raw `fetch`/`axios` inside components — all requests go \
           through a typed API layer (per-feature `api/` module); paths match the architecture \
           doc; handle loading / error / empty for every data view.\n\
         - **State三类分治**: server data via React Query/SWR (don't hand-manage in Redux), \
           global client state via a store, UI state local.\n\
         - **Logic out of JSX**: computation/validation/derivation in pure functions/hooks; \
           presentational vs container components separated.\n\
         - **查阅前端/客户端标准库**：`knowledge/<platform>/01-standards/` 附带商业级标准——\
           web(web-framework-best-practices React/Next/Vue · forms · admin-dashboard-and-crud · i18n · accessibility · seo)、\
           移动(mobile + ios-design-hig/android-material-design)、鸿蒙(harmonyos-design)、小程序(miniprogram + WeUI design)、\
           桌面(desktop-design macOS/Windows)。按目标平台查对应标准，UI 遵循该端**官方设计规范**，注入的要落地、用到未注入的主动检索。\n\n\
         ## Design quality contract\n\n\
         Before writing ANY component, read `output/{slug}-uiux.md` and \
         lock these into your code:\n\
         1. **Typography first** — set the type scale in `:root` before \
            touching layout. Headings drive visual hierarchy.\n\
         2. **Whitespace is a feature** — generous padding/margin using \
            the spacing scale. No cramped layouts.\n\
         3. **Color from tokens only** — bind every `color`, \
            `background`, `border-color`, `box-shadow` to a CSS var. \
            Zero hardcoded hex/rgb/hsl.\n\
         4. **Real content** — use realistic placeholder text, not \
            \"Lorem ipsum\" or \"Example\". Names, dates, numbers that \
            feel authentic.\n\
         5. **Component states** — every interactive element must have \
            hover, active, focus, disabled styles. No state-less buttons.\n\
         6. **Dark mode** — if the UIUX doc defines dark tokens, wire \
            them via `prefers-color-scheme`.\n\
         7. **Responsive** — at minimum 2 breakpoints (mobile 360px, \
            desktop 1024px). Test both.\n\
         8. **Motion** — use the transition tokens from UIUX. Subtle \
            enter/exit animations on cards and modals.\n\n\
         ## Hard rules (will be audited)\n\n\
         - Icons from the declared library only. Zero emoji as icons.\n\
         - Every `fetch` URL MUST appear in `output/{slug}-architecture.md`.\n\
         - Run `npm run build` / `cargo check` — zero errors before submitting.\n\
         - Take a screenshot of the running app for the preview gate.\n\n\
         ## Anti-patterns to avoid (P0 cardinal sins)\n\n\
         - Purple/pink gradient hero sections\n\
         - Lorem ipsum / filler text\n\
         - \"Welcome to App\" generic headings\n\
         - Invented metrics without a source (\"10x faster\")\n\
         - Emoji used as functional icons\n\
         - Cards with identical placeholder text repeated 3x\n\
         - \"AI chatbot\" shell layout when the product is not a chatbot\n\
         - More than 2 accent-colored elements per viewport\n\
         - 3+ sections with identical layout (alternate the rhythm)\n\n\
         For the full P0/P1/P2 checklist, read \
         `knowledge/design-systems/00-craft-rules.md` if it exists.\n\n\
         Also check `knowledge/seed-templates/` for a matching page type \
         (saas-landing, dashboard, blog-content) — the seed template gives \
         you the section order and component patterns to follow.\n\n\
         ## Required output\n\n\
         - **Code:** actual runnable frontend files in project directory. NOT just a notes file.\n\
           Follow the file structure from `output/{slug}-architecture.md`.\n\
           Create real components, pages, layouts, API client.\n\
         - **Notes:** `output/{slug}-frontend-notes.md` with:\n\
           - ## Files created — list every file you wrote with purpose\n\
           - ## Architecture compliance — for each API endpoint in the architecture doc, \
             confirm frontend calls it correctly\n\
           - ## Design compliance — for each UIUX token, confirm it's used\n\
           - ## Test instructions — how to run and verify\n\
           - ## Known gaps — what's not implemented yet and why\n\n\
         ## Implementation checklist (follow in order)\n\n\
         1. Read execution plan → identify Sprint 1 tasks\n\
         2. Set up project scaffold (if not exists): `npx create-*` or equivalent\n\
         3. Copy design tokens from UIUX into CSS/theme file\n\
         4. Create shared components first (Button, Input, Card, Layout)\n\
         5. Create page components following page hierarchy from UIUX\n\
         6. Wire API client following architecture API surface\n\
         7. Add error handling (loading states, error states, empty states)\n\
         8. Test responsive (mobile + desktop)\n\
         9. Test dark mode\n\
         10. Run build — fix all errors\n\
         11. Write frontend-notes.md with compliance audit\n\n\
         ## Input\n\n\
         ### Requirement\n\n{req}\n\n\
         Read these in order:\n\
         1. `output/{slug}-uiux.md` — your PRIMARY visual guide. Every CSS var, \
            font choice, spacing value, and component pattern comes from here.\n\
         2. `output/{slug}-architecture.md` — API surface and tech stack.\n\
         3. `output/{slug}-prd.md` — acceptance criteria and user stories.\n\
         4. If `knowledge/design-systems/*.md` exists and the UIUX doc references \
            a design direction, read the matching file for detailed Do/Don't rules \
            and component patterns.\n\n\
         ## After you finish\n\n\
         Run the app, take a screenshot, ask the user to review. \
         They will approve via `umadev continue`.\n\
         {design_inject}",
    )
}

fn render_backend(slug: &str, req: &str) -> String {
    format!(
        "## Role\n\nSenior backend engineer.\n\n\
         ## Task\n\nImplement the complete backend: API routes, database schema, \
         authentication, validation, error handling, and tests.\n\n\
         ## Implementation requirements\n\n\
         ### Structure FIRST (商业级分层，先定骨架再写实现)\n\
         - Build EXACTLY the feature modules + layers declared in the architecture doc's \
           `## Architecture & layering` section (package-by-feature: each module has its own \
           interface / application(service) / domain / infrastructure).\n\
         - Controller = parse→authorize→call service→map response ONLY (no business logic, \
           no direct DB/SQL/ORM).\n\
         - Service = stateless; one method = one use-case = one transaction boundary; \
           takes/returns DTOs, NEVER exposes ORM entities; orchestrates, depends on injected \
           repository/gateway interfaces.\n\
         - Domain entities hold business rules/invariants (NOT anemic — `order.cancel()` checks its own state).\n\
         - Repository ONLY persists (no business logic, does not commit transactions).\n\
         - Dependencies flow inward; inner layers depend on interfaces, not concrete infra.\n\n\
         ### 查阅工程标准库（重要）\n\
         - 本项目 `knowledge/backend/01-standards/` 等目录附带商业级工程标准。**识别本项目用到的方面，查对应标准照着做**：\
           auth(认证授权) / payment(支付) / file-upload / background-jobs / email / search / realtime / analytics / \
           llm-application(AI·RAG·Agent·护栏) / data-modeling / secure-coding(OWASP) / config-and-observability / \
           microservices / backend-framework-idioms(所选框架地道写法)。注入的相关标准要落地，未注入但用到的主动检索 knowledge/。\n\n\
         ### API routes\n\
         - Implement EVERY endpoint from `output/{slug}-architecture.md` API surface table\n\
         - Follow the error convention from the architecture doc\n\
         - Input validation on every endpoint (reject malformed requests with 422)\n\
         - Rate limiting on auth endpoints\n\n\
         ### Database\n\
         - Create migration files for the data model from architecture doc\n\
         - Add indexes listed in the architecture doc\n\
         - Seed data for development (realistic test fixtures)\n\n\
         ### Authentication\n\
         - Implement the auth method specified in architecture doc\n\
         - Protect routes according to the permission matrix\n\
         - Token refresh / session management\n\n\
         ### Error handling\n\
         - Consistent error response format across all endpoints\n\
         - Log errors with context (request ID, user ID, timestamp)\n\
         - Never expose internal errors to clients\n\n\
         ### Testing\n\
         - Unit tests for business logic\n\
         - Integration tests for each API endpoint\n\
         - Test happy path + error paths + edge cases\n\
         - Test auth: unauthorized access returns 401, forbidden returns 403\n\n\
         ## Required output\n\n\
         - **Code:** actual backend files (routes, models, middleware, tests)\n\
         - **Notes:** `output/{slug}-backend-notes.md` with:\n\
           - ## API compliance — for each architecture endpoint, confirm handler exists\n\
           - ## Database — migration files created, seed data\n\
           - ## Auth — implementation details\n\
           - ## Tests — test count, what's covered, what's not\n\
           - ## Environment variables — list every env var needed to run\n\
           - ## How to run — exact commands to start the backend\n\n\
         ## Input\n\n{req}\n\n\
         Read `output/{slug}-architecture.md` (API surface + data model + auth design) \
         and `output/{slug}-prd.md` (acceptance criteria to test against).\n",
    )
}

fn render_quality(slug: &str) -> String {
    format!(
        "## Role\n\nQuality lead + design critic.\n\n\
         ## Task\n\nTwo-part quality check:\n\n\
         ### Part 1 — Automated gate\n\n\
         UmaDev runs the quality gate automatically. Review \
         `output/{slug}-quality-gate.md` and `output/{slug}-quality-gate.json`. \
         Fix anything flagged `failed` or `warning`.\n\n\
         ### Part 2 — Visual design critique (5 dimensions)\n\n\
         Open the running frontend and score it 1-10 on each dimension:\n\n\
         | Dimension | What to evaluate | Minimum |\n\
         |---|---|---|\n\
         | **Hierarchy** | Can a new user find the primary action in < 2 seconds? | 7 |\n\
         | **Distinctiveness** | Does it look like a custom design, not a template? | 7 |\n\
         | **Detail** | Hover states, focus rings, loading skeletons, empty states — all present? | 7 |\n\
         | **Function** | Every button works, every link navigates, every form validates? | 8 |\n\
         | **Polish** | Spacing consistent, type scale respected, dark mode works? | 7 |\n\n\
         If ANY dimension < minimum, go back to the frontend phase and fix it. \
         Append your scores to `output/{slug}-quality-gate.md`.\n\n\
         ## Required output\n\n\
         No new artifact — the gate report + your 5-dimension scores. Make sure \
         `quality_gate_passed: true` and all 5 dimensions ≥ minimum before delivery.\n",
    )
}

fn render_delivery(slug: &str) -> String {
    format!(
        "## Role\n\nRelease engineer.\n\n\
         ## Task\n\n`umadev continue` from the quality phase already:\n\n\
         - Wrote `output/{slug}-compliance-mapping.json` (SOC 2 / ISO 27001 / EU AI Act).\n\
         - Bundled `release/proof-pack-{slug}-<ts>.zip` containing every artifact and audit log.\n\n\
         ## What to do now\n\n\
         - Inspect the proof pack: `unzip -l release/proof-pack-{slug}-*.zip`.\n\
         - Hand the proof pack to the reviewer / compliance officer.\n\
         - Run `umadev report` later if you need to regenerate the compliance mapping.\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn recommend_design_system_maps_product_types() {
        // The reasoning engine routes each product type to its archetype so the
        // design system is on by default (no /design needed).
        assert_eq!(
            recommend_design_system("做一个技术博客写作平台").0,
            "editorial-clean"
        );
        assert_eq!(
            recommend_design_system("数据监控后台 dashboard").0,
            "tech-utility"
        );
        assert_eq!(recommend_design_system("儿童教育学习 app").0, "soft-warm");
        assert_eq!(
            recommend_design_system("金融支付落地页 marketing").0,
            "bold-geometric"
        );
        assert_eq!(
            recommend_design_system("a b2b saas tool").0,
            "modern-minimal"
        );
        // The expanded archetypes.
        assert_eq!(
            recommend_design_system("高端奢侈腕表品牌官网").0,
            "premium-luxury"
        );
        assert_eq!(
            recommend_design_system("设计工作室作品集网站").0,
            "brutalist-bold"
        );
        assert_eq!(
            recommend_design_system("AI 大模型聊天助手产品").0,
            "glass-aurora"
        );
        // Unknown → versatile default, never empty.
        assert_eq!(recommend_design_system("xyzzy").0, "modern-minimal");
    }

    #[test]
    fn detect_declared_archetype_only_trusts_unambiguous_visual_direction() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("output");
        std::fs::create_dir_all(&out).unwrap();
        let uiux = out.join("demo-uiux.md");
        let mut o = opts(tmp.path());
        o.slug = "demo".into();

        // Unambiguous: one archetype in the Visual direction section → that one,
        // even though another archetype is MENTIONED elsewhere in the doc.
        std::fs::write(
            &uiux,
            "# UIUX\n## Visual direction\n选用 tech-utility,数据密集。\n\
             ## Notes\n不要做成 modern-minimal 那种风格。\n",
        )
        .unwrap();
        assert_eq!(
            detect_declared_archetype(&o).as_deref(),
            Some("tech-utility")
        );

        // Ambiguous: two archetypes in the Visual direction section → None
        // (caller falls back to the deterministic recommendation).
        std::fs::write(
            &uiux,
            "# UIUX\n## Visual direction\n在 tech-utility 和 modern-minimal 之间权衡。\n",
        )
        .unwrap();
        assert_eq!(detect_declared_archetype(&o), None);

        // No Visual direction section → None.
        std::fs::write(&uiux, "# UIUX\nsome tokens, mentions soft-warm in prose.\n").unwrap();
        assert_eq!(detect_declared_archetype(&o), None);
    }

    #[test]
    fn design_system_binds_by_default_without_user_selection() {
        // With NO `/design` selection, a recommended archetype's tokens must
        // still inject as a binding contract (the default-on guarantee).
        let tmp = TempDir::new().unwrap();
        let ds_dir = tmp.path().join("knowledge/design-systems");
        std::fs::create_dir_all(&ds_dir).unwrap();
        std::fs::write(
            ds_dir.join("editorial-clean.md"),
            "# Editorial Clean\n:root { --text-base: 18px; }\n",
        )
        .unwrap();
        let mut o = opts(tmp.path());
        o.requirement = "做一个技术博客".into();
        o.design_system = String::new(); // user did NOT pick one
        let inject = load_design_system_inject(&o, Phase::Frontend);
        assert!(inject.contains("BINDING DESIGN CONTRACT"));
        assert!(inject.contains("默认自动选定"));
        assert!(inject.contains("--text-base")); // tokens actually injected
    }

    fn opts(root: &Path) -> RunOptions {
        RunOptions {
            project_root: root.to_path_buf(),
            requirement: "build a login system".into(),
            slug: "demo".into(),
            model: "stub".into(),
            backend: String::new(),
            design_system: String::new(),
            seed_template: String::new(),
        }
    }

    #[test]
    fn writes_coach_file_per_phase() {
        let tmp = TempDir::new().unwrap();
        for phase in [
            Phase::Research,
            Phase::Docs,
            Phase::DocsConfirm,
            Phase::Spec,
            Phase::Frontend,
            Phase::PreviewConfirm,
            Phase::Backend,
            Phase::Quality,
            Phase::Delivery,
        ] {
            let path = write_coach_prompt(&opts(tmp.path()), phase).unwrap();
            assert!(path.is_file(), "missing coach file for {phase:?}");
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.contains("UMADEV_HOST_SPEC_V1"));
            assert!(body.contains(phase.id()));
        }
        // CURRENT.md is always the latest
        assert!(tmp.path().join(".umadev/coach/CURRENT.md").is_file());
    }

    #[test]
    fn research_prompt_carries_requirement_and_discovery() {
        let body = render_coach_prompt(&opts(Path::new("/tmp")), Phase::Research);
        assert!(body.contains("build a login system"));
        assert!(body.contains("Similar products"));
        assert!(body.contains("Discovery"));
        assert!(body.contains("Target audience"));
        assert!(body.contains("Design direction"));
        assert!(body.contains("MUST-DO"));
        assert!(body.contains("Self-check"));
    }

    #[test]
    fn docs_prompt_demands_three_artifacts() {
        let body = render_coach_prompt(&opts(Path::new("/tmp")), Phase::Docs);
        assert!(body.contains("output/demo-prd.md"));
        assert!(body.contains("output/demo-architecture.md"));
        assert!(body.contains("output/demo-uiux.md"));
    }

    #[test]
    fn gate_prompts_tell_host_to_wait() {
        let body = render_coach_prompt(&opts(Path::new("/tmp")), Phase::DocsConfirm);
        assert!(body.to_lowercase().contains("wait"));
        assert!(body.contains("Do NOT advance"));
    }

    #[test]
    fn frontend_prompt_locks_hard_rules() {
        let body = render_coach_prompt(&opts(Path::new("/tmp")), Phase::Frontend);
        assert!(body.contains("Lucide"));
        assert!(body.contains("design tokens"));
        assert!(body.contains("frontend-api-calls.jsonl") || body.contains("architecture"));
    }

    #[test]
    fn coach_filename_is_zero_padded_phase_ordered() {
        assert_eq!(coach_filename(Phase::Research), "01-research.md");
        assert_eq!(coach_filename(Phase::Delivery), "09-delivery.md");
    }
}
