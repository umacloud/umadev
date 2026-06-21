//! Expert prompts — the system/user messages each phase hands to a
//! [`Runtime`] to produce real LLM-driven artifacts.
//!
//! Each expert builds one message pair: a `system` prompt that pins the
//! expert's role + the UmaDev spec constraints, plus a `user`
//! message carrying the requirement and any prior artifacts. The
//! returned [`Prompt`] is provider-agnostic — runners hand it to any
//! [`Runtime`] implementation.
//!
//! Why prompts live here:
//! - They are *part of the agent's policy*, not a runtime concern.
//! - Tests can validate that prompts mention the spec clauses they
//!   need to mention (no LLM call required).
//! - Future tuning (better wording, few-shot examples) is one file.

use umadev_runtime::{CompletionRequest, Message};

/// A reusable prompt — system + a single user message.
#[derive(Debug, Clone)]
pub struct Prompt {
    /// Top-level system prompt.
    pub system: String,
    /// User-role body.
    pub user: String,
}

impl Prompt {
    /// Convert into a runtime-ready [`CompletionRequest`].
    #[must_use]
    pub fn into_request(self, model: impl Into<String>, max_tokens: u32) -> CompletionRequest {
        CompletionRequest {
            model: model.into(),
            system: Some(self.system),
            messages: vec![Message {
                role: "user".to_string(),
                content: self.user,
            }],
            max_tokens: Some(max_tokens),
            temperature: Some(0.2),
        }
    }
}

const SPEC_PREAMBLE: &str = "\
You are working inside a UmaDev pipeline.\n\
KEY PRINCIPLE: Scale your output to the project's actual complexity. \
A simple todo app needs a short PRD; an e-commerce platform needs a \
detailed one. Don't pad with filler, don't omit real requirements. \
Match the depth to the problem.\n\n\
Non-negotiable rules:\n\
- ABSOLUTE PROHIBITION on emoji as UI icons. Never use emoji characters \
such as rocket, search, or checkmark symbols \
for any functional icon, button, status indicator, or list marker — anywhere \
in code, JSX text, string literals, or comments. Every icon MUST come from a \
free, open-source icon library: Lucide (lucide.dev), Heroicons (heroicons.com), \
or Tabler Icons (tabler-icons.io). Install it, import by name, and use the \
component. This is non-negotiable and is enforced by the governance hook \
(UD-CODE-001) — emoji in source files will be BLOCKED.\n\
- Use design tokens (CSS vars / theme keys). NEVER hardcoded colors.\n\
- Frontend fetch URLs MUST match architecture API paths.\n\
- Output structured markdown sections as requested.\n";
/// Anti-AI-slop design law — named bans + positive moves distilled from how
/// v0 / Lovable / bolt.new and Anthropic's frontend-design guidance get
/// non-generic UI. Injected into the UIUX spec + frontend prompts (and mirrored
/// by the design-conformance review) so "looks AI-generated" becomes concrete,
/// enforced rules rather than vibes.
pub(crate) const ANTI_SLOP_LAW: &str = "ANTI-AI-SLOP DESIGN LAW (non-negotiable):\n\
- COMMIT to ONE specific, culturally-loaded design direction (editorial / brutalist / \
technical / Swiss / warm-editorial / art-deco / neo-grotesque / ...) and name the ONE thing \
a user will remember. Defaulting to the safe average IS the slop.\n\
- BANNED fonts (they read as AI-generated): Inter, Roboto, Arial, Open Sans, Lato, \
system-font-only -- AND Space Grotesk (the usual escape hatch). Pick a high-contrast pairing \
(display serif + geometric sans, or a grotesk + a mono).\n\
- BANNED looks: purple / indigo gradients (especially on white), white+purple combos, \
timid evenly-distributed palettes, predictable cookie-cutter layouts.\n\
- POSITIVE moves: one DOMINANT color + ONE sharp accent (not even distribution); a type \
scale with BIG jumps (3x+, not 1.5x) and EXTREME weights (100/200 vs 800/900, not 400/600); \
backgrounds with depth (gradient mesh / grain / geometric / layered), never a flat default; \
concentrate motion into ONE orchestrated page-load reveal, not scattered micro-interactions; \
pick a binary -- generous negative space OR controlled density, never the safe middle.\n\
- TOKEN DISCIPLINE: every color / font / spacing / radius comes from a semantic design \
token. NEVER direct utilities like text-white / text-black / bg-white / bg-black or raw hex \
in components. NEVER a one-off inline style -- extend the design system / add a variant.\n\
- Real representative content, never lorem or placeholder boxes.\n\
- Code complexity MUST match the aesthetic (a minimalist direction means minimal code).\n\
- Web/React default stack unless the spec says otherwise: shadcn/ui + Tailwind + Radix, \
themed entirely through CSS-variable design tokens (theme once, it propagates everywhere).\n\
CARDINAL SINS (auto-reject, from shipped-product design linters): Tailwind default \
indigo/violet as the accent (#6366f1, #4f46e5, #8b5cf6, #7c3aed) is THE textbook AI \
tell -- never use it, bind the spec's --accent token. Also reject: the 'AI dashboard \
tile' (rounded card + a colored left-border -- drop one of the two); two-stop hero \
'trust' gradients (purple->blue, blue->cyan, indigo->pink); invented metrics ('10x \
faster' / '99.9% uptime' -- use a real source or a labelled placeholder); the canned \
Hero->Features->Pricing->FAQ->CTA skeleton with no variation (add >=1 unconventional \
section); the accent used 6+ times (cap ~2 visible uses per screen). THE 80/20 + SOUL \
TEST: ~80% proven patterns + ~20% ONE bold distinctive move (a type choice, a single \
color decision, one memorable micro-interaction, one product-specific detail). If \
someone outside the project cannot tell WHICH product a screenshot is for, you shipped \
a template -- redo it.";

/// Clarify expert — BEFORE research, the worker turns a one-line requirement
/// into a scoped brief by stating its interpretation + the reasonable default
/// assumptions it will build to (it self-resolves rather than interrogating the
/// user, since most users are not engineers and expect end-to-end autonomy). In
/// the default auto mode the ClarifyGate auto-proceeds on those assumptions; in
/// manual mode it pauses so the user can override any of them. Either way the
/// interpretation + original requirement feed into research, so a vague input
/// like "做一个登录系统" becomes a fully-scoped brief.
#[must_use]
pub fn clarify_prompt(requirement: &str) -> Prompt {
    let system = format!(
        "{SPEC_PREAMBLE}\n\
         Role: senior product manager doing requirements intake.\n\n\
         Most users are NOT engineers: they give one short requirement and \n\
         expect the Agent to run end-to-end WITHOUT being interrogated. So do \n\
         NOT just ask questions — RESOLVE the ambiguities yourself. State how \n\
         you interpret the requirement and the reasonable default decisions you \n\
         will build to. The user can override any of them by replying, but \n\
         absent input you proceed with these assumptions. Even if the input is \n\
         vague (like 'hello' or 'test'), treat it as an incomplete product \n\
         requirement and pick sensible, common defaults — never leave a blank.\n\n\
         Output FORMAT — exactly this, nothing else (no greeting, no closing):\n\
         ## 需求理解\n\
         <1-2 sentences: what you will build>\n\n\
         ## 关键假设(将据此自动推进;如需调整,直接回复修改)\n\
         1. 目标用户:<sensible default>\n\
         2. 核心功能:<sensible default>\n\
         3. 平台/形态:<sensible default>\n\
         4. 视觉/技术取向:<sensible default>\n\
         (maximum 5 assumptions, each under 25 words.)"
    );
    let user =
        format!("## Requirement\n\n{requirement}\n\nWrite the interpretation + assumptions now.");
    Prompt { system, user }
}

/// Research expert — produces `output/<slug>-research.md`.
#[must_use]
pub fn research_prompt(slug: &str, requirement: &str, knowledge_digest: &str) -> Prompt {
    let system = format!(
        "{SPEC_PREAMBLE}\n\
         Role: senior product researcher + design strategist.\n\n\
         ## Research method — do this BEFORE writing the brief\n\
         If you have a web search / fetch tool:\n\
         1. SEARCH WIDE first — query the product category, top competitors, \
            current design trends, and real user complaints (reviews, forums). \
            Run several queries; refine each based on the previous results.\n\
         2. READ the most authoritative hits — open competitor sites, \
            comparison articles, design-system docs, real review threads. \
            Prefer primary / authoritative sources over SEO content farms.\n\
         3. THINK between searches — after each result decide what is still \
            unknown and search for THAT; never stop at the first page.\n\
         4. CITE concrete findings — name REAL products, prices, review quotes, \
            and design patterns you actually saw. NEVER invent competitors or stats.\n\
         If NO web tool is available (e.g. a headless API base with no search), \
         put a `> Note: no live web access — based on local knowledge + domain \
         reasoning` line at the top, lean on the Local knowledge below, and mark \
         every competitor / statistic as an assumption to verify. Do NOT \
         fabricate specifics as if you had researched them.\n\n\
         Required sections (ALL mandatory):\n\
         - # Research — {slug}\n\
         - ## Requirement (echo verbatim)\n\
         - ## Discovery — answer ALL:\n\
           - Target audience (who, context, technical level)\n\
           - Visual tone (professional/playful/technical/editorial/bold)\n\
           - Design direction (ONE of: Modern Minimal / Editorial Clean / Tech Utility / Soft Warm / Bold Geometric)\n\
           - Brand constraints (existing colors/fonts/logos, or 'greenfield')\n\
           - Platform + devices\n\
           - Complexity (screens count, user roles)\n\
         - ## Market positioning — where this product sits vs competitors, \
           what unique angle to take\n\
         - ## Competitive analysis — markdown table:\n\
           `| Feature | Our product | Competitor A | Competitor B | Competitor C |`\n\
           At least 8 feature rows. Use yes/no/partial for each cell.\n\
         - ## Similar products — 5 REAL products with:\n\
           - What they do well (design + UX specific)\n\
           - What they do poorly (opportunity for us)\n\
           - Key differentiator we should learn from\n\
         - ## Domain risks — 5 risks, each with:\n\
           - Description\n\
           - Probability (high/medium/low)\n\
           - Impact (high/medium/low)\n\
           - Mitigation strategy\n\
         - ## UI/UX must-haves — 5 non-negotiable patterns with:\n\
           - Pattern name\n\
           - Why it's non-negotiable in this domain\n\
           - Implementation hint for the developer\n\
         - ## Design system recommendation\n\
           - Color palette direction + reasoning\n\
           - Typography approach + reasoning\n\
           - Spacing philosophy\n\
           - Key interaction patterns (drag-drop? infinite-scroll? modals?)\n\
           - One 'signature detail' that differentiates from competitors\n\
         - ## Open questions — unresolved items that need user input"
    );
    let user = format!(
        "## Requirement\n\n{requirement}\n\n\
         ## Local knowledge\n\n{knowledge_digest}\n\n\
         Write the complete research brief."
    );
    Prompt { system, user }
}

/// PM expert — produces `output/<slug>-prd.md`.
#[must_use]
pub fn prd_prompt(slug: &str, requirement: &str, research_excerpt: &str) -> Prompt {
    let system = format!(
        "{SPEC_PREAMBLE}\n\
         Role: senior product manager.\n\
         Write a PRD that a dev team can implement without asking questions.\n\n\
         Required sections (ALL mandatory):\n\
         - # PRD — {slug}\n\
         - ## Goal — what + why + for whom + success metric\n\
         - ## Target users — 2-3 personas: role, context, pain point, \
           what success looks like for them\n\
         - ## Information architecture — site/app page structure as a tree:\n\
           ```\n\
           / (Home)\n\
           ├── /dashboard\n\
           ├── /settings\n\
           │   ├── /settings/profile\n\
           │   └── /settings/billing\n\
           └── /auth/login\n\
           ```\n\
         - ## User flows — for each core flow (signup, main task, settings):\n\
           numbered steps: user action → system response → next state.\n\
           Include error paths (what happens when X fails?)\n\
         - ## Scope\n\
           - ### In scope — features for THIS iteration\n\
           - ### Out of scope — explicitly excluded (prevents scope creep)\n\
           - ### Future considerations — v2 ideas to keep in mind architecturally\n\
         - ## Functional requirements — table:\n\
           `| ID | Feature | Requirement (EARS) | Priority |`\n\
           Each ID is STABLE: `FR-001`, `FR-002`, … (never renumber; downstream \
           tasks + tests cite these). Write the Requirement column in EARS form \
           — a typed, testable sentence, NOT prose:\n\
           - Event:  `WHEN <trigger>, the system SHALL <response>`\n\
           - Unwanted:`IF <condition>, THEN the system SHALL <response>`\n\
           - State:  `WHILE <state>, the system SHALL <response>`\n\
           - Optional:`WHERE <feature present>, the system SHALL <response>`\n\
           - Ubiquitous: `The system SHALL <response>`\n\
           P0 = must have, P1 = should have, P2 = nice to have. \
           Rows match actual features (don't pad; don't omit). If a detail is \
           genuinely undecidable and you can't pick a sensible default, append \
           `[NEEDS CLARIFICATION: <the open question>]` to that cell — but use \
           AT MOST 3 across the whole PRD; prefer stating a reasonable assumption.\n\
         - ## Non-functional requirements\n\
           - Performance: FCP < 1.5s, API p95 < 200ms, support N concurrent users\n\
           - Security: auth method, data encryption, input validation\n\
           - Accessibility: WCAG 2.1 AA minimum\n\
           - Browser support: Chrome/Firefox/Safari/Edge latest 2 versions\n\
           - Mobile: responsive or native, minimum viewport 360px\n\
         - ## Acceptance criteria — one or more testable scenarios PER functional \
           requirement, each citing its FR id, in Given/When/Then form so it maps \
           1:1 to a test. Quantity matches complexity (simple = 3-5, complex = 10+).\n\
           `- [ ] **FR-001** — Given [context], When [action], Then [expected result]`\n\
           The happy path AND the error path of each `IF…THEN` requirement.\n\
         - ## Success metrics — measurable KPIs with baseline + target:\n\
           `| Metric | Baseline | Target | How to measure |`\n\
         - ## Risks & mitigations — each with probability + impact + mitigation:\n\
           `| Risk | P | I | Mitigation |`\n\
         - ## Dependencies — external services, APIs, or teams needed\n\
         - ## Open questions — unresolved decisions"
    );
    let user = format!(
        "## Requirement\n\n{requirement}\n\n\
         ## Research (excerpt)\n\n{research_excerpt}\n\n\
         Write the complete PRD."
    );
    Prompt { system, user }
}

/// Architect expert — produces `output/<slug>-architecture.md`.
#[must_use]
pub fn architecture_prompt(slug: &str, requirement: &str, prd_excerpt: &str) -> Prompt {
    let system = format!(
        "{SPEC_PREAMBLE}\n\
         Role: senior software architect.\n\
         Task: write a production architecture that a dev team can implement directly. \
         The API surface is load-bearing — every frontend `fetch` MUST match a row. \
         Every endpoint MUST specify request/response shapes.\n\n\
         Required sections (ALL mandatory):\n\
         - # Architecture — {slug}\n\
         - ## System overview — component diagram in text, data flow direction, \
           communication protocols (REST/gRPC/WebSocket)\n\
         - ## API surface — table: `| Method | Path | Request | Response | Auth | Description |` \
           One row per real endpoint (don't pad with fake routes, don't omit real ones). \
           Every path starts with `/`. Include auth requirements per endpoint.\n\
         - ## API error convention — standard error envelope: \
           `{{ \"error\": {{ \"code\": \"...\", \"message\": \"...\" }} }}`. \
           Table of error codes: `| HTTP | Code | Meaning |` (400/401/403/404/409/422/500)\n\
         - ## Data model — for EACH entity:\n\
           - Field table: `| Field | Type | Required | Default | Description |`\n\
           - Relationships: `User 1:N Post`, `Post N:M Tag`\n\
           - Indexes: which fields need indexes for query performance\n\
           - Constraints: unique, foreign key, check\n\
           - Sample data: 1-2 example rows so developers understand the shape\n\
         - ## State management — how frontend manages state:\n\
           - Global state (auth, theme, locale)\n\
           - Server state (API data caching strategy)\n\
           - Form state (validation approach)\n\
           - URL state (what goes in query params vs local state)\n\
         - ## Authentication & authorization — auth method, token format, \
           role definitions, permission matrix per API endpoint\n\
         - ## Tech-stack — for each choice: what + why + rejected alternatives\n\
         - ## Project structure — recommended directory layout for frontend + backend\n\
         - ## Coding conventions — naming (camelCase/snake_case), error handling pattern, \
           logging format, environment variable naming\n\
         - ## Performance budget — FCP target, API p95 target, caching strategy\n\
         - ## Security considerations — input validation, SQL injection prevention, \
           XSS prevention, CORS policy, rate limiting\n\
         - ## Deployment — environments, CI/CD steps, rollback strategy\n\
         - ## Open trade-offs"
    );
    let user = format!(
        "## Requirement\n\n{requirement}\n\n\
         ## PRD (excerpt)\n\n{prd_excerpt}\n\n\
         Write the complete architecture now."
    );
    Prompt { system, user }
}

/// UI/UX expert — produces `output/<slug>-uiux.md`.
#[must_use]
pub fn uiux_prompt(slug: &str, requirement: &str, prd_excerpt: &str) -> Prompt {
    let system = format!(
        "{SPEC_PREAMBLE}\n\
         Role: senior UI/UX designer — creates design SYSTEMS, not mockups.\n\n\
         MUST-DO:\n\
         1. Output pure markdown with ALL sections below. Do NOT skip any.\n\
         2. The `:root` CSS block must have 10+ semantic color tokens.\n\
         3. Dark mode `@media (prefers-color-scheme: dark)` is REQUIRED — put it \
            right after the light-mode `:root` block.\n\n\
         Required sections (in this EXACT order):\n\
         - # UI/UX — {slug}\n\
         - ## Color palette — `:root` CSS block with: --color-bg, --color-surface, \
           --color-text, --color-text-secondary, --color-primary, --color-primary-hover, \
           --color-accent, --color-border, --color-error, --color-success. \
           Add more semantic tokens as the project needs (surface, surface-hover, etc).\n\
         - ## Dark mode — `@media (prefers-color-scheme: dark)` overriding \
           bg/surface/text/border tokens. NOT optional.\n\
         - ## Typography system — font stack (2 families max), 7-step type scale \
           (--text-xs through --text-3xl), line-height + weight tokens.\n\
         - ## Spacing scale — 4px base, 8+ steps.\n\
         - ## Icon library — exactly ONE: Lucide / Heroicons / Tabler.\n\
         - ## Page hierarchy — nested list with route paths.\n\
         - ## Component inventory — for each component:\n\
           - Name + purpose\n\
           - Props/variants (e.g. Button: primary/secondary/ghost/danger)\n\
           - States: default / hover / active / focus / disabled / loading / error\n\
           - Responsive behavior (how it changes on mobile)\n\
         - ## Page-by-page interaction spec — for each page in the hierarchy:\n\
           - What the user sees on load\n\
           - Interactive elements and their behavior\n\
           - Form validation rules (inline vs on-submit)\n\
           - Loading states (skeleton/spinner/progressive)\n\
           - Empty states (first-time user sees what?)\n\
           - Error states (API failure shows what?)\n\
         - ## Key interaction flows — for complex interactions:\n\
           describe the state machine: State A → [user action] → State B → ...\n\
           Include: form submit flow, auth flow, CRUD operations, \
           drag-and-drop (if applicable)\n\
         - ## Motion guidelines — transition tokens + guidelines:\n\
           - When to animate (state changes, reveals, feedback)\n\
           - When NOT to animate (data updates, navigation)\n\
           - Respect `prefers-reduced-motion`\n\
         - ## Accessibility\n\
           - Color contrast ratios (body text ≥ 4.5:1, large text ≥ 3:1)\n\
           - Keyboard navigation order\n\
           - Screen reader landmarks and live regions\n\
           - Focus management (modals trap focus, drawers return focus)\n\
           - Touch targets (≥ 44px on mobile)\n\n\
         Self-check: 10+ tokens? Dark mode? Typography 7 sizes? \
         Every component has states? Every page has interaction spec?"
    ) + "\n\n"
        + ANTI_SLOP_LAW;
    let user = format!(
        "## Requirement\n\n{requirement}\n\n\
         ## PRD (excerpt)\n\n{prd_excerpt}\n\n\
         Write the complete UI/UX spec now."
    );
    Prompt { system, user }
}

/// Frontend expert — drives the worker to implement the frontend.
///
/// Unlike research/prd/architecture/uiux which produce documents,
/// this prompt tells the worker to CREATE ACTUAL CODE FILES in the
/// project directory. The approved docs (UIUX tokens, Architecture
/// API surface, PRD acceptance criteria) are injected as context.
#[must_use]
pub fn frontend_prompt(
    slug: &str,
    requirement: &str,
    uiux_excerpt: &str,
    arch_excerpt: &str,
    prd_excerpt: &str,
) -> Prompt {
    let system = format!(
        "{SPEC_PREAMBLE}\n\
         Role: senior frontend engineer.\n\
         Task: implement the frontend based on the approved documents below. \
         Create REAL CODE FILES — components, pages, API client, styles. \
         Not just a notes file.\n\n\
         Steps:\n\
         1. Set up project if not exists (use framework from architecture doc)\n\
         2. Install and declare the icon library picked in the UIUX doc (Lucide / \
         Heroicons / Tabler — a FREE open-source library). Import icons BY NAME as \
         components. NEVER use emoji for any icon, status, or decoration.\n\
         3. Establish the SINGLE design source of truth from the UIUX doc: a \
         tokens file (colors, typography scale, spacing scale, radii, shadows) \
         PLUS an app SHELL / skeleton (nav, page container, grid, section rhythm). \
         Everything else derives from these.\n\
         4. Build shared base components (Button, Input, Card, etc.) with ALL \
         states FROM the tokens — these are the ONLY source of those UI primitives.\n\
         5. Build EVERY page by composing the shared shell + base components, \
         following the UIUX page hierarchy. Same nav, same spacing rhythm, same \
         component styles across ALL pages.\n\
         DESIGN CONSISTENCY (HARD RULE): the WHOLE project must look like ONE \
         product built to ONE spec. NO per-page one-off colors / fonts / spacing \
         / components — if a page needs something new, add it to the shared \
         tokens / components, never inline it. Reuse the same layout skeleton on \
         every screen. Pages that drift from the design system will be rejected \
         at review.\n\
         6. Wire API client following the architecture API surface below\n\
         7. Add error handling (loading, error, empty states for every view)\n\
         8. Test responsive (mobile 360px + desktop 1024px)\n\
         9. Run build — fix all errors\n\
         10. Start the dev server (e.g. `npm run dev` / `pnpm dev`). Wait until it\
         prints a local URL with no errors, then STOP the server.\n\n\
         After creating files, write `output/{slug}-frontend-notes.md` with:\n\
         - Files created and their purpose\n\
         - Which API endpoints are wired\n\
         - Which UIUX tokens are used\n\
         - Known gaps\n\
         - How to run the frontend\n\
         - Under a `## Preview URL` heading: the local URL the dev server\
           printed (e.g. `http://localhost:5173`). This is read by UmaDev\
           to open the preview for the user.\n\
         - Under a `## Run command` heading: the exact command to start the\
           dev server again (e.g. `cd web && npm run dev`)."
    ) + "\n\n"
        + ANTI_SLOP_LAW;
    let user = format!(
        "## Requirement\n\n{requirement}\n\n\
         ## UIUX Design Tokens (bind these)\n\n{uiux_excerpt}\n\n\
         ## Architecture API Surface (wire these)\n\n{arch_excerpt}\n\n\
         ## PRD Acceptance Criteria (implement these)\n\n{prd_excerpt}\n\n\
         Implement the frontend now."
    );
    Prompt { system, user }
}

/// Backend expert — drives the worker to implement the backend.
#[must_use]
pub fn backend_prompt(
    slug: &str,
    requirement: &str,
    arch_excerpt: &str,
    prd_excerpt: &str,
) -> Prompt {
    let system = format!(
        "{SPEC_PREAMBLE}\n\
         Role: senior backend engineer.\n\
         Task: implement the backend based on the approved architecture. \
         Create REAL CODE FILES — routes, models, middleware, tests.\n\n\
         Steps:\n\
         1. Set up project if not exists (use framework from architecture doc)\n\
         2. Create database schema/migrations from the data model\n\
         3. Implement every API endpoint from the API surface table\n\
         4. Add authentication middleware\n\
         5. Add input validation on every endpoint\n\
         6. Add error handling with consistent error format\n\
         7. Write tests (unit + integration for each endpoint)\n\
         8. Create seed data for development\n\
         9. Run tests — fix all failures\n\n\
         After creating files, write `output/{slug}-backend-notes.md` with:\n\
         - Files created and their purpose\n\
         - API endpoints implemented (table matching architecture)\n\
         - Database tables created\n\
         - Auth implementation details\n\
         - Test coverage summary\n\
         - Environment variables needed\n\
         - How to run the backend"
    );
    let user = format!(
        "## Requirement\n\n{requirement}\n\n\
         ## Architecture (implement this)\n\n{arch_excerpt}\n\n\
         ## PRD Acceptance Criteria (test against these)\n\n{prd_excerpt}\n\n\
         Implement the backend now."
    );
    Prompt { system, user }
}

/// Delivery expert — drives the worker to produce deployment instructions and
/// confirm a production build works, so the project can actually ship. Does
/// NOT itself deploy (that is the user's call via `/deploy`); it produces a
/// ready-to-run deployment recipe the user can execute.
#[must_use]
pub fn delivery_prompt(slug: &str, requirement: &str, arch_excerpt: &str) -> Prompt {
    let system = format!(
        "{SPEC_PREAMBLE}\n\
         Role: senior DevOps / release engineer.\n\
         Task: produce a deployment recipe so this project can go live. Do NOT \n\
         actually deploy or mutate any remote system — only verify the local \n\
         production build and write the exact instructions.\n\n\
         Steps:\n\
         1. Run the production build for BOTH frontend and backend (e.g. \n\
            `npm run build`, `cargo build --release`). Fix any errors.\n\
         2. Identify the simplest FREE deployment target for this stack:\n\
            - Frontend SPA/static → Vercel / Netlify / Cloudflare Pages (all free tier)\n\
            - Backend API → Render / Railway / Fly.io free tier, or serverless\n\
            - Full-stack monorepo → Vercel (frontend) + Render (backend)\n\
         3. List every required environment variable (DB URL, API keys, auth secrets)\n\
            as `KEY=<description>` — never real values.\n\
         4. Write the exact deploy commands (e.g. `npx vercel --prod`, CLI login steps).\n\
         5. Confirm the build output dir exists and is non-empty.\n\n\
         After verifying, write `output/{slug}-delivery-notes.md` with:\n\
         - Build status (frontend + backend, both green?)\n\
         - Under a `## Deploy target` heading: the recommended platform + why\n\
         - Under a `## Deploy command` heading: the EXACT command to deploy \n\
           (e.g. `npx vercel --prod`) — UmaDev reads this for `/deploy`\n\
         - Under a `## Frontend URL` heading: the live URL AFTER a successful \n\
           deploy, or `(not yet deployed)` if undeployed\n\
         - Under a `## Environment variables` heading: every required var as \n\
           `KEY=<description>` (never real secrets)\n\
         - Under a `## Run command` heading: how to run the production build locally"
    );
    let user = format!(
        "## Requirement\n\n{requirement}\n\n\
         ## Architecture (deploy per this)\n\n{arch_excerpt}\n\n\
         Produce the deployment recipe now."
    );
    Prompt { system, user }
}

/// Truncate `text` to at most `max_chars` characters, keeping head.
/// Returns text with a trailing `…` marker when it had to cut.
#[must_use]
pub fn excerpt(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut buf: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    buf.push('…');
    buf
}

/// Section-aware excerpt: when `text` overflows `max_chars`, keep WHOLE markdown
/// sections (split on `#` heading lines) within the budget, prioritising the
/// contract-critical ones (API surface, design tokens, acceptance criteria) so
/// they survive instead of being cut mid-table by a blind char-count truncation.
/// Falls back to [`excerpt`] when the text has no heading structure. This is the
/// context-engineering floor: a later phase sees the binding contract, not an
/// arbitrary prefix of the document.
#[must_use]
pub fn excerpt_sections(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    // Split at heading lines; the preamble before the first heading is its own
    // section so a doc that opens with prose keeps its intro.
    let mut sections: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        if line.starts_with('#') && !cur.is_empty() {
            sections.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.is_empty() {
        sections.push(cur);
    }
    if sections.len() <= 1 {
        return excerpt(text, max_chars); // no structure to preserve
    }
    const KEYS: &[&str] = &[
        "api",
        "endpoint",
        "route",
        "schema",
        "contract",
        "token",
        "design",
        "color",
        "colour",
        "typograph",
        "font",
        "icon",
        "component",
        "acceptance",
        "criteria",
        "stack",
        "data model",
        "interface",
    ];
    let is_critical = |s: &str| {
        let head = s.lines().next().unwrap_or("").to_lowercase();
        head.starts_with('#') && KEYS.iter().any(|k| head.contains(k))
    };
    // Select critical sections first (so they win the budget), but emit in
    // document order so the result still reads top-to-bottom.
    let mut order: Vec<usize> = (0..sections.len()).collect();
    order.sort_by_key(|&i| !is_critical(&sections[i]));
    let mut take = vec![false; sections.len()];
    let mut trunc: Option<(usize, String)> = None;
    let mut used = 0usize;
    for i in order {
        let n = sections[i].chars().count();
        if used + n <= max_chars {
            take[i] = true;
            used += n;
        } else if trunc.is_none()
            && is_critical(&sections[i])
            && max_chars.saturating_sub(used) > 240
        {
            // A critical section can't fit WHOLE — keep a truncated copy rather
            // than drop it (a cut API table beats none), then stop filling.
            trunc = Some((i, excerpt(&sections[i], max_chars - used)));
            used = max_chars;
        }
    }
    let mut kept = String::new();
    for (i, s) in sections.iter().enumerate() {
        if take[i] {
            kept.push_str(s);
        } else if let Some((ti, t)) = &trunc {
            if *ti == i {
                kept.push_str(t);
            }
        }
    }
    if kept.is_empty() {
        return excerpt(text, max_chars); // even the smallest section overflowed
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_sections_keeps_critical_section_over_arbitrary_prefix() {
        let doc = format!(
            "# Title\n{}\n\n## Overview\n{}\n\n## API\nGET /users -> 200\nPOST /login -> token\n",
            "x".repeat(200),
            "y".repeat(2000),
        );
        let out = excerpt_sections(&doc, 400);
        assert!(out.contains("## API"), "critical API section must survive");
        assert!(out.contains("/login"), "the API body is kept whole");
        // A blind char-count excerpt never reaches the late API section.
        assert!(!excerpt(&doc, 400).contains("## API"));
    }

    #[test]
    fn excerpt_sections_truncates_oversized_critical_not_drops() {
        // A critical ## API section bigger than the whole budget must SURVIVE
        // (truncated), not be dropped while smaller prose fills the budget.
        let doc = format!(
            "# Title\n\n## Overview\n{}\n\n## API\n{}\n",
            "y".repeat(300),
            "GET /a\n".repeat(400),
        );
        let out = excerpt_sections(&doc, 800);
        assert!(
            out.contains("## API"),
            "oversized critical section kept truncated"
        );
        assert!(out.contains("GET /a"), "some API body survives");
    }

    #[test]
    fn excerpt_sections_falls_back_when_no_headings() {
        let doc = "a".repeat(1000);
        let out = excerpt_sections(&doc, 100);
        assert!(out.chars().count() <= 100);
    }

    #[test]
    fn research_prompt_mentions_required_sections() {
        let p = research_prompt("demo", "build a login system", "- knowledge/x.md");
        assert!(p.system.contains("Similar products"));
        assert!(p.system.contains("Domain risks"));
        assert!(p.user.contains("build a login system"));
        assert!(p.user.contains("knowledge/x.md"));
    }

    #[test]
    fn prd_prompt_quotes_research() {
        let p = prd_prompt("demo", "x", "excerpt: research goes here");
        assert!(p.user.contains("excerpt: research goes here"));
    }

    #[test]
    fn architecture_prompt_demands_api_table() {
        let p = architecture_prompt("demo", "x", "");
        assert!(p.system.to_lowercase().contains("api surface"));
        assert!(p.system.contains("|"));
    }

    #[test]
    fn uiux_prompt_locks_icon_library() {
        let p = uiux_prompt("demo", "x", "");
        assert!(p.system.contains("Lucide"));
        assert!(p.system.contains("Heroicons"));
        assert!(p.system.contains("Tabler"));
    }

    #[test]
    fn every_prompt_carries_spec_preamble() {
        for p in [
            research_prompt("s", "r", "k"),
            prd_prompt("s", "r", "x"),
            architecture_prompt("s", "r", "x"),
            uiux_prompt("s", "r", "x"),
        ] {
            assert!(p.system.contains("UmaDev pipeline"));
            assert!(p.system.contains("Scale your output"));
            assert!(p.system.contains("icon library"));
        }
    }

    #[test]
    fn into_request_round_trip() {
        let req = research_prompt("s", "r", "k").into_request("claude-sonnet-4-6", 4096);
        assert_eq!(req.model, "claude-sonnet-4-6");
        assert_eq!(req.max_tokens, Some(4096));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert!(req.system.is_some());
    }

    #[test]
    fn excerpt_trims_long_text() {
        let s: String = "a".repeat(5000);
        let e = excerpt(&s, 100);
        assert_eq!(e.chars().count(), 100);
        assert!(e.ends_with('…'));
    }

    #[test]
    fn excerpt_passes_short_text_through() {
        assert_eq!(excerpt("hi", 100), "hi");
    }

    #[test]
    fn delivery_prompt_instructs_deploy_and_free_platforms() {
        let p = delivery_prompt(
            "demo",
            "做一个登录系统",
            "## API
POST /login",
        );
        // Must name free platforms.
        assert!(p.system.contains("Vercel") || p.system.contains("Netlify"));
        // Must demand the deploy/URL/run sections the TUI reads.
        assert!(p.system.contains("## Deploy command"));
        assert!(p.system.contains("## Frontend URL"));
        assert!(p.system.contains("## Run command"));
        // Must forbid real secrets.
        assert!(p.system.contains("never real") || p.system.contains("never real secrets"));
        assert!(p.user.contains("deployment recipe"));
    }
}
