# UmaDev — Product Vision & Roadmap to Commercial Launch

> **Status:** authoritative. This document supersedes scattered direction notes and
> the aspirational `AGENT_WIELDS_BASE_ARCHITECTURE.md` migration-wave language.
> It defines the target "real director Agent," an honest gap analysis against the
> current code, and an executable, impact-ordered roadmap to launch.
>
> **Non-negotiable identity:** UmaDev is **firmware over a borrowed brain**. It owns
> **no model**, brokers **no endpoint**, and does **not** re-implement the base's
> agentic loop. It borrows the base CLI's brain to **THINK** (route, plan, judge,
> adapt) and directs the base's body to **WORK** (write code, run, fix). Every
> capability below is built as *deterministic Rust orchestration + injected prompt
> firmware + structured artifacts UmaDev owns* — never as cognition UmaDev performs
> itself.

---

## 0. The one-sentence problem

The product the README/spec **describe** (a director that routes, plans, decomposes,
schedules a team, delivers a proof-pack, and learns) is **not the product that ships
by default**. The default path (`drive_director_loop`) is *one base build turn + ≤3
rounds of read-only QC*, fed by a *static* directive, with **no routing, no visible
plan, no team scheduling, no learned-memory injection, and no codebase-context
engine**. All of that intelligence exists in the tree but is either stranded in the
env-gated legacy pipeline (`UMADEV_LEGACY_PIPELINE=1`), reduced to a non-binding
prompt hint, or dead code (`director::summon`). The launch work is **composition and
wiring of assets that already exist** plus three new owned primitives (Router, Plan,
visible event surface) — not greenfield invention.

---

## 1. Target Architecture — the "real director Agent"

UmaDev is one engine with five layers. The base brain is consulted at **explicit
decision points** to produce **typed artifacts UmaDev owns**; UmaDev then drives the
base body deterministically against those artifacts. Everything is **fail-open**: any
brain consult that fails or returns garbage falls back to a deterministic floor and
never blocks the host.

```
                         ┌─────────────────────────────────────────────┐
   user turn  ─────────► │  L1  INTENT ROUTER                          │
   (chat / /run /        │  classify → RoutePlan{class,kind,depth,team, │
    ad-hoc task)         │            scope,needs_clarify,est_budget}   │
                         └───────────────┬─────────────────────────────┘
                                         │ RoutePlan
              ┌──────────────────────────┼───────────────────────────────┐
              ▼                          ▼                                ▼
   ┌───────────────┐         ┌───────────────────────┐        ┌────────────────────┐
   │  FAST PATH    │         │  DELIBERATE PATH       │        │  CLARIFY           │
   │  chat / 1-2   │         │  L2 PLAN → L3 DRIVE     │        │  one batched, MCQ  │
   │  line edit    │         │  → L4 VERIFY → L5 LEARN │        │  question, then    │
   │  (no run-lock)│         │  (run-lock, gates)      │        │  re-route          │
   └───────────────┘         └───────────────────────┘        └────────────────────┘
              │                          │
              └──────────┬───────────────┘
                         ▼
        L0  FIRMWARE INJECTION (every base session, every path):
            identity + craft/anti-slop + JIT knowledge + learned-lessons digest
            + repo-map slice + project-action ledger  (token-budgeted, curated)
```

### L0 — Firmware injection (the borrowed-brain conditioning layer)

The single boundary where UmaDev conditions the base session. **Today this is
asymmetric and thin on the live path** — `session_for` injects *no* system prompt;
the only firmware is a static `director_build_directive` string. Target: **one
`umadev_agent::context::compose_firmware(root, route, requirement)`** that *every*
path (chat, fast, deliberate) calls to build a curated, token-budgeted system prompt:

- **Identity** — senior director + the seat the current step needs (`experts::agentic_team_identity`).
- **Craft / anti-slop law** — `experts::ANTI_SLOP_LAW` + `agentic_engineering_rules` (already strong; just always-on).
- **JIT knowledge** — `umadev_knowledge::retrieve` keyed on *requirement + stack + current step* (BM25↔vector + HyDE/RRF), returned as a small digest, **not** the whole corpus. Curated to a budget, not front-loaded.
- **Learned-lessons digest** — `lessons::lessons_for_error` / recurring-pitfall recall keyed on the detected stack fingerprint (today this fires *only* in `runner.rs`).
- **Repo-map slice** — a token-budgeted, intent-personalized signature outline of the user's code (new; see L1/Wave 3).
- **Project-action ledger digest** — a compact "what I've done this session" from an owned JSONL (new), so the agent can answer "what did you just do" from ground truth, not the base's fuzzy recall.

> **Governance contract is preserved:** firmware is injected into the base's *own*
> native system-prompt / settings surface (each of the three bases has one). UmaDev
> declares policy; the base executes. We never own cognition.

**Token economy** (the discipline that prevents "context rot"): identity + active-step
craft + governance are always-on and *small*; knowledge and repo-map are *JIT-retrieved
and budgeted*; prior-phase artifacts are *compacted to decisions, not transcripts*.

### L1 — Intelligent Router (`umadev_agent::router`, NEW)

The missing brain. Replaces the two current "classifiers" — `looks_like_work_request`
(a substring toggle that only gates injection) and `planner::classify` (a keyword
table reachable only via `/run`). Produces one typed `RoutePlan`:

```rust
pub struct RoutePlan {
    pub class: RouteClass,        // Chat | Explain | QuickEdit | Debug | Build
    pub kind: TaskKind,           // Greenfield | FrontendOnly | Bugfix | Refactor | ...
    pub depth: Depth,             // Fast | Standard | Deep
    pub team: Vec<Seat>,          // who to convene (doers serial, critics parallel)
    pub scope: Vec<PathHint>,     // likely-relevant files (feeds repo-map + retrieval)
    pub needs_clarify: Option<ClarifyQuestion>,
    pub est_budget: Budget,       // rough tool-calls / token ceiling for this turn
    pub confidence: f32,
}
```

**Two-tier, fail-open:**
- **Tier-0 (deterministic, zero latency):** `planner::classify` + `looks_like_work_request` catch the obvious (greeting → Chat; "改个文案" → QuickEdit). This is the floor and the fallback.
- **Tier-1 (brain-assisted):** for anything ambiguous, a cheap **structured-JSON consult on a `fork()`ed read-only session** (modeled exactly on the existing `surface_intake_plan`, which already proves the pattern works) returns `{class, kind, complexity, needs, scope, risks, confidence}`. Reconciled with the Tier-0 prior: **brain wins, but never drops below the deterministic safe floor** (it may escalate depth/team, never silently de-scope below what keywords flagged dangerous).

**Decision points:**
- `class == Chat | Explain` → **Fast path**, no run-lock, light firmware.
- `class == QuickEdit | Debug` and `depth == Fast` → fast single-writer turn + targeted verify.
- `class == Build` or `depth >= Standard` → **Deliberate path** (L2→L5), run-lock, gates, team.
- `needs_clarify.is_some()` → emit **one** batched multiple-choice question via the existing `ClarifyGate`, then re-route. Hard rule injected: *never ask what you can discover by reading the code.*

**The decision is surfaced** (`EngineEvent::IntentDecided`) so the user *sees* the route: "这是一个小修改，直接做" vs "这是完整产品，进入研发流程 (~20 min, will create files under src/)." Routing is legible, and the user can override (`/run` forces Deep, `/quick` forces Fast).

### L2 — Planning & Scheduling (`umadev_agent::plan_state`, NEW)

The keystone of "feels like a real agent." Today there is **no `Plan` data structure**;
the plan lives invisibly in the base's head and the 9-dot phase bar sits frozen at
`0/9` during a director run.

- **Plan synthesis:** before the build loop, one **forked planning turn** asks the brain to emit a strict-JSON plan UmaDev parses and **owns**:
  ```rust
  pub struct Plan { pub steps: Vec<PlanStep>, pub risks: Vec<String>, pub open_questions: Vec<String> }
  pub struct PlanStep { pub id, pub title, pub seat: Seat, pub kind: StepKind /*Build|Review*/,
                        pub depends_on: Vec<StepId>, pub acceptance: AcceptanceSpec, pub status }
  ```
  This is a **dependency DAG**, not a flat list — independent nodes are parallelizable, and `acceptance` makes each step's "done" mechanical, not vibes-based.
- **Visible & steerable:** persisted to `.umadev/plan.json`; rendered live in the TUI as a checklist that ticks off (`[x] scaffold · [~] auth route · [ ] login form  3/8`). A `/plan` command lets the user **reorder / skip / add / veto** a step, folded back into the next directive over the same session. This converts gate-only intervention into step-level control.
- **Scheduling = single-writer + map-reduce-manage** (the Devin/Anthropic doctrine, to be locked as a spec clause): **doing-roles drive the main session serially** under the run-lock (one writer — actions carry implicit decisions that conflict if parallelized); **reviewing critics run on parallel `fork()`ed read-only sessions** and return bounded `RoleVerdict`s. Decomposable doing-work (per-module, per-route) may fan out to **isolated forked sub-sessions** that return only a structured summary; writes still serialize through the one main session. The team scales with `RoutePlan.depth/team`, on **every** path — not just `/run`.

### L3 — Director Intelligence: drive, adapt, self-correct (`director_loop` rebuilt)

The loop changes from "one mega-turn + generic QC" to **plan-driven, step-by-step,
evidence-grounded**:

1. **Drive** each ready step (deps satisfied) as a `summon(Serial)` directive — finally using the dead `director::summon` lever.
2. **Verify against that step's `acceptance`** using the **deterministic floor only** (coverage / contract / runtime-proof / build-test). Critic verdicts stay **advisory**; the floor is the only thing that flips a step to done or forces rework. Hard rule in firmware: *an open loop that declares done without a green gate is a defect.*
3. **Self-correct with diagnosis, not "go fix it":** tag each blocking finding with a class (build / contract / coverage / behavior / craft), attach the matching `error_kb` playbook + recalled `lessons`, and fold a **concrete** rework directive (with raw failing-test/stderr evidence) back into the session.
4. **Adapt / escalate** with a typed blocker disposition per finding — `Investigate | FixAdjacent | NoteAndContinue | Escalate`. Only `Escalate` consumes the bounded gap/stall counter; `NoteAndContinue` records to `lessons` without burning a rework round. On a *recurring* identical finding (stuck-detector over the event log: same error/directive fingerprint N times), **change strategy** (reflect, narrow to the failing file, split the step) or exit cleanly as `BLOCKED{reason, evidence}` — never spin.
5. **Decide when to involve the user:** clarify only at L1 (intent) and at the two confirm gates; otherwise drive autonomously within the trust tier. Irreversible/out-of-scope actions always hit the floor.

**Context survival across a long build:** a **compaction module** clears consumed
tool outputs first, then (if still over budget) summarizes into a fixed template
(*Primary Intent · Files & Code · Errors & Fixes · Pending Tasks · Current Work · Next
Step*), reopening the session seeded with the summary + most-recent artifacts. Plan
progress + a `PROGRESS.md`-style state file are re-read on every step entry, so the
session survives compaction and process restart.

### L4 — Mature functions & verification (deterministic floor, on by default)

- **Acceptance gate is real and required for heavyweight kinds:** `coverage` (FR→step) + `acceptance` (task→API) + `umadev-contract` validation + `verify --runtime` (boot + route probe → `runtime-proof.json`) become **required signals on the default deliberate path**, not legacy-only. For bugfixes, require a **failing reproduction test that actually reproduces the issue**, gated on red→green plus regression staying green.
- **Delivery artifacts restored on the default path:** `output/*-prd.md`, `-architecture.md`, `-uiux.md`, scorecard HTML, and the zipped **proof-pack** — lifted out of `phases.rs` into a `director::finalize()` that runs once QC is clean (gated by depth, so a todo page doesn't get a proof-pack).
- **Owned baseline SAST** so `security` produces findings tool-free (extend the `rules` engine with injection / missing-auth / hardcoded-secret heuristics); gitleaks/semgrep remain optional upgrades.
- **Git as the trust substrate:** auto-commit each accepted step on a derived `umadev/<task>` branch (attributed `umadev via <base-id>`), never the user's working/default branch, **never auto-merge**; `rollback` = real git revert; per-turn checkpoints decoupled from the user's `.git`.

### L5 — Memory & learning (active on the live path)

- **Episodic vs semantic split, consolidate on a separate reflection pass** (don't summarize at write-time): keep raw, stack-fingerprinted pitfall episodes; a periodic reflection scores by recency × relevance × salience and writes **semantic rules** + **procedural skills** without deleting episodes.
- **Correction-as-documentation, re-injected as firmware:** on a true recurrence, a `Reflection` compiles a concrete prevention rule keyed by stack fingerprint into the **next run's L0 injection** — so the same class of mistake is *prevented*, not just logged. Optionally auto-promote a durable reflection into a committed `AGENTS.md`/`.umadev/rules/` artifact (shareable, diff-reviewable).
- **All capture sites wired into `director_loop`** (`capture_dev_errors` / `capture_validated_patterns` / `record_tool_call` / `record_usage`) so `/lessons` and `/usage` and the audit trail are real on the default path — for all three bases, not just claude.

### Conversation as a first-class surface

Chat is the everyday surface and is currently a goldfish: the `conversation` buffer
is built, trimmed to 16, and **never sent** to the brain (memory is delegated entirely
to the base's `--resume`); restart = amnesia; offline chat returns empty; chat and
`/run` use **disjoint** sessions. Target:

- **Send UmaDev's own bounded transcript every turn** (base `--resume` becomes belt-and-suspenders, not the only memory).
- **Persist + resume chat sessions per project** (`.umadev/chat/<id>.json`), with `/sessions` and `/resume`.
- **Unify chat ↔ `/run` memory:** hand the finished director session back to chat so "what did you just build?" continues the *same* session that did the build.
- **Context management:** token-budgeted summarize-and-fold (a `/compact` verb) instead of FIFO-drop-at-16.
- **Offline/external brain:** a context-aware canned reply offline; an `External`/HTTP arm so chat has a brain even with no base CLI — still never owning cognition, just refusing to return silence.

---

## 2. Honest Gap Analysis — current vs target

Ranked by impact on "not a real agent / not launchable."

| # | Gap | Current reality (file evidence) | Target |
|---|-----|--------------------------------|--------|
| **G1** | **No router on the live surface.** Every non-slash message is one undifferentiated base turn; `/run` planner is keyword-only and walled off. | `app.rs:2439`, `lib.rs:2183`, `director_build:false` hardcoded `lib.rs:1479`; `planner::classify` substring table `planner.rs:288`. | L1 Router; auto fast-vs-deliberate; visible route. |
| **G2** | **No plan UmaDev owns; phase bar frozen 0/9.** Plan lives in the base's head; only `Note`+`WorkerStream` emitted. | `director_loop.rs:237,261-308`; `experts.rs:804` ("no fixed phase checklist"); `app.rs:8166` ("the 0/9 window"). | L2 `Plan` DAG; live, steerable `PlanUpdated` surface. |
| **G3** | **Firmware is thin/asymmetric on the default path.** Default session gets no system prompt + a static directive; knowledge/lessons/repo-map never reach it. | `host/lib.rs:1205-1219` ("we append no extra system prompt"); lessons calls only in `runner.rs`. | L0 `compose_firmware` on every path. |
| **G4** | **No codebase-context engine on the live path.** No repo-map/symbol graph; `adopt` is manual + legacy-only; brownfield/explain/navigate add nothing over the bare base. | no tree-sitter/symbol dep; `brownfield_*` wired only into `runner.rs:2966,2991`. | L0 repo-map slice + auto-adopt + JIT retrieval everywhere. |
| **G5** | **Learned memory is dark in the shipping path.** Sophisticated lessons/reflection/error_kb fire only in legacy. | `lessons.rs` (5.8k LOC) called only from `runner.rs`. | L5 wired into `director_loop`. |
| **G6** | **The "team" is invisible and post-hoc, not a driven workflow.** `summon` is dead; critics review *after* the base built solo; verdicts stranded as bland `Note`s. | `director.rs:14-15` (admits summon unused, task #22); `director_loop.rs:494`. | L2 scheduling + `EngineEvent::CriticVerdict` panel. |
| **G7** | **Self-correction is shallow & generic.** Catches only no-source / emoji-color / build-fail; responds with one undifferentiated "go fix it"; re-tries identically. | `director_loop.rs:451-466,489,530`; acceptance/coverage legacy-only. | L3 typed disposition + diagnosed correction + stuck-detector. |
| **G8** | **Delivery artifacts gone by default.** No PRD/architecture/scorecard/proof-pack — only source files. | artifact writers stranded in `phases.rs:2129-2269`. | L4 `director::finalize()`. |
| **G9** | **`continue`/`revise`/`redo` don't compose with the default engine.** They resume on gates the director path never emits; `continue` opens a cold session. | `main.rs` `cmd_continue` cold-session; gates legacy-only. | Unified engine; persist + resume the same base session. |
| **G10** | **First-run "ready" mark is a lie.** Picker probes `--version` (PATH), not auth; a not-logged-in base shows green and fails mid-run. | `claude.rs:704`, `host/lib.rs:1274`, `doctor.rs:238`. | Real auth probe in picker (Wave 1 quick win). |
| **G11** | **Chat memory thrown away; restart = amnesia; offline silent; chat↔run split.** | `lib.rs:1352` (transcript not sent), `app.rs:593-595,683-700`, `runtime/lib.rs:312`, `lib.rs:1877-1883`. | First-class conversation surface. |
| **G12** | **Spec/docs/code three-way drift.** Spec normative chain = 9 phases; architecture doc says "no fixed chain"; `CLAUDE.md` says "9-phase runner is core." | `spec:894`, `AGENT_WIELDS_BASE_ARCHITECTURE.md:49,133`. | One canonical truth (Wave 6). |
| **G13** | **Governance asymmetric across bases.** Real-time PreToolUse hook is claude-only; audit/usage empty on a default codex/opencode run. | `hook.rs:3`. | Per-turn governance scan for all bases; honest in-product disclosure. |

**The throughline:** UmaDev's *real, working asset today is the firmware* (craft taste +
honesty floor + curated knowledge) — a genuine reason to prefer it over the bare base,
but it is thin and **invisible**, and everything *above* the firmware (routing, plan,
team, delivery, memory) is dead code, legacy-only, or aspirational. The launch gap is
**wiring + visibility + three owned primitives**, not invention.

---

## 3. Roadmap to Commercial Launch — impact-ordered Waves

Each wave is a coherent shippable increment. **TS** = table-stakes (a launch can't ship
without it). **DIFF** = differentiating (the reason a user picks UmaDev). Every brain
consult is fail-open to a deterministic floor; no wave makes a governance function
fail-closed; no wave adds a model endpoint or re-implements the base loop.

---

### WAVE 1 — "It thinks, plans, and shows it." *(the user's sharpest pain)*  — TS + DIFF
**Goal:** the default surface routes intelligently, owns a visible/steerable plan, and
*looks and behaves* like a director. This single wave converts "feels like the bare
base, slower" into "a director ran my project."

**Deliverables**
1. **Intent Router** — `umadev-agent/src/router.rs` (NEW): `RoutePlan`, Tier-0 deterministic + Tier-1 forked-JSON consult (clone `surface_intake_plan`'s pattern), reconciliation with safe floor. Wire into the **one bypassed entry point**: `umadev-tui/src/app.rs:2439-2453` / `lib.rs:2183-2193`, and make `AgenticTurn` (`lib.rs:755`) carry `route: RoutePlan`, dropping the hardcoded `director_build:false` (`lib.rs:1479`) so chat can auto-promote into a build.
2. **Owned Plan** — `umadev-agent/src/plan_state.rs` (NEW): `Plan`/`PlanStep` DAG; a forked planning turn before the build loop in `director_loop.rs:261`; persist `.umadev/plan.json`; parse via the existing `extract_json_object`. Fail-open: unparseable → today's single-turn behavior.
3. **Visible surface** — `events.rs`: add `IntentDecided`, `PlanPosted`, `PlanStepStatus`/`PlanUpdated`, `CriticVerdict`. Emit from the router + plan loop + critic fan-out (replace the bland `Note` at `director_loop.rs:494`). Render in `umadev-tui/src/app.rs`/`ui.rs`: an intent pre-commitment card, a live plan checklist (replacing the frozen 0/9 bar on the director path), and a collapsible team-review panel.
4. **Step-level steering** — `/plan` command to reorder/skip/add/veto; fold edits into the next directive over the same session (reuse `queued_steer`).
5. **Auth probe in picker** (cheap, P0 trust fix) — `umadev-host`: add `auth_state` to `ProbeResult` (cheapest authenticated no-op per base); three picker states (`logged in` / `installed · not logged in → <login cmd>` / `not installed → <install cmd>`); block commit on not-authed.

**Touches:** `umadev-agent/{router,plan_state,events,director_loop}.rs`, `umadev-tui/{lib,app,ui}.rs`, `umadev-host/{lib,claude,codex,opencode}.rs`.

**User-visible outcome:** type "build me X" in chat → UmaDev says *"I'll BUILD this (~N min, files under src/) — here's my plan: [8 steps]. [c] go · [revise] · [chat]"*, then drives it step-by-step with a checklist ticking off and a visible team review — and the first-run picker tells the truth about login.

---

### WAVE 2 — "It actually directs a team, with real firmware." — DIFF
**Goal:** make the plan *driven* (not just shown) and deliver UmaDev's firmware on the
live path, so the depth that already exists in the tree is on by default.

**Deliverables**
1. **`compose_firmware`** — `umadev-agent/src/context.rs` (NEW/expanded): one builder injecting identity + craft + JIT knowledge digest + lessons digest, called by **all** paths. Apply it in `host::session_for` (`host/lib.rs:1205`) by accepting a system prompt for all three bases (claude `--append-system-prompt`/settings; codex/opencode native system text). Promote the rich TUI composition (`lib.rs:1336`) to the director caller.
2. **Drive the plan via `summon`** — wire `director::summon(Serial)` into `director_loop` for each Build step; `summon(Parallel)`/`review` for Review steps. Map-reduce-manage: decomposable steps fan to isolated forked sub-sessions returning structured summaries.
3. **Brain-assisted team sizing** — lift `*_team_for_kind` (`critics.rs:570`) out of `/run`-only; size the roster from `RoutePlan.team`/depth on every path.
4. **Wire learned memory + audit + usage into the default loop** — call `lessons::capture_*` / `record_tool_call` / `record_usage` from the `drive_one_turn` event pump (`director_loop.rs:365-378`); inject recalled pitfalls into the directive.

**Touches:** `umadev-agent/{context,director,director_loop,critics,lessons}.rs`, `umadev-host/src/lib.rs`.

**User-visible outcome:** `/lessons` and `/usage` are real; the team is an actual workflow (architect → backend → frontend, governed, serial) not a post-hoc critique; the same prompt now visibly carries UmaDev's craft + project knowledge + learned pitfalls into the base.

---

### WAVE 3 — "It understands your codebase." — TS (for brownfield) + DIFF
**Goal:** close the single biggest competitive gap — on a real repo, UmaDev currently
adds nothing over the bare base.

**Deliverables**
1. **Repo-map / symbol graph** — `umadev-knowledge` (NEW module): a dependency-light symbol scan (regex/ctags-style to honor the anti-rule against heavy parser trees; tree-sitter only if budget allows), ranked (PageRank-style) and **intent-personalized** by `RoutePlan.scope` + current step; rendered as a token-budgeted signature outline; cached by mtime in `.umadev/`.
2. **Auto-adopt + shared project context** — `umadev-agent/src/context.rs::project_context(root, query)`: auto-run a fast incremental index on first turn in a non-empty repo; promote `brownfield_code_snippets` out of `runner.rs:2991` into this shared helper consumed by L0 on every path.
3. **Incremental verify** — in `run_auto_qc`, prefer reading the base's *own* just-run build/test result over re-running the full suite; only re-run the specific failing step (kills the duplicate-build cost on heavy QC).

**Touches:** `umadev-knowledge/src/*`, `umadev-agent/{context,adopt,director_loop}.rs`.

**User-visible outcome:** "explain this code", "fix the bug in checkout", "add a field to the user model" all get a real repo-aware answer/edit; brownfield feature-add stops feeling like the bare CLI.

---

### WAVE 4 — "It delivers proof and verifies itself." — TS
**Goal:** restore commercial-grade delivery + make the acceptance gate real on the
default path.

**Deliverables**
1. **`director::finalize()`** — lift artifact writers from `phases.rs:2129-2269`; produce PRD/architecture/uiux/scorecard/proof-pack once QC is clean, gated by depth.
2. **Required acceptance floor for heavyweight kinds** — promote `coverage` + `acceptance` + `umadev-contract` + `verify --runtime` into the default deliberate path; for Bugfix, require a reproduction test (red→green) + green regression.
3. **Diagnosed, escalating self-correction** — typed blocker disposition (`Investigate|FixAdjacent|NoteAndContinue|Escalate`); `error_kb` playbook + `lessons` folded into the fix directive; stuck-detector over the event log → strategy change or `BLOCKED{reason,evidence}`.
4. **Owned baseline SAST** — extend `rules` with injection/missing-auth/secret heuristics so `security`/`report --review` find things tool-free.

**Touches:** `umadev-agent/{director,director_loop,acceptance,coverage,security,error_kb}.rs`, `umadev-contract`, `umadev-governance/src/rules.rs`.

**User-visible outcome:** a default `/run` again leaves a PRD, an architecture doc, a runnable proof, a scorecard, and a shareable proof-pack — and won't declare done without a green gate.

---

### WAVE 5 — "It remembers and you can converse with it." — TS
**Goal:** fix the conversation goldfish + unify engines for honest resume.

**Deliverables**
1. **Send UmaDev's transcript every turn** (`lib.rs:1352`); thread `conversation` into `AgenticTurn`. **Persist + resume per-project chat** (`.umadev/chat/<id>.json`); `/sessions`, `/resume`, `/compact` (token-budgeted summarize-and-fold replacing FIFO-16).
2. **Unify chat ↔ `/run` memory** — don't close+clear the held session on run finish (`lib.rs:1877-1883`); hand it back to chat as the active session.
3. **Engine unification & session-carrying `continue`** — `drive_director_loop` emits `GateOpened` + persists `workflow-state.json` (incl. `plan_steps`) at checkpoints so `continue`/`revise`/`redo` operate on the live engine; resume the **same** base session id (drivers already support `set_session_id`) instead of a cold one.
4. **Cross-session goal continuity** — on launch, if `.umadev/plan.json` has an unfinished plan, surface "resume goal X (step 3/7)?"; in `auto` tier, drive to completion.
5. **Offline/external chat reply** — context-aware offline reply (no more empty `String::new()`); add an `External`/HTTP arm to `build_brain` for a base-less chat brain.

**Touches:** `umadev-tui/{lib,app}.rs`, `umadev-agent/{state,director_loop}.rs`, `umadev/src/main.rs`, `umadev-runtime/src/lib.rs`.

**User-visible outcome:** reopen UmaDev and the conversation + goal are still there; ask "why did you do it that way?" after a build and it answers from the same session; `continue` actually continues.

---

### WAVE 6 — "Trustworthy, legible, and consistent." — TS (launch hardening)
**Goal:** the unglamorous commercial-grade + credibility layer.

**Deliverables**
1. **Graduated trust + recovery UX** — decompose `TrustMode` into per-capability auto-approve (read auto / write guarded / shell confirm / network allowlist) with the always-on irreversible floor + a circuit breaker; turn aborts into action cards (`Retry / Switch base / Open log / Run doctor / Show login cmd`); adaptive stall threshold (longer during installs/builds).
2. **Branch isolation + git substrate** — workspace-mutating runs operate on a derived `umadev/<task>` branch, never default, **never auto-merge**; per-turn checkpoints + real-git `rollback`.
3. **Governance parity disclosure** — run the content-governance scan after every base turn for codex/opencode; honestly surface real-time-hook parity.
4. **Spec/docs/code reconciliation** — make the director/USB model canonical; demote the 9-phase chain to "the deep-commercial-build play the director may choose"; rewrite README "how it works / deliverables / team"; finish/close tasks #22 (delete or wire dead `summon`/`checkpoint`/`director_build` branches) and #23; retire the env-gated legacy engine or make it the director's internal "Deep" tier.
5. **Cost/time expectation setting** + contextual discoverability nudges.

**Touches:** `umadev-agent/{trust,run_lock,pr,checkpoint}.rs`, `umadev-tui/*`, `umadev/src/hook.rs`, `spec/`, `docs/`, `README`.

**User-visible outcome:** users can watch, steer, stop, undo, and trust; the docs match the product; codex/opencode users know exactly what they get.

---

**Sequencing rationale:** W1 attacks the literal complaint (routing + visible plan + "feels real") and the worst first-run cliff. W2 makes the plan *driven* and delivers the firmware that justifies the product. W3 closes the biggest competitive gap (real repos). W4 restores table-stakes delivery + a gate that says no. W5 makes it remember. W6 hardens for trust and reconciles the story. W1–W3 are the launch-critical trio; without them a user on a real repo bounces back to their base CLI.

---

## 4. Competitive Positioning

### Why a user picks UmaDev

**vs the raw base CLI (claude-code / codex / opencode):** UmaDev is **firmware that makes
the base behave like a senior delivery team** — it *routes* the request, *plans* visibly,
*schedules a team* (serial doers + parallel reviewers), injects *curated engineering
craft + anti-AI-slop taste + your project's knowledge + learned pitfalls*, verifies
against a *deterministic acceptance floor* that can say no, and hands back a *proof-pack
+ scorecard*. The bare base is a brilliant generalist with no taste floor, no plan you
can steer, no team, no memory of your project, and no honesty gate. UmaDev keeps the
base's brain and subscription (no second bill, no second login) and adds the **director
layer the base lacks** — and it works across **all three** bases identically.

**vs Cursor:** Cursor's moat is its IDE + apply pipeline + index. UmaDev is **CLI/terminal-
native, base-agnostic, and governance-first** — it brings the multi-seat review,
anti-slop craft floor, proof-pack delivery, and self-evolving project memory that Cursor's
single-model agent doesn't. UmaDev borrows whatever brain the user already pays for
instead of locking them to one vendor's model.

**vs Devin:** Devin owns its model, VMs, and ACU economics. UmaDev is the **open,
local-first, bring-your-own-brain** director: the same single-writer/map-reduce-manage
doctrine and plan-before-spend checkpoint, but running on the user's existing base
subscription, on their machine, with their data, with a transparent audit trail — no
proprietary endpoint, no ACU meter.

### What must be true for that to hold (the launch bar)

1. **The default path must deliver the firmware** (W2) — today it barely beats the bare base. *Non-negotiable.*
2. **Routing + a visible, steerable plan must be real** (W1) — this is the felt difference between "agent" and "wrapper."
3. **It must understand the user's existing code** (W3) — most dev work is brownfield; without repo-awareness UmaDev loses every non-greenfield scenario.
4. **The acceptance gate must say no, and delivery must produce proof** (W4) — "commercial-grade" is verification + shareable artifacts, not vibes.
5. **It must remember across turns and sessions** (W5) — a goldfish never feels like a colleague.
6. **The first-run signal must be honest and the docs must match the product** (W1 auth probe, W6 reconciliation) — trust is lost permanently at the first lie.
7. **All of it stays fail-open firmware over a borrowed brain** — the moat is the *deterministic director shell + taste + memory + governance*, with the base as a swappable cognition source. The day UmaDev owns a model or forks the base loop, it loses its identity and its reason to exist.

> **The bet:** the 2026 differentiator is no longer price or raw model IQ — it's *the
> loop, the gates, the plan, the taste, the memory, and the transparency*. UmaDev's
> assets for all six already exist in the tree. Launch is the act of wiring them onto
> the path users actually hit, and making the director *visible*.
