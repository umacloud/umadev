# UmaDev Agent-Team Interaction — Design, Evidence & Enhancement Plan

**Status:** design note / research synthesis. Not normative spec (that stays in
`spec/UMADEV_HOST_SPEC_V1.md`). This document answers one question: *how should
the dev-team agents interact — standardized-document relay vs. direct
transmission — and what would make our design stronger?* It grounds the answer
in the 2024–2025 multi-agent literature and maps every recommendation onto what
the code already does.

---

## 0. TL;DR

- **Our current design is, almost line-for-line, the 2024–2025 state-of-the-art
  consensus.** Independently, Anthropic, Cognition, LangChain, MetaGPT and the
  academic failure literature converge on: **parallelize reads, serialize
  writes, centralize decision-making, and never relay work between agents as
  lossy prose.** UmaDev already does exactly this (blackboard + single-writer
  doers on one continuous session + parallel read-only forked critics + a
  deterministic director).
- **Standardized document vs. direct transmission is NOT either/or — it is a
  layering, and conflating the two layers is the classic trap:**
  - **Relay *decisions* as standardized, versioned, typed documents** (PRD,
    architecture, API contract, acceptance criteria, `RoleVerdict`). Durable,
    machine-verifiable, provenance-bearing, survives a context reset.
  - **Relay *context / reasoning trace* as the raw forked session** (the "why,"
    the tool outputs, the rejected options). A `fork()` is lossless transmission
    with zero telephone-game.
  - **Never summarize the trace into prose to hand between roles.** That is where
    provenance dies and errors cascade.
- The highest-leverage reliability findings in the field — **centralized
  orchestration, per-step verification, isolated worker context** — are all
  already in the codebase. The enhancements below are refinements, not a
  rewrite.

---

## 1. What UmaDev does today (precise characterization)

Read from `umadev-agent`: `director`, `director_loop`, `critics`, `plan_state`,
`acceptance`/`coverage`, `context`.

1. **Single source of truth = a blackboard.** Roles communicate ONLY through the
   shared `output/*.md` artifacts + the source tree, plus their verdicts. Roles
   never chat to each other. (`critics.rs`: "roles communicate only through the
   shared blackboard and their verdicts.")
2. **Typed artifact bundle for reviewers.** `CriticArtifacts { requirement, prd,
   architecture, uiux, code, qa_floor, security_floor }` — each stage fills only
   the fields it has; each critic reads the standardized artifacts *from its own
   seat*. The QA/security critics are also handed the **deterministic floor
   findings first**, so their semantic pass targets what a hard check cannot see
   rather than re-deriving it.
3. **Doers = serial single-writer on one continuous session.** Frontend/backend
   engineers drive the *same* base session through every step (via
   `continuous::drive_rework_turn`, under the run-lock). They inherit their own
   accumulated context — direct, not re-primed.
4. **Critics = parallel read-only `fork()`s.** Each reviewing seat forks the
   session (isolated, read-only), runs ONE strict-JSON judge turn, and returns
   `RoleVerdict { accepts, blocking, advisory, evidence }`. Fail-open (a critic
   that errors/can't-fork returns a neutral accept).
5. **Deterministic floor governs the loop.** Coverage (FR→task) / contract
   (`umadev-contract` OpenAPI cross-check) / verify (real build+test+route probe)
   are computed *before* the team runs and OWN loop control. Critic verdicts are
   **advisory only** — they never drive termination.
6. **Deterministic aggregation → one typed rework directive.** Blocking findings
   fold into a single diagnosed, evidence-bearing rework directive injected back
   into the main session ("diagnosed, not 'go fix it'"), bounded by gap + stall
   counters.
7. **The router (L1) is a separate concern** — it classifies the turn; it is not
   the inter-role fabric.

So UmaDev is already a **hybrid**: *decision/contract handoffs and reviews use
standardized documents (the blackboard); doer continuity uses direct context
(the continuous session); reviewer input is a typed artifact bundle; aggregation
is deterministic, not an LLM vote.*

---

## 2. The five interaction patterns, and where each fails

| Pattern | Mechanism | Wins when | Dominant failure mode |
|---|---|---|---|
| **Blackboard** (Hearsay-II → 2025 revival) | specialists post to a shared structured store; a control unit schedules; the board *is* the memory | heterogeneous specialists build on a shared solution state; provenance must survive (a dev team on a repo) | scheduling bottleneck; a **stale/contradictory board silently poisons everyone**; unbounded board bloats context |
| **Direct message-passing / conversation** (AutoGen conversable agents) | agents send NL messages to each other | flexible, hard-to-pre-structure back-and-forth; human-in-loop; execute-debug loops | **worst of the five** — dialogue drift, telephone-game, loops, error cascades |
| **Shared message pool / pub-sub** (MetaGPT) | publish once to a global pool; **subscribe by role**; SOP mandates standardized artifacts | fixed role pipeline with well-typed handoffs | rigid SOP limits adaptivity; a bad publish still fans out; pool grows unbounded |
| **Handoff / routing** (OpenAI Swarm, *deprecated*) | transfer *control* via a tool call; receiver sees only handed-over context vars | intent triage / routing to one specialist | **lost provenance + dropped context at every hop** ("no magical memory") |
| **Structured-artifact relay vs. raw-context** | write a doc for the next role vs. pass the working context directly | *see §3 — this is the crux* | using a document as a *substitute* for the trace = provenance loss |

**Failure literature (why this matters):** the Berkeley **MAST** taxonomy (1,600+
traces, 7 frameworks) attributes multi-agent failures to **~42% specification,
~37% inter-agent misalignment/coordination, ~21% verification** — i.e. *most
failures are coordination/spec problems, not model weakness*, and it warns that
"solutions focused on communication protocols are often insufficient" because
free chat demands social reasoning models lack. Error-cascade work quantifies
the compounding: a ~1%/token error rate reaches ~87% failure by token 200;
**decentralized chat amplifies errors ~17×** vs a single agent, while
**centralized orchestration contains it to ~4.4×**; adding a **verifier after
each step recovers ~96%** of errors.

---

## 3. The crux: standardized documents vs. direct transmission

Two opposed 2025 flagship results, reconciled:

- **Raw/full context wins for *fidelity*** — Cognition, *Don't Build
  Multi-Agents*: "Share context, and share full agent traces, not just
  individual messages; actions carry implicit decisions, and conflicting
  decisions carry bad results." They advocate a **single-threaded** agent and
  warn models "are not quite able to engage in long-context proactive discourse
  with reliability."
- **Structured artifacts win for *durability/verifiability*** — MetaGPT
  (`Code = SOP(Team)`; structured PRD/interface/task docs explicitly to "reduce
  dialogue-induced hallucinations") and Anthropic (subagents need "an objective,
  an **output format**, and clear task boundaries").
- **The reconciliation** (LangChain, *How and when to build multi-agent
  systems*): "**Read actions are inherently more parallelizable than write
  actions** — parallelize information gathering, centralize decision-making and
  output production." That is UmaDev's exact split.

**Therefore, the answer to "标准化文档接力 vs 直接传输":**

> **Relay *decisions* as standardized documents. Relay *context* as the raw
> forked session. Never prose-summarize the trace to hand between agents.**

- **Direct/raw is better for the reasoning trace.** When a critic reviews the
  doer's work it should inherit the *full forked session* — the why, the tool
  outputs, the rejected options — not a hand-written recap. `fork()` = raw
  transmission with zero telephone-game.
- **Standardized documents are better for the decision/contract layer.** PRD,
  architecture, the API contract, acceptance criteria, `RoleVerdict` are
  *canonical, versioned, machine-verifiable ground truth* — they make
  frontend↔backend alignment checkable and survive a context reset.
- **A document is *worse* only when used as a substitute for the trace** — a
  linear phase-to-phase prose summary that drops the causal structure. *Beyond
  Compaction* shows prose summaries destroy provenance ("a tool call produced an
  output, that output informed a decision" collapses into narrative) and the
  loss is undetectable from the summary alone. If you must compress for length,
  compress to **structured/event-preserving records (typed logs, diffs, decision
  entries)**, not prose, and keep the raw trace retrievable.

---

## 4. Verdict: is our design right? — Yes. (Best-practice → what we already have)

| 2024–2025 best practice | UmaDev today |
|---|---|
| Parallelize reads, serialize writes | doers serial single-writer; critics parallel read-only forks ✔ |
| Blackboard as the source of truth; no agent↔agent chat | `output/*.md` + source tree; roles never chat ✔ |
| Centralized orchestration (contains errors ~4.4× vs ~17×) | the director is the sole scheduler + sole deterministic aggregator ✔ |
| Verifier after each step (recovers ~96%) | deterministic floor (coverage/contract/verify) + read-only critics per stage ✔ |
| Asymmetric context / isolated workers return summaries not raw dumps | critics fork in isolation and return typed `RoleVerdict`, not tool spew ✔ |
| Type every handoff (MetaGPT anti-hallucination) | `RoleVerdict {accepts,blocking,advisory,evidence}` + `umadev-contract` OpenAPI derivation ✔ |
| Handoff/routing only for triage, never the inter-role fabric | Swarm-style routing confined to the L1 router ✔ |
| Bound the loop | rework bounded by gap + stall counters ✔ |
| Budget cost; scale team to task | `*_team_for_kind` (a bugfix convenes no team) ✔ |

**Nothing in the SOTA says "refactor toward agents chatting." The evidence
endorses what we have.**

---

## 5. Enhancement plan (prioritized) — how to make it *stronger*

> **Delivery status.** Phase 1 (Seat Cards — the typed self-describing capability
> card + `ArtifactKind` vocabulary) and Phase 2 (the per-hop hand-off check:
> `Seat::missing_inputs` / `CriticArtifacts::present`) are **DONE** — tested +
> clippy-clean — and Phase 3 **wires the per-hop check into the live critic review
> flow** (a seat that reviews without its declared `reads` gets a diagnosed
> advisory folded into its verdict). Together that is the complete *typed-contract
> → per-hop-validation* vertical (the highest-leverage recommendation), live. The
> remaining items below are larger and touch core parsing / the plan DAG / verdict
> shape — do them deliberately, one tested increment each. (Note: a first attempt
> to add a structured `provenance` field directly to `RoleVerdict` was reverted —
> it broke ~24 struct-literal constructors; the right path is a `..Default::default()`
> refactor of those sites or a side-channel, done in a focused pass, not at a
> session tail.)

### P0 — the two-layer artifact (the operational answer to "docs vs transmission")
- **Give every blackboard artifact TWO layers: a schema-typed *contract* block
  (the *what*) + a natural-language *trace* block (the *why*).** A validated
  frontmatter / JSON sidecar carries the machine-checkable contract (route
  table, data model, design tokens, FR→acceptance map); the markdown body keeps
  the reasoning. Pure prose loses verifiability; pure schema loses the implicit
  decisions the next seat needs to interpret it. Version the schema keys and keep
  them stable. We already do this for the API surface (`umadev-contract` →
  OpenAPI) — **generalize it** to data model, design tokens, and acceptance.
- **Validate the contract AT THE HOP, before the next seat advances**
  (trajectory-level / per-hop validation). A bad field must fail at step 2, not
  surface at step 5 — inter-agent schema breaks are invisible to both sides until
  a late eval fires. This is the highest-leverage error-containment add.

### P0 — protect the two properties that already make us strong
- **Guarantee critics inherit the FULL forked session trace, not a compressed
  recap.** This is the single biggest lever against telephone-game degradation.
  Add a test/invariant that a critic's input is the forked session (+ typed
  `CriticArtifacts`), and treat any future move to feed critics a prose summary
  as a **regression**, not an optimization.
- **Keep decisions typed end-to-end.** Where a handoff is still prose inside an
  `output/*.md` doc (e.g. the API table), keep deriving the machine-checkable
  form (`umadev-contract` already renders OpenAPI). Extend the same "prose →
  typed artifact" derivation to acceptance criteria and the task DAG so
  downstream verification never re-parses prose.

### P1 — close the known blackboard failure modes
- **Public + private blackboard lanes.** Today the board is one global space.
  Add a **private/scratch lane** (per the 2025 blackboard papers) so a critic and
  a doer can resolve one specific conflict without polluting global context —
  the fix keeps global provenance clean while allowing focused back-and-forth.
- **Artifact versioning + staleness invalidation.** The blackboard's signature
  failure is *silent poisoning by a stale/contradictory board*. Version each
  `output/*.md` artifact; when an upstream artifact changes, have the director
  **invalidate the downstream plan steps that consumed it** (the plan is already
  a DAG in `.umadev/plan.json` — wire dependency edges to artifact versions).
- **Compress long runs to structured decision/event logs, not prose.** When a
  continuous session must be compacted for length, emit a typed decision log
  (what was decided, why, which artifact/version, which evidence) and keep the
  raw trace retrievable (hybrid summary-plus-retrieval). Never let `/compact`
  drop the causal structure the critics depend on.

### P2 — interop & auditability (adopt the *ideas* from the protocols, not necessarily the wire formats)
- **MCP (agent→tool) is already our lane** — UmaDev drives base CLIs and forwards
  MCP; keep tool access flowing through MCP so every tool call is auditable.
  MCP is *not* an agent-to-agent protocol; don't force it into that role.
- **A2A (agent→agent) ideas worth borrowing without the transport:** Google's
  Agent2Agent models work as **typed Tasks that carry Artifacts + Messages with
  explicit state**, and each agent publishes a capability **"Agent Card."** We
  don't need HTTP/JSON-RPC between our in-process seats, but we *can* adopt the
  shape: give each seat a declared **capability + input/output contract** (a
  typed "seat card"), and make every seat handoff a **typed Task {inputs
  (artifact refs + versions), expected output schema, acceptance}** rather than
  an implicit convention. This makes the roster self-describing, the handoffs
  verifiable, and a future real A2A bridge (exposing a UmaDev seat to an external
  agent) a small step.
- **Provenance/evidence tagging on every verdict.** `RoleVerdict.evidence`
  exists; make it *mandatory and structured* (artifact ref + version + line/loc)
  so a blocking finding is always traceable to the exact source — the audit
  trail then reconstructs the whole decision chain deterministically.

### Keep (do not "improve" into a regression)
- Centralized director as sole scheduler + **deterministic** aggregator — never
  let critics vote or drive loop control (MAST's #1/#2 failure classes are
  exactly what one deterministic controller suppresses).
- Advisory-only critics + hard deterministic floor.
- Cost gate (`*_team_for_kind`); the multi-session premium is real (Anthropic's
  multi-agent research system used ~15× tokens — worth it only for breadth-first
  high-value builds).

---

## 6. Anti-patterns — explicitly do NOT do these

- ❌ **Agent↔agent free-form chat** as the primary channel (top failure source;
  demands social reasoning models lack).
- ❌ **Group-chat where the transcript is the shared state** (O(n) context growth,
  speaker-selection drift, cost blowup).
- ❌ **Prose-summary handoff of the reasoning trace** between roles (provenance
  death, undetectable loss).
- ❌ **Letting critics vote / drive termination** (breaks the deterministic
  guarantee; re-introduces coordination failure).
- ❌ **Handoff/routing as the inter-role fabric** (drops context at every hop).

---

## 7. Sources

- MetaGPT — https://arxiv.org/abs/2308.00352 · https://arxiv.org/html/2308.00352v6
- ChatDev — https://arxiv.org/abs/2307.07924
- AutoGen — https://arxiv.org/pdf/2308.08155
- CrewAI processes — https://docs.crewai.com/en/concepts/processes
- LangGraph graph API — https://docs.langchain.com/oss/python/langgraph/graph-api
- LangChain, *How and when to build multi-agent systems* — https://www.langchain.com/blog/how-and-when-to-build-multi-agent-systems
- OpenAI Swarm / Agents SDK — https://github.com/openai/swarm · https://openai.github.io/openai-agents-python/multi_agent/
- Claude Code subagents — https://code.claude.com/docs/en/sub-agents · https://claude.com/blog/subagents-in-claude-code · https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
- Anthropic multi-agent research system — https://www.anthropic.com/engineering/multi-agent-research-system
- Cognition, *Don't Build Multi-Agents* — https://cognition.com/blog/dont-build-multi-agents
- MAST / *Why Do Multi-Agent LLM Systems Fail?* — https://arxiv.org/abs/2503.13657
- Blackboard revival — https://arxiv.org/html/2507.01701v1 · https://arxiv.org/html/2510.01285v1 · https://arxiv.org/html/2510.14312v1
- Context/provenance (compaction destroys structure) — https://www.langchain.com/blog/context-management-for-deepagents
- Standardized protocols — Google A2A https://developers.googleblog.com/en/a2a-a-new-era-of-agent-interoperability/ · Anthropic MCP https://www.anthropic.com/news/model-context-protocol · MCP vs A2A https://auth0.com/blog/mcp-vs-a2a/ · IBM ACP https://research.ibm.com/projects/agent-communication-protocol · ACP→A2A https://lfaidata.foundation/communityblog/2025/08/29/acp-joins-forces-with-a2a-under-the-linux-foundations-lf-ai-data/

> **Note on sources.** Primary vendor/foundation sources (A2A, MCP, ACP) and the
> two engineering essays (Anthropic multi-agent research system; Cognition *Don't
> Build Multi-Agents*), plus MetaGPT / ChatDev / AutoGen / MAST, are solid. A few
> arXiv IDs surfaced by the search index carried implausible/future dates and were
> dropped rather than cited as authoritative; the error-amplification figures are
> attributed to the secondary write-ups above.

## 8. Appendix — standardized protocols (A2A / MCP / ACP)

Two orthogonal protocol layers matured in 2024–2025; both now sit under the
Linux Foundation.

- **MCP (Model Context Protocol, Anthropic, Nov 2024) — *vertical*, agent→tool.**
  Three primitives: **tools** (callable actions), **resources** (readable
  context), **prompts** (templates); kills the M×N connector problem. The Nov
  2025 spec **requires tool servers to return schema-conforming structured
  output**. *This is already UmaDev's lane* — we drive base CLIs and forward MCP;
  keep every tool call flowing through MCP so it stays auditable. MCP is **not**
  an agent-to-agent protocol — don't force it into that role.
- **A2A (Agent2Agent, Google, Apr 2025 → Linux Foundation) — *horizontal*,
  agent↔agent.** **Agent Cards** (a `/.well-known/agent-card.json` capability
  descriptor), **Tasks** (explicit lifecycle, long-running, SSE-streamed),
  **Messages/Parts** (typed content), **Artifacts** (the finalized typed
  deliverable). By design agents stay **opaque — they do NOT share internal
  state.**
- **ACP (IBM, Mar 2025)** merged into A2A (Aug 2025), consolidating the
  horizontal space.

**The load-bearing guidance for us:** A2A's opacity (no shared state) is the
*opposite* of what coding wants — Anthropic explicitly names coding a poor fit
for opaque multi-agent hand-off because it needs shared context and many
dependencies. Therefore:

- **Internally**, keep the shared-context continuous session + a typed contract
  at the shared surface. Do **not** adopt A2A-style opaque agent-to-agent
  transmission between our own seats.
- **Reserve the A2A/MCP wire-shapes for true external boundaries only** —
  exposing a UmaDev seat to *another vendor's* agent (publish an Agent Card), or
  consuming an external tool/data source (MCP). Borrow A2A's *shape* internally
  (a typed "seat card" + typed Task handoff) without the transport, per §5 P2.


## 9. Executable handoff for the remaining items (P1/P2)

Delivered (phases 1-8, tested + clippy-clean + committed): Seat Cards
(ArtifactKind + SeatCard + Seat::card), the bidirectional per-hop contract
(Seat::missing_inputs / missing_outputs + CriticArtifacts::present, wired live in
run_critics_concurrently), and the versioning primitive (artifact_version /
is_stale). The four remaining items + their EXACT approach:

### A. Two-layer artifact materialization (largest; direction 1)
Extend the umadev-contract pattern (architecture API table -> typed ApiSpec ->
OpenAPI + cross-validate) to the data model, design tokens, acceptance map: a
section parser -> typed struct -> validator, emitted to .umadev/contracts/. Then
feed the materialized contracts into CriticArtifacts as first-class fields so
present() stops INFERRING them from the source doc. Test: a frontend referencing an
absent data-model field fails the cross-check.

### B. Verdict provenance (medium; constructor refactor first)
A Provenance { seat, artifact, note } + a provenance field on RoleVerdict, populated
Rust-side. BLOCKER (a first attempt was reverted): RoleVerdict is built via ~24 full
struct literals with no ..Default::default(). Do the refactor FIRST (append
..Default::default() to every literal, one commit, verify green), THEN add the field.

### C. Artifact versioning -> DAG staleness (medium)
Primitive exists. Store consumed versions WITHOUT touching PlanStep (no Default): a
side file .umadev/artifact-versions.json (step_id -> {artifact -> version}), written
on step completion. On re-plan, a done step whose recorded upstream is_stale vs the
current artifact flips back to Pending (cascade the reverse depends_on closure).

### D. Blackboard public/private lanes (medium)
A private scratch lane .umadev/scratch/<seat>-<topic>.md (never under output/),
visible only to the two seats in a conflict, NOT part of CriticArtifacts, GC'd on run
end.

Sequencing: A -> C -> B -> D. Each a tested, clippy-clean, committed increment. The
two Default-less struct refactors (B, and the PlanStep side-step in C) are exactly
where a fatigued session introduces regressions - do them fresh.


### Status delta (phases 6-8, delivered after this section was first written)
- **item C** is now DONE at the logic level: the versioning STORE
  (`read`/`write_artifact_versions`, `stale_artifacts` at `.umadev/artifact-versions.json`)
  AND the DAG invalidation (`Plan::invalidate_stale` - re-opens a step whose seat
  reads a stale artifact + its transitive downstream) are implemented + tested. ONLY
  the live director-flow wiring remains (record doc versions on a build pass; call
  `invalidate_stale` on resume) - deferred because its correctness depends on precise
  record/check lifecycle placement in the live run path (a fatigued mis-placement
  causes spurious re-runs in users' runs). A glob-based hook design (scan `output/`,
  map `*-<kind>.md` -> `ArtifactKind`, no slug plumbing) is worked out and ready.
- **item D** is DONE: the private scratch lane (`write`/`read`/`clear_scratch`) ships.
- **Remaining, genuinely large/risky:** item A (parser materialization) and item B
  (the ~24-site `RoleVerdict` `..Default::default()` refactor, then the field).


## 10. Roadmap COMPLETE (phases 1-11)

All planned enhancements from section 5 / section 9 are now implemented, tested, and
clippy-clean:

- Seat Cards (typed self-describing capability + I/O contract per seat).
- Per-hop contract - BOTH directions (missing_inputs + missing_outputs), wired live
  into run_critics_concurrently.
- item A - two-layer artifact materialization: umadev_agent::materialize parses the
  data model / design tokens / acceptance out of the prose docs into typed contracts,
  emitted to .umadev/contracts/derived-contracts.json, wired into the docs stage.
- item B - structured verdict provenance: RoleVerdict.provenance (seat + artifact +
  diagnosed note), populated by the per-hop check; serde-backward-compatible.
- item C - artifact versioning to DAG staleness invalidation: artifact_version /
  is_stale + the .umadev/artifact-versions.json store + Plan::invalidate_stale + the
  live director wiring (record on save, re-open stale downstream on resume).
- item D - private blackboard scratch lane: write/read/clear_scratch.

Every piece is additive, deterministic, and fail-open; the deterministic floor still
owns loop control (critic opinions + these signals are advisory). Full workspace
clippy + tests green.
