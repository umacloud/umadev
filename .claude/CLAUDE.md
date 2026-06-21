<!-- BEGIN UMADEV CLAUDE -->
# UmaDev Claude Code Integration

This project uses a pipeline-driven development model.

## Positioning
- UmaDev does not own a model endpoint.
- Claude Code remains the execution host for coding capability.
- UmaDev provides governance: protocol, gates, and audit artifacts.

## Runtime Contract
- Treat UmaDev as the local Python workflow tool plus Claude Code `CLAUDE.md + Skills` integration.
- Primary surfaces are project-root `CLAUDE.md`, compatibility mirror `.claude/CLAUDE.md`, project-level `.claude/skills/umadev/`, and user-level `~/.claude/skills/umadev/`.
- Compatibility surface `.claude/commands/umadev.md` remains installed so older Claude Code builds still converge onto the same UmaDev workflow.
- Optional repo enhancement surfaces `.claude-plugin/marketplace.json` and `plugins/umadev-claude/.claude-plugin/plugin.json` can expose a richer Claude-native plugin layer without replacing the base `CLAUDE.md + Skills` contract.
- When the user triggers `/umadev`, `umadev:`, or `umadev：`, enter the UmaDev pipeline immediately rather than handling it like casual chat.
- Use Claude Code browse/search for research and Claude Code terminal/editing for implementation.
- Use local `umadev` commands whenever you need to generate/update docs, spec artifacts, quality reports, and delivery outputs.

## First-Response Contract
- On the first reply after a host-supported UmaDev entry (for example `/umadev ...`, `$umadev`, `umadev: ...`, `umadev：...`, `/umadev-seeai ...`, `$umadev-seeai`, `umadev-seeai: ...`, or `umadev-seeai：...`), explicitly state that the matching UmaDev mode is now active rather than normal chat mode.
- If the repository already contains `umadev.yaml`, `.umadev/WORKFLOW.md`, `output/*`, `.umadev/review-state/*`, or an unfinished run state, the first natural-language requirement in a new host session must also default to continuing UmaDev rather than plain chat.
- Before the first reply, read `.umadev/WORKFLOW.md` and `output/*-bootstrap.md` when present, and treat them as the explicit bootstrap contract for this repository.
- The first reply must explicitly state that the current phase is `research`, and that you will read `knowledge/` plus `output/knowledge-cache/*-knowledge-bundle.json` first when available before similar-product research.
- In standard mode, the next sequence is research -> three core documents -> wait for user confirmation -> Spec / tasks -> frontend first with runtime verification -> backend / tests / delivery.
- In SEEAI mode, the next sequence is research -> compact competition docs -> wait for user confirmation -> compact Spec -> full-stack sprint -> polish / handoff.
- Both modes must explicitly promise that they will stop after the three core documents and wait for approval before creating Spec or writing code.

## Local Knowledge Contract
- Read relevant files under `knowledge/` before drafting PRD, architecture, and UIUX.
- If `output/knowledge-cache/*-knowledge-bundle.json` exists, read it first and inherit its local knowledge hits into later stages.
- Treat matched local standards, scenario packs, and checklists as hard constraints, not optional hints.

## Conversation Continuity Contract
- If `.umadev/SESSION_BRIEF.md` exists, read it before responding and treat it as the active workflow state.
- If the workflow is waiting for docs confirmation, preview confirmation, UI revision, architecture revision, or quality revision, then user replies like `修改`, `补充`, `继续改`, `确认`, `通过`, `继续`, or detailed feedback remain inside the current UmaDev stage.
- After each requested revision inside a gate, stay in the same stage, update the required artifacts, summarize what changed, and wait again for explicit confirmation.
- Do not silently exit UmaDev mode because the user asked for several edits, follow-up questions, or extra constraints.
- Only leave the current UmaDev workflow if the user explicitly says to cancel the workflow, restart from scratch, or switch back to normal chat.

## Before coding
1. If Claude Code browse/search is available, research similar products first and write output/*-research.md as a real repository file
2. Read output/*-prd.md
3. Read output/*-architecture.md
4. Read output/*-uiux.md
5. Summarize the three core documents to the user and wait for explicit confirmation before creating Spec or coding
6. Chat-only summaries do not count as completion; the required artifacts must exist in the workspace
7. Read output/*-execution-plan.md
8. Follow .umadev/changes/*/tasks.md after confirmation, with frontend-first implementation and runtime verification

9. If the user requests a UI redesign or says the UI is unsatisfactory, first update `output/*-uiux.md`, then redo the frontend, and rerun frontend runtime + UI review before continuing.

## Output Quality
- Keep security/performance constraints from red-team report.
- Ensure quality gate threshold is met before merge.
- UI must follow output/*-uiux.md and avoid AI-looking templates (purple gradient, emoji icons, default-font-only).
- Before any UI implementation, lock the icon library, typography, design token system, component ecosystem, and page skeleton from output/*-uiux.md.
- Do not use emoji as functional icons or placeholders.
- For non-conversational AI products, avoid Claude / ChatGPT-style shells unless the UI plan explicitly justifies them.
- UI implementation must define typography system, design tokens, page hierarchy and component states before polishing visuals.
- Prioritize real screenshots, trust modules, proof points and task flows over decorative hero sections.

## Coding Constraints (active during ALL coding phases)

These rules apply every time you write or edit a file. They are NOT suggestions:

### Tech Stack Pre-Research
- Before writing ANY code, run `cat package.json` (or equivalent) to check framework versions.
- If unsure about an API for the installed version, use WebFetch to read official docs first.
- Never guess API signatures. Check docs.

### Icon & Visual Rules
- Icons MUST come from a declared icon library (Lucide/Heroicons/Tabler). No emoji as icons.
- No purple/pink gradient themes. No default system font only.
- Before showing any UI code, self-check: no emoji characters in the source.

### Frontend/Backend Alignment
- Frontend fetch URLs must exactly match backend route definitions.
- Define API paths as shared constants when possible.

### Per-File Self-Check
- Before writing each file: correct imports, no emoji, colors from tokens only.
- After completing a feature, run build + lint. Fix errors before moving on.

### CLI Commands During Coding
- Run `umadev enforce validate` after writing UI code.
- Run `umadev quality` after completing a feature.
- Run `umadev review --state ui` after frontend is done.
- Run `umadev release proof-pack` before final delivery.

## Four-Layer Governance Model

UmaDev governance operates at four layers:

**Layer 1 — CLAUDE.md (Persistent Rules)**
Project-root `CLAUDE.md` is the canonical persistent memory surface. `.claude/CLAUDE.md` is kept as a compatibility mirror for builds that still read nested memory files.

**Layer 2 — Skills (Primary Execution Contract)**
Project-level `.claude/skills/umadev/` and user-level `~/.claude/skills/umadev/` carry the primary UmaDev execution contract. Claude Code only uses `umadev` as the single skill name — no `umadev-core` alias.

**Layer 3 — Hooks (Runtime Enforcement)**
PreToolUse hooks validate every file write. PostToolUse hooks audit results.
Hooks are auto-registered when /umadev is invoked.

**Layer 4 — CLI Commands & Optional Plugin Enhancement (On-Demand Checks)**
Run `umadev enforce validate` / `umadev quality` for deeper checks.
These are triggered at key milestones, not every turn.
If Claude Code surfaces repo plugins, `.claude-plugin/marketplace.json` + `plugins/umadev-claude/.claude-plugin/plugin.json` should enhance the same UmaDev flow rather than fork it.

## UmaDev System Flow Contract
- UMADEV_FLOW_CONTRACT_V1
- PHASE_CHAIN: research>docs>docs_confirm>spec>frontend>preview_confirm>backend>quality>delivery
- DOC_CONFIRM_GATE: required
- PREVIEW_CONFIRM_GATE: required
- HOST_PARITY: required
<!-- END UMADEV CLAUDE -->





