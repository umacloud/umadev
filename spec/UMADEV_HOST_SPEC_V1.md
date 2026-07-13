# UmaDev Host Specification, Version 1 (UMADEV_HOST_SPEC_V1)

> **Status:** Draft  
> **Version:** 1.0.0-draft.5  
> **Date:** 2026-06-24  
> **Editor:** UmaDev maintainers (`<11964948@qq.com>`)  
> **License:** MIT  

This document defines the **UmaDev Host Specification**: the set of
constraints, contracts, artifacts, and evidence requirements that an AI
coding host MUST satisfy to be called a *conformant UmaDev host*.

UmaDev itself is not a code generator, not an IDE, not a workflow tool.
**UmaDev is this specification.** Every shipped binary (`umadev` CLI,
SKILL.md packages, `umadev-governance` MCP server, hook scripts, host
adapter recipes) exists to **inject this specification** into a host's
native configuration surfaces, or to **verify** that a host meets it.

### The coach metaphor

The reader's everyday mental model for UmaDev is this:

> **UmaDev is a coach for the host.** It does not write code itself.
> It hands the host a complete, pipeline-shaped playbook for delivering a
> commercial software project — what to research first, what artifacts to
> produce, when to pause and ask for sign-off, what to refuse to write,
> what evidence to leave behind — and then steps off the field. The
> host's existing model + tools execute; the coach's standard is what
> makes the result commercial-grade.

The remainder of this document is the playbook, expressed as machine-
verifiable normative clauses. The user-facing CLI (`umadev install`,
`umadev verify`, `umadev report`) is the coach "handing over the
playbook" and "watching from the sideline."

Coding hosts are independent products. UmaDev's first-class drivers are
**exactly three** base CLIs — `claude-code`, `codex`, and `opencode`
(`umadev_host::BACKEND_IDS` is the authoritative list); everything else
is out of first-class support. Any other host MAY still meet this spec by
adopting UmaDev's reference injectors, or by implementing the rules
natively. Both paths produce a conformant host.

## 1. Conformance and conventions

### 1.1 Keywords

The keywords **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, **MAY**,
**REQUIRED**, **OPTIONAL**, and **RECOMMENDED** in this document are to be
interpreted as described in RFC 2119, when, and only when, they appear in
all capitals.

### 1.2 Clause identifiers

Every normative clause has a stable identifier of the form `UD-<LAYER>-<NUM>`,
where `<LAYER>` is one of:

- `CODE` — code-weight constraints (Layer 1, §3)
- `FLOW` — flow contract (Layer 2, §4)
- `ART` — delivery artifacts (Layer 3, §5)
- `EVID` — evidence chain (Layer 4, §6)
- `META` — versioning and conformance declaration (§8)

§7 (host surface mapping) is **non-normative guidance**: it carries no
clauses and therefore reserves no `UD-HOST-*` identifier space. Every
normative clause lives under one of the prefixes listed above.

Clause IDs are permanent. A clause MAY be deprecated in a later version
but MUST NOT be renumbered or repurposed.

### 1.3 Conformance levels

A host declares its conformance level via the `UMADEV_HOST_SPEC_V1`
manifest (§8.1). Three levels exist:

| Level | Definition |
|---|---|
| **L1 — Aware** | The host loads UmaDev's persistent rules surface (e.g. `CLAUDE.md`, `AGENTS.md`, equivalent). The model sees the rules but enforcement is advisory. |
| **L2 — Enforced** | All MUST-level rules in §3 are enforced at the host's tool-call boundary (block before write). Confirmation gates in §4 are honored. |
| **L3 — Audited** | L2 plus all evidence requirements in §6 produce machine-readable artifacts. The host produces a UmaDev–compatible proof pack. |

A host that satisfies all MUST clauses at the L3 level is **fully
conformant**. Hosts MAY claim partial conformance at L1 or L2.

### 1.4 Test vectors

Each enforceable clause SHOULD link to a test vector — a `(file_path,
content) → expected_decision` tuple that any implementation can run to
verify its enforcement layer. Reference test vectors live in
`tests/spec_vectors/<clause-id>.json`.

## 2. Definitions

- **Host** — a user-facing AI coding product (CLI, IDE, agent, web app)
  that invokes a language model on the user's behalf and may call tools
  (read/write file, run shell, etc.).
- **Tool call** — any model-initiated action that mutates the user's
  workspace or environment.
- **Pre-write checkpoint** — the host-internal hook fired immediately
  before a Write/Edit/Patch tool call is dispatched. MUST be cancellable.
- **Post-write checkpoint** — the host-internal hook fired immediately
  after a Write/Edit/Patch tool call has returned.
- **Prompt checkpoint** — the host-internal hook fired immediately before
  the model receives the user's prompt.
- **Workspace** — the directory the host treats as the project root.
- **Surface** — a host-native configuration file or directory through
  which rules can be injected (e.g. `CLAUDE.md`, `.cursor/rules/`,
  `AGENTS.md`, `.factory/rules/`, MCP server registration, hooks.json).
- **Spec injection** — the act of writing/updating a surface so that a
  host enforces a UmaDev clause.

## 3. Layer 1 — Code-weight constraints

This layer governs **what model-emitted code MUST NOT contain** when it
lands in a UI / source file.

### 3.1 Emoji as functional icons (`UD-CODE-001`)

> Level: **MUST**  
> Test vector: `tests/spec_vectors/UD-CODE-001.json`

A conformant host **MUST** refuse a Write/Edit/Patch operation if, in any
file whose extension is in {`tsx`, `ts`, `jsx`, `js`, `vue`, `svelte`,
`astro`}, the *new content* contains any codepoint in the ranges:

- `U+2600–U+27BF` (Miscellaneous Symbols / Dingbats)
- `U+1F300–U+1FAFF` (Pictographs / Symbols / Supplemental Symbols)
- `U+1F900–U+1F9FF` (Supplemental Symbols and Pictographs)
- `U+1FA70–U+1FAFF` (Symbols and Pictographs Extended-A)

The host **MUST** return a refusal reason that instructs the model to use
a declared icon library (e.g. Lucide, Heroicons, Tabler).

A conformant host **SHOULD NOT** apply this rule to documentation files
(`.md`, `.mdx`, `.rst`, `.txt`).

### 3.2 Hardcoded color literals (`UD-CODE-002`)

> Level: **MUST**  
> Test vector: `tests/spec_vectors/UD-CODE-002.json`

In files whose extension is in {`tsx`, `ts`, `jsx`, `js`, `vue`, `svelte`,
`astro`, `css`, `scss`, `sass`}, a conformant host **MUST** refuse a
Write/Edit if the new content contains any literal chromatic color of the
forms:

- `#RGB`, `#RGBA`, `#RRGGBB`, `#RRGGBBAA` (case-insensitive)
- `rgb(...)`, `rgba(...)`
- `hsl(...)`, `hsla(...)`

The following are **exempt**:

- The achromatic literals `#fff`, `#ffffff`, `#000`, `#000000`
- CSS custom property references (`var(--*)`)
- Files whose path matches any of:
  - `/tokens/`, `/theme/`, `/themes/`, `/design-system/`, `/design-tokens/`
  - `/.storybook/`, `*.stories.*`, `*.test.*`, `*.spec.*`
  - `/fixtures/`, `/mocks/`

A conformant host **SHOULD** include the offending literal in the refusal
reason (up to 5 distinct examples) to aid the model's self-correction.

### 3.3 Frontend–backend API path alignment (`UD-CODE-003`)

> Level: **SHOULD** at L2; **MUST** at L3.  
> Test vector: `tests/spec_vectors/UD-CODE-003.json`

In files of frontend extension (see §3.1), a conformant host **SHOULD**
extract every URL emitted by `fetch(...)`, `axios.<verb>(...)`,
`ky.<verb>(...)`, `useSWR(...)`, `useQuery(...)`, or `http.<verb>(...)`,
filter to paths beginning with `/`, and:

- At **L2**: persist the extracted set into the workspace audit log (§6.1).
- At **L3**: additionally cross-check each path against backend route
  definitions (any of `output/*-architecture.md`, `umadev.yaml`,
  `openapi.yaml`, or an auto-discovered framework route table). If a
  frontend path does not appear in any backend source-of-truth, the host
  **MUST** surface this as a `compliance:api-mismatch` warning in the
  Quality Report (§6.3).

A conformant host **MUST NOT** block a Write solely on §3.3 — alignment
is verification-time, not write-time, because backend route definitions
may not yet exist in early phases.

### 3.4 Tech-stack pre-research (`UD-CODE-004`)

> Level: **SHOULD**

Before the host writes its first non-scaffolding source file in a fresh
project, the conformant host **SHOULD**:

1. Read the project's dependency manifest (`package.json`,
   `requirements.txt`, `pyproject.toml`, `go.mod`, ...).
2. For each top-level framework with declared version, fetch the official
   docs (or rely on cached knowledge) before generating code for it.

This is an advisory clause and is **not** machine-verifiable at L2. At
L3, the Quality Report (§6.3) **MUST** include a section indicating
whether pre-research occurred.

### 3.5 Test-integrity / anti-reward-hacking guard (`UD-QA-001`)

> Level: **MUST**

A borrowed brain can make a failing test suite report "pass" without
delivering working code by **gaming the tests** rather than fixing the
implementation. A conformant host **MUST NOT** trust a step's passing test
signal when, across that step's doer turn, the project's **test files** were
weakened. The host **MUST** deterministically compare the test surface
*before* and *after* the doer turn and treat any of the following as a
blocking integrity violation:

1. a test file or a test case (`it` / `test` / `def test_` / `#[test]` / …)
   was **deleted**;
2. the **assertion count** in a kept test dropped with no corresponding test
   removed (assertions stripped out of a surviving test);
3. a **skip / xfail / ignore / `.only` / focus** marker, or commented-out
   test code, was newly introduced;
4. a test now **asserts a hard-coded literal that matches the
   implementation's own output** (best-effort), trivially forcing a green;
5. the **test harness / runner config or the test command** (`jest.config.*`,
   `pytest.ini`, `package.json` `scripts.test`, …) was **modified or deleted**
   during the build step.

The guard is part of the **deterministic floor** (not an advisory critic): a
violation is folded into the step's acceptance as a blocking finding with a
typed, file-naming directive, driving a **bounded** rework round under the
host's existing fix-round / stall counters — never an open-ended loop. It is
**fail-open**: when integrity cannot be determined (no baseline snapshot, an
unreadable tree, an unparseable file) the guard yields **no** finding rather
than a spurious block, and **adding** new tests is never a violation — only
the destruction or weakening of pre-existing test signal is. A
genuinely-passing, un-gamed suite is unaffected.

### 3.6 Architecture-fitness floor (`UD-CODE-006`)

> Level: **MUST** (sub-rule `UD-CODE-006c` is advisory)

The injected firmware *preaches* layering, small focused modules, and no
copy-paste — but a prompt is not a floor. A borrowed brain under pressure
ships one giant file, imports the database from the UI, and pastes the same
block into three places while every other deterministic check (build/test,
coverage, contract, test-integrity) still reads green. A conformant host
**MUST** verify architecture fitness on its own deterministic floor, with
three sub-rules:

1. **God-file gate** (`UD-CODE-006a`, blocking) — a **NEW** source file over
   500 lines, or a touched file that **grew past** 800 lines during the
   step, is a blocking finding carrying a split directive ("split by
   feature/domain"). The grown ceiling **MAY** be overridden by host
   configuration; generated, vendored, lock, and test files are exempt.
   Without a before-baseline, newness cannot be established, so only the
   hard grown ceiling applies to a touched file — a merely-touched legacy
   file is never falsely blocked.
2. **Layer-dependency rules** (`UD-CODE-006b`, blocking) — the architecture
   document (`output/<slug>-architecture.md`) **MAY** declare a layering
   contract: a directory→layer mapping table, a one-way `->` order chain,
   and/or explicit `LAYER-RULE: a !-> b` ban lines. When declared, every
   resolved import edge of the repository **MUST** be checked against it;
   an edge that goes against the declared one-way order or crosses a
   banned pair is a blocking finding naming both files and the violated
   rule. No declaration in the document → the check silently no-ops.
3. **Clone detection** (`UD-CODE-006c`, advisory) — normalized
   (whitespace-squeezed, comment-stripped) windows of code **added** in
   touched files are compared against the rest of the repository; a
   duplicated block of at least 5 lines yields an advisory finding naming
   the sibling location ("reuse it instead"). Deduplication judgment stays
   with the critics and the user; the host **MUST NOT** block on this
   sub-rule alone — the floor only surfaces the evidence.

Blocking findings are part of the **deterministic floor** (not an advisory
critic): each is folded into the step's acceptance as a typed, file-naming
rework directive, driving a **bounded** rework round under the host's
existing fix-round / stall counters — never an open-ended loop. The gate is
**fail-open**: an unreadable or absent architecture document, a document
with no layering declaration, an empty or unresolvable import-edge set, an
unreadable tree, and an oversized repository (a blown scan budget) all
degrade to a silent skip — the gate never fabricates a block and never
errors. The sub-rule identifiers `UD-CODE-006a/b/c` label individual
findings; the clause carried in the machine-readable `CLAUSES` data is the
parent `UD-CODE-006`. (The `-005` slot in this family remains reserved for
the §10 accessibility candidate.)

## 4. Layer 2 — Flow contract

This layer governs **the order, gates, and continuity** of the
development process inside a conformant host.

### 4.1 Phase chain (`UD-FLOW-001`)

> Level: **MUST**

The conformant host **MUST** model the development pipeline as the
following ordered chain:

```
research → docs → docs_confirm → spec → frontend → preview_confirm
        → backend → quality → delivery
```

Each phase identifier is normative. A host MAY add sub-phases but MUST
NOT reorder, skip, or rename the listed phases without declaring a spec
profile (§8.4).

The host **MUST** persist the active phase to `.umadev/workflow-state.json`
in a format containing at least the keys `phase` and `active_gate`.

**Scope of this clause.** `UD-FLOW-001` governs the *full commercial
delivery build* — the `standard` profile of §8.4. It is the contract for
**how a heavyweight greenfield product is delivered**: when a host commits
to that play, it MUST run these phases in this order and honor the gates.
It does **not** require that *every* user turn be a nine-phase walk. A
casual question, a read-only review, a one-line edit, or a bugfix is not a
commercial delivery build and is **out of this clause's scope** (see the
team-scaling rule in §9.4 and the lightweight `seeai` profile in §8.4). The
reference implementation reaches the phase chain by **routing** a turn to
the deliberate path and synthesizing a plan whose deepest form *is* this
chain (§9.5) — the chain is the deep play the coordinator routes the team
into for a full build, not a funnel every message is forced through.

### 4.2 Docs confirmation gate (`UD-FLOW-002`)

> Level: **MUST**

After the `docs` phase produces all artifacts required by §5.2, the
conformant host **MUST**:

1. Pause the pipeline.
2. Set `workflow-state.json#active_gate` to `"docs_confirm"`.
3. Refuse any spec-writing or code-writing tool call until the user
   submits an explicit approval prompt.

Approval prompts that count as confirmation include exact matches of:
`确认`, `通过`, `继续`, `approved`, `approve`, `lgtm`, `ship it`. The
host **MAY** extend this set but **MUST NOT** infer approval from
unrelated user input.

### 4.3 Preview confirmation gate (`UD-FLOW-003`)

> Level: **MUST**

After the `frontend` phase produces a runnable preview, the conformant
host **MUST** apply the same gate semantics as §4.2 with
`active_gate = "preview_confirm"`. The host **MUST NOT** begin the
`backend` phase before user approval.

### 4.4 Gate-local revisions (`UD-FLOW-004`)

> Level: **MUST**

While `active_gate` is non-empty, any user message **MUST** be
interpreted as *inside the active gate*. Replies that request revisions
(`修改`, `补充`, `继续改`, free-form edit requests) **MUST**:

1. Keep the pipeline in the same phase.
2. Update the affected artifact in place.
3. Re-stage the gate (the host MUST wait for explicit approval again).

A conformant host **MUST NOT** silently exit UmaDev mode in response
to revision requests.

### 4.5 Phase-local artifact mutability (`UD-FLOW-005`)

> Level: **MUST**

Within a phase, the host **MUST** be able to revise the artifacts
produced by earlier phases (e.g. update `output/*-architecture.md` from
inside `backend`) as long as the active gate is re-staged afterward.

### 4.6 Session continuity (`UD-FLOW-006`)

> Level: **MUST**

On every model prompt, the conformant host **MUST**:

1. Read `.umadev/SESSION_BRIEF.md` if it exists.
2. Read `.umadev/workflow-state.json` if it exists.
3. Prepend a digest of these into the model's context (mechanism is
   host-defined: a system reminder, a hidden prefix, a tool result, etc.).

The host **MUST NOT** rely on the user to repeat workflow state between
turns.

### 4.7 Role-critic team (`UD-FLOW-007`)

> Level: **MUST**

A conformant host **MUST** model its quality judgement as a *team*, not a
single pass: a directing role drives the pipeline and one or more
**read-only role critics** cross-review the shared artifacts from their own
seat (e.g. product-manager, architect, qa-lead) and return structured
verdicts. The team layer is bound by four hard invariants:

1. **Deterministic floor is the gate.** A critic verdict is **advisory
   only**. Loop termination and gate progression **MUST** stay governed by
   the deterministic floor (coverage / contract / governance gap counts +
   stall counter). A non-deterministic critic opinion **MUST NOT** drive
   loop control. Critic findings MAY be fed back as advisory fixes.
2. **Read-only / single-writer.** A critic **MUST NOT** write files or
   mutate the workspace. It reviews artifacts on an isolated, no-resume
   forked session; only the directing (main) session ever writes.
3. **No new endpoint.** A critic **MUST** run over the *same* borrowed base
   brain via the existing host-driver subprocess — no extra model endpoint
   and no extra API key.
4. **Fail-open.** A critic that errors, cannot be forked, or returns
   unparseable output **MUST** yield an empty verdict that accepts; an
   absent critic can never block the base.

### 4.8 Trust tiers + irreversible-action floor (`UD-FLOW-008`)

> Level: **MUST**

A conformant host **MUST** expose a progressive-trust ladder controlling
how much autonomy the run is granted at the confirmation gates:

- `plan` — research + planning only; **read-only**, never executes real
  code (stops at `docs_confirm`).
- `guarded` — the **default**; every gate pauses for explicit confirmation
  (the §4.2 / §4.3 human-in-the-loop behaviour).
- `auto` — fully autonomous; every gate auto-approves.

Riding on top of the ladder is a **hard reversibility floor**: any action
that is irreversible or blast-radius-heavy — touching version-control
internals, the **network**, a **push**, opening a **PR**, a **deploy**, or a
destructive shell verb — **MUST** be escalated to an explicit confirmation
**regardless of mode**. `auto` does **not** get to skip the floor; the only
non-interactive bypass is an explicit `--yes`. The classifier is a pure,
deterministic function of the action — it introduces no new model endpoint
and no randomness.

## 5. Layer 3 — Delivery artifacts

This layer governs **what files MUST land on disk** at each phase. Files
are normative; the format inside each file is illustrative.

### 5.1 Research artifacts (`UD-ART-001`)

> Level: **MUST**

The `research` phase **MUST** produce, in the workspace `output/`
directory, all of:

| Path | Content |
|---|---|
| `output/<slug>-research.md` | Similar-product research, citing sources |
| `output/knowledge-cache/<slug>-knowledge-bundle.json` | Local knowledge hits + research summary |

`<slug>` is the project identifier; if absent, the host **MUST** derive
one from the workspace name.

### 5.2 Core documents (`UD-ART-002`)

> Level: **MUST**

The `docs` phase **MUST** produce all of:

| Path | Required sections |
|---|---|
| `output/<slug>-prd.md` | Goal, scope, user stories, acceptance criteria |
| `output/<slug>-architecture.md` | System diagram, API surface, data model, tech stack rationale |
| `output/<slug>-uiux.md` | Design tokens, component skeleton, page hierarchy, accessibility notes |

The host **MUST NOT** advance to `docs_confirm` until all three files
exist and are non-empty.

### 5.3 Spec + tasks (`UD-ART-003`)

> Level: **MUST**

The `spec` phase **MUST** produce:

| Path | Content |
|---|---|
| `output/<slug>-execution-plan.md` | Spec narrative, task breakdown |
| `.umadev/changes/<change-id>/tasks.md` | Machine-trackable task list |

### 5.4 ADR records (`UD-ART-004`)

> Level: **SHOULD**

For every non-trivial architectural decision, the host **SHOULD** create
an ADR file under `.umadev/decisions/ADR-<id>-<slug>.md` containing:
context, decision, alternatives, consequences.

### 5.5 Mutability of artifacts (`UD-ART-005`)

> Level: **MUST**

Artifacts in §5.1–§5.4 **MUST** be re-writable by the host throughout
the pipeline (e.g. `output/*-uiux.md` is updated from inside `frontend`
when the user requests a redesign). The host **MUST NOT** treat any
artifact as read-only once produced.

### 5.6 No chat-only completion (`UD-ART-006`)

> Level: **MUST**

A conformant host **MUST NOT** declare a phase complete based on chat
output alone. The required artifact file(s) **MUST** exist in the
workspace before the host advances.

### 5.7 PR artifact (`UD-ART-007`)

> Level: **SHOULD**

When a finished run is handed off as a pull request, the conformant host
**SHOULD** render the PR body from the run's **own evidence**, not a bare
diff: the PR-ready review report (§6.8) followed by a proof-pack (§6.5)
summary. The assembled body **SHOULD** be persisted to
`output/<slug>-pr-body.md` so the same artifact backs both the dry-run
preview and the opened PR. Opening the PR is a network/VCS action governed
by the irreversibility floor (§4.8): the host **MUST NOT** push or open a
PR without confirmation, **MUST NOT** commit on the default branch (it
creates a feature branch first), and **MUST** degrade fail-open to printed
manual steps when `git` / `gh` is missing or not logged in.

## 6. Layer 4 — Evidence chain

This layer governs **what audit-grade evidence MUST be produced** so the
result of an AI-driven development run is verifiable by reviewers,
auditors, and compliance officers.

### 6.1 API audit log (`UD-EVID-001`)

> Level: **MUST** at L3

Every API path extracted under §3.3 **MUST** be appended to
`.umadev/audit/frontend-api-calls.jsonl` as one JSON object per line
containing at least:

```json
{
  "ts": <integer unix seconds>,
  "file": "<workspace-relative path>",
  "tool": "<host tool name, e.g. Write|Edit>",
  "urls": ["<extracted url>", ...],
  "session_id": "<opaque host session id>"
}
```

The host **MUST NOT** rewrite or truncate prior entries. The log is
append-only.

### 6.2 Tool-call audit log (`UD-EVID-002`)

> Level: **SHOULD** at L3

For every Write/Edit/Patch tool call, the host **SHOULD** append to
`.umadev/audit/tool-calls.jsonl` a record with: timestamp, tool name,
target file, decision (allow/block/warn), and the governance rule that
fired (clause ID, e.g. `UD-CODE-001`). This record is the primary input
for the compliance mapping (§6.4).

### 6.3 Quality report (`UD-EVID-003`)

> Level: **MUST** at L3

At the `quality` phase, the host **MUST** produce
`output/<slug>-quality-gate.json` containing at minimum:

```json
{
  "passed": <bool>,
  "total_score": <integer 0-100>,
  "weighted_score": <float 0-100>,
  "scenario": "<run scenario id>",
  "critical_failures": ["<string>", ...],
  "recommendations": ["<string>", ...],
  "summary": {
    "executive_summary": "<one-line headline>",
    "summary_context": {"<key>": "<value>", ...}
  },
  "checks": [
    {
      "name": "<check id>",
      "category": "<grouping>",
      "description": "<human readable>",
      "status": "passed|warning|failed",
      "score": <integer 0-100>,
      "weight": <float>,
      "details": "<freeform>"
    }
  ]
}
```

A run that emits `passed: false` **MUST** cause the host to refuse
advancing to `delivery`. The pass threshold defaults to `90`; projects
MAY override via `umadev.yaml#quality_gate`. The host **MAY** wrap
the document with the `evidence_identity` envelope used by the
reference implementation (`umadev.cli_release_quality_mixin`) — that
wrapping is OPTIONAL and does not break conformance.

The companion human-readable report `output/<slug>-quality-gate.md` is
RECOMMENDED but not required.

### 6.4 Compliance mapping (`UD-EVID-004`)

> Level: **SHOULD** at L3

At the `delivery` phase, the host **SHOULD** emit
`output/<slug>-compliance-mapping.json` linking each UmaDev clause
that fired during the run to its mapping into external compliance
frameworks. At minimum:

| External framework | Mapping field |
|---|---|
| SOC 2 (2017 TSC) | `soc2_cc` (e.g. `CC9.2`) |
| ISO/IEC 27001:2022 | `iso27001_annex_a` (e.g. `A.14.2.1`) |
| EU AI Act (2024/...) | `eu_ai_act_article` (e.g. `Article 15`) |

Recommended top-level shape:

```json
{
  "spec_version": "UMADEV_HOST_SPEC_V1",
  "slug": "<project slug>",
  "generated_at": "<ISO8601>",
  "quality_gate_passed": <bool>,
  "clauses": [
    {
      "id": "UD-CODE-001",
      "fired_count": <integer>,
      "soc2_cc": ["CC9.2"],
      "iso27001_annex_a": ["A.14.2.1"],
      "eu_ai_act_article": ["Article 15"],
      "evidence": [".umadev/audit/tool-calls.jsonl"]
    }
  ]
}
```

The mapping is the foundation of UmaDev's compliance evidence pack.
The reference implementation lives in `umadev.governance.compliance`.

### 6.5 Proof pack (`UD-EVID-005`)

> Level: **MUST** at L3

At the `delivery` phase, the host **MUST** assemble a *proof pack*
archive containing every artifact named in §5 plus every evidence file
named in §6.1–§6.4. Recommended location: `release/proof-pack-<run-id>.zip`.

The proof pack is the **product output of a UmaDev–conformant run**.
A reviewer SHOULD be able to verify spec conformance solely by inspecting
the pack — no host session required.

### 6.6 Runtime evidence (`UD-EVID-006`)

> Level: **SHOULD** at L3

Beyond proving the code *was written*, a conformant host **SHOULD** prove
the app *actually runs*: boot the detected dev server, wait for it to
answer, probe the documented routes over HTTP, and persist the result to
`.umadev/audit/runtime-proof.json` (per-route status + the boot command),
which is then folded into the delivery proof pack (§6.5). This clause is
**fail-open**: a missing dev server, probe tool, or route table is recorded
as `"not verified"`, never an error.

### 6.7 Deploy evidence (`UD-EVID-007`)

> Level: **SHOULD** at L3

When the post-delivery handoff actually deploys, the conformant host
**SHOULD** capture the deploy outcome — the detected target, the executed
command, the resulting preview/live URL, and a log tail — into
`.umadev/audit/deploy-proof.json`, folded into the delivery proof pack
(§6.5). The deploy itself runs through the user's own logged-in platform
CLI (UmaDev owns no credentials and injects nothing) and is gated by the
irreversibility floor (§4.8). **Fail-open**: an unknown platform, missing
CLI, or failed deploy is recorded as `"not deployed"` with a manual hint,
never an error.

### 6.8 Review-report evidence (`UD-EVID-008`)

> Level: **SHOULD** at L3

A conformant host **SHOULD** assemble the run's checks into a single
PR-ready review report at `output/<slug>-review-report.md` — contract
alignment (§3.3 / §6.1), acceptance, FR→task coverage, governance
(§6.2), the pre-PR security scan, runtime evidence (§6.6), and the
rollback/checkpoint story — each section degrading to an honest "not
available" line rather than failing. This report is the human-readable
case-for-merge that backs the PR artifact (§5.7).

## 7. Host surface mapping

Every clause in §3–§6 is layer-agnostic. This section is **non-normative
guidance** showing how each layer is realized on UmaDev's
officially-supported host families. Other hosts MAY implement this spec
independently; the reference implementation (`umadev install <host>`)
ships plugin bundles only for the families listed below.

### 7.1 Officially supported hosts

The reference implementation drives exactly **three** first-class base
CLIs — `claude-code`, `codex`, and `opencode` — each an
already-logged-in host CLI run as a subprocess (`umadev_host::BACKEND_IDS`
is the authoritative id list). They resolve to the two host families that
have an **official Agent SDK** as of UMADEV_HOST_SPEC_V1's draft date
(May 2026):

| Host family | Provider | SDK | Workspace install | User-scope install |
|---|---|---|---|---|
| **Claude Code** / Claude Desktop | Anthropic | Claude Agent SDK | `.claude/` | `~/.claude/` |
| **Codex CLI** / Codex Desktop | OpenAI | OpenAI Agents SDK | `AGENTS.md` + `.codex/` | `~/.codex/` |

Hosts outside this set (Cursor, Windsurf, Cline / Roo, Continue, Droid
CLI, Kiro IDE, Trae, Qoder, CodeBuddy, …) are explicitly **out of
scope** for the reference implementation. They MAY still implement this
specification by adopting the same `.umadev/` workspace layout and
the same hook commands; the reference plugin bundles will not be
shipped for them.

### 7.2 Reference surface inventory

Only **Claude Code** exposes a tool-lifecycle hook surface
(`hooks.PreToolUse` / `hooks.PostToolUse`). Codex and OpenCode have **no such
hook** — the reference implementation does NOT invent a `.codex/config.toml`
(or opencode) hook for them. For those two bases the SAME governance effect is
delivered runner-side: UmaDev's own runner scans each write before it is
applied and records the audit trail as it streams the base's tool-use events
(`umadev_agent::continuous::govern_tool_call`,
`director_loop::record_tool_call_audit`), and the firmware reaches the base via
the first-directive prefix / `AGENTS.md` rather than a native system-prompt
hook. The clause *effect* is preserved on all three; only the delivery pipe
differs (this is the §7.4 degradation in practice).

| Surface | Claude Code | Codex CLI | OpenCode |
|---|---|---|---|
| Persistent rules | `.claude/skills/umadev/SKILL.md`, `CLAUDE.md` | `AGENTS.md` | `AGENTS.md` |
| Slash command | `commands/umadev.md` | (host-defined; AGENTS.md guidance) | (host-defined; AGENTS.md guidance) |
| Firmware injection | native `--append-system-prompt` | first-directive prefix (no native slot) | first-directive prefix (no native slot) |
| Pre-write governance | `hooks.PreToolUse` matcher `Write\|Edit` | runner-side pre-apply scan (no host hook) | runner-side pre-apply scan (no host hook) |
| Post-write audit | `hooks.PostToolUse` matcher `Write\|Edit` | runner-side tool-use stream (no host hook) | runner-side tool-use stream (no host hook) |
| Prompt-time context | `hooks.UserPromptSubmit` | runner-side per-turn injection | runner-side per-turn injection |
| Artifact dir | workspace `output/` | workspace `output/` | workspace `output/` |
| Evidence dir | workspace `.umadev/audit/` | same | same |

### 7.3 Reference clause → hook command

All host families invoke the **same** `umadev hook <subcommand>`
binary; only the host-specific config syntax differs. The binary itself
implements every governance clause once.

| Clause | Pipeline event | Hook subcommand |
|---|---|---|
| `UD-CODE-001` (emoji) | Pre-write | `umadev hook check-emoji` |
| `UD-CODE-002` (color) | Pre-write | `umadev hook check-color` |
| `UD-CODE-003` + `UD-EVID-001` (API audit) | Post-write | `umadev hook audit-api` |
| `UD-EVID-002` (tool-call audit) | Post-write | `umadev hook tool-audit` |
| `UD-FLOW-006` (session continuity) | Prompt-time | `umadev hook inject-context` |

### 7.4 Surface-not-available degradation

Where a host lacks a surface required by a clause (e.g. a host whose
hook contract is not yet GA), the conformant injector MUST substitute
the next-best surface — for example, the bundled
`AGENTS.md` instructs the host model to invoke the equivalent
`umadev hook …` commands manually before committing UI source. The
clause's *effect* is preserved; only the delivery pipe is host-specific.
Conformance level MAY drop from L2 to L1 on the affected clause if
substitution is not viable.

## 8. Versioning and conformance declaration

### 8.1 Spec manifest (`UD-META-001`)

> Level: **MUST**

A conformant host workspace **MUST** contain a top-level marker that
declares its spec conformance level. The canonical marker is in
`umadev.yaml`. The `spec:` block is **normative** — every key in it
is required:

```yaml
spec:
  version: UMADEV_HOST_SPEC_V1
  level: L3
  profile: standard   # or "seeai" for competition mode
  declared_by: umadev@1.0.0
```

A host MAY append **non-normative** blocks that its own tooling reads;
these MUST NOT affect conformance judgement. The reference
implementation appends two:

```yaml
project:
  slug: <project-slug>      # used in artifact filenames
quality_gate:
  threshold: 90             # UD-EVID-003 pass threshold
```

A conformant verifier **MUST** ignore blocks it does not recognise and
**MUST NOT** fail conformance because of them.

### 8.2 Version negotiation (`UD-META-002`)

> Level: **MUST**

When a host attaches to a workspace whose declared spec version is
higher than the host supports, the host **MUST** refuse to proceed and
emit `compliance:version-mismatch`. It **MUST NOT** silently downgrade
the workspace.

### 8.3 Backward compatibility (`UD-META-003`)

> Level: **MUST**

Within a major version (V1 → V1.x), removing or strengthening a clause
**MUST NOT** happen. Clauses MAY be added with `SHOULD` or `MAY` levels
and promoted to `MUST` only at the next major version.

### 8.4 Profiles (`UD-META-004`)

> Level: **MAY**

A host MAY declare a **profile** that adjusts non-MUST clauses. Two
profiles are reserved:

- `standard` — the full pipeline (this document)
- `seeai` — competition / time-boxed delivery; alternate phase chain
  `research → docs → docs_confirm → spec → build_fullstack → polish → handoff`

Profiles **MUST NOT** weaken any MUST clause in §3–§6.

## 9. Reference implementation

The UmaDev repository at <https://github.com/umacloud/umadev>
ships a reference injector + orchestrator + verifier for this
specification as a **single pure-Rust binary** (`umadev`). The
workspace is ten crates:

| Crate | Role |
|---|---|
| `umadev` | The binary — clap CLI + the `tui` subcommand |
| `umadev-spec` | This specification as Rust data (clauses, phases, gates) |
| `umadev-governance` | Every enforceable rule in §3 / §6 — fail-open |
| `umadev-agent` | The team engine — intent router + owned plan DAG + a coordinator seat that schedules the role team (PM, architect, designer, frontend, backend, QA, security, DevOps) step by step + firmware injection, with the full commercial phase chain as its deepest play; gate semantics, role-critic team, trust tiers, runtime/deploy/review evidence |
| `umadev-runtime` | Runtime trait + OfflineRuntime + RuntimeKind (the host drivers impl Runtime; UmaDev owns no HTTP/model endpoint) |
| `umadev-host` | Drives a logged-in `claude` / `codex` / `opencode` CLI as a subprocess |
| `umadev-knowledge` | Structured BM25 + CJK retrieval over the curated `knowledge/` corpus |
| `umadev-contract` | Machine-verifiable frontend↔backend API contract (UD-CODE-003) |
| `umadev-tui` | A ratatui terminal app over the engine event stream |
| `umadev-i18n` | Trilingual (zh-CN / zh-TW / en) string catalogs + locale detection |

### 9.1 Execution modes

The reference implementation drives a turn through the team runtime of §9.5
— a coordinator seat routing it, and for a full commercial build expanding the
plan into the §4 phase chain — with one of two interchangeable backends. The
choice does **not** affect which clauses fire, only where the generative
work happens:

| Mode | Selector | Needs an API key |
|---|---|---|
| **Base CLI** | `--backend claude-code` / `--backend codex` / `--backend opencode` | No — drives the user's logged-in base CLI, reusing its own model + reasoning effort (UmaDev imposes neither) |
| **Offline** | (default / internal CI + no-base fallback) | No — deterministic templates |

UmaDev owns no model endpoint and connects no third-party API itself: a base
that the user has pointed at a third-party / local model simply runs with that
model. UmaDev reads and displays the base's model + reasoning effort (it never
overrides them) so the user always sees what is driving the Agent.

### 9.2 Governance hook entry

All four enforceable layers converge on one command surface —
`umadev hook <name>` — invoked by the host's pre/post-write hooks:
`check-emoji` (`UD-CODE-001`), `check-color` (`UD-CODE-002`),
`audit-api` (`UD-CODE-003` + `UD-EVID-001`), `tool-audit`
(`UD-EVID-002`), `inject-context` (`UD-FLOW-006`).

The reference implementation is one realization. Hosts MAY implement
this spec independently; conformance is judged by the spec, not by use
of the reference code.

### 9.3 Continuous-session driving model

The reference implementation drives a base CLI as **one long-lived,
stateful session** for the lifetime of a run, not as a fresh single-shot
subprocess per phase. This is a property of the *reference driver*, not a
new normative clause: every clause in §3–§6 fires identically whether the
phases are driven over a persistent session or over per-phase single
shots. The phase chain (`UD-FLOW-001`), the confirmation gates
(`UD-FLOW-002` / `UD-FLOW-003`), session continuity (`UD-FLOW-006`), and
the evidence chain (§6) are all defined on the *order of phases and the
artifacts they leave on disk* — never on the wire mechanism underneath.

In the reference driver the model is this:

- **One session = the team's shared working context.** A run opens a
  single base session, hands it the `research` directive, and then drives
  the *same* session through `docs`, `spec`, `frontend`, `backend`,
  `quality`, and `delivery`. The base keeps the accumulated context across
  phases instead of being re-primed from cold nine times. The confirmation
  gates (`UD-FLOW-002` / `UD-FLOW-003`) are natural pause points: after a
  turn completes the driver simply does not send the next directive until
  the user approves — the session parks with its context intact and resumes
  on confirmation.
- **The base does all cognition; the reference shell does only tooling.**
  Reasoning, research, design, writing code, and review are the base's
  work, observed as a stream of tool-call and text events. The shell layer
  is deterministic orchestration: advancing phases, staging gates,
  enforcing governance at the tool-call boundary, writing the audit chain
  (`UD-EVID-002`), applying the hard reversibility floor (`UD-FLOW-008`),
  and running the role-critic team (`UD-FLOW-007`). A run's *truth* is the
  set of tool calls and the files they leave on disk, not the prose the
  base narrates — the "no chat-only completion" rule (`UD-ART-006`) is
  enforced against the filesystem, and runtime evidence (`UD-EVID-006`)
  proves the result actually boots.
- **All intents share the session.** A casual question, an ad-hoc
  read/inspect/small-fix task, and a full pipeline run are not three
  separate code paths but three ways the coordinator steers the *same*
  team session. The base classifies which one a turn is and the shell drives
  accordingly; only work that mutates the workspace takes the single-writer
  run lock and the full gate machinery, while chat and read-only review do
  not.
- **Single-shot is a fail-open fallback.** The per-phase single-shot path
  (`claude --print` / `codex exec` / `opencode run`) is retained only as a
  degradation route: when the base session cannot start, when the brain is
  the offline runtime, or when an operator explicitly opts out. A driver
  that cannot open a continuous session **MUST** degrade rather than wedge,
  consistent with the fail-open posture required everywhere else in this
  spec.

### 9.4 The team-of-roles collaboration model

The role-critic team required by `UD-FLOW-007` is realized in the
reference implementation as a **full project team** — product manager,
architect, UI/UX designer, frontend, backend, QA, security, and DevOps,
each a seat doing its own specialty on a shared blackboard. The everyday
mental model is the *team*, not one seat: the team decomposes the
requirement, builds it, and reviews it. A **coordinator** seat (the
team's technical lead) schedules those seats and signs off at each gate;
it does not type code and it is not the headline — it is the glue. Each
seat is a *persona* that drives the borrowed base brain from that seat's
point of view; none is a hand-coded heuristic. The team obeys the four
hard invariants of `UD-FLOW-007` and is organized along two axes:

- **Doing roles vs. reviewing roles.** Doing roles (the frontend and
  backend engineers, the PM writing the spec) drive the *main* session
  **serially**, because writers share one workspace and cannot be
  parallelized without their implicit decisions colliding. Reviewing roles
  (PM, architect, designer, QA, security, backend, frontend, DevOps
  critics) each run on their **own read-only forked session** and therefore
  review **in parallel** — they never write, so they never conflict.
- **Communication is a shared blackboard plus structured verdicts, never
  free-form chat.** Roles do not converse with each other (cross-talk
  amplifies hallucination and never converges). They exchange exactly two
  things: the shared blackboard — the artifact files of §5 and the source
  tree on disk — which doing roles write and reviewing roles read; and the
  structured verdict each reviewer returns to the director (overall accept
  plus a list of blocking findings, advisory notes, and concrete
  evidence). Every verdict is appended to a team ledger as audit-grade
  evidence alongside the §6 chain.
- **The director aggregates deterministically and drives bounded rework.**
  At each gate the director collects the reviewers' verdicts together with
  the deterministic floor — FR→task coverage, the frontend↔backend contract
  check (`UD-CODE-003`), the governance scan, the verify/runtime result
  (`UD-EVID-006`), and the always-on hard gates. Loop control is a pure
  function of that deterministic floor: a non-deterministic critic opinion
  is advisory and **MUST NOT** drive termination (`UD-FLOW-007`). Blocking
  findings are folded into a single rework directive injected back into the
  main session, the affected artifacts are revised in place
  (`UD-ART-005` / `UD-FLOW-004`), and the gate is re-staged. Rework is
  **bounded**: a gap counter plus a stall counter terminate the loop
  deterministically rather than asking the base whether the result is "good
  enough."
- **The team scales with task complexity.** Trivial work (a bugfix or a
  refactor) convenes no team — the deterministic floor stands alone. A
  greenfield build convenes the full roster. This keeps the cost of the
  cross-review proportional to the risk of the change, and matches the
  lightweight path allowed for simple requirements (the spec profile of
  §8.4 governs the time-boxed `seeai` variant).

### 9.5 Team turn model — route, plan, schedule, deliver (coordinator-scheduled)

This section describes the **canonical shape of the reference
implementation's runtime**. It is **non-normative**: it adds no clause and
changes no clause. It documents *how* the reference driver decides, for any
given turn, whether and how deeply to engage the §4 flow contract — and it
reconciles the everyday product behaviour with the phase chain of
`UD-FLOW-001`.

The mental model is **firmware over a borrowed brain**. UmaDev owns no
model and does not re-implement the base's agentic loop. It borrows the
base brain to **think** (route, plan, judge) and directs the base body to
**work** (write code, run, fix), driving the base deterministically against
**typed artifacts UmaDev owns**. Every brain consult is fail-open to a
deterministic floor and never blocks the host. A turn flows through up to
five layers:

- **L0 — Firmware injection (every path).** Before any base turn, the
  reference driver composes a curated, token-budgeted system prompt —
  identity + engineering craft / anti-AI-slop taste + just-in-time
  knowledge digest + learned-pitfall recall + a repo-map slice of the
  user's existing code — and injects it through each base's *own* native
  system-prompt surface. The base executes; UmaDev declares policy. This is
  the layer that makes a UmaDev turn behave like a senior delivery team
  rather than a bare base; governance (§3 / §6) remains the silent floor
  under it.
- **L1 — Intent router.** The coordinator seat classifies the turn into a
  typed route — its class (chat / explain / quick-edit / debug / build), its
  task kind, the depth warranted, and the team to convene — using a
  deterministic tier as the floor and an optional cheap forked brain consult
  to refine it (the brain may escalate depth, never silently de-scope below
  the safe deterministic floor). The route is **surfaced to the user** so
  the decision is legible ("small change, on it" vs "full build, entering
  the delivery flow"), and the user can override it.
- **L2 — Owned plan + scheduling.** For a deliberate build the driver asks
  the brain for a strict plan it **parses and owns** as a dependency DAG of
  steps, each with a mechanical acceptance check; the plan is persisted, and
  rendered as a live, steerable checklist. Scheduling obeys the
  single-writer / map-reduce-manage doctrine of §9.4: doing-roles drive the
  main session serially under the run lock; reviewing critics run on
  parallel read-only forked sessions (`UD-FLOW-007`).
- **L3–L5 — Drive, verify, learn.** The driver walks the plan step by step,
  verifying each step against its acceptance on the deterministic floor
  (coverage / contract / verify / hard gate), self-correcting blocking
  findings with a typed, evidence-bearing rework directive, and exiting
  cleanly when stuck rather than spinning. Delivery artifacts (§5) and the
  proof pack (§6) are produced once the floor is clean, and the run's
  episodes feed the self-evolving memory.

**Relation to `UD-FLOW-001`.** The §4 phase chain is the **deepest play the
coordinator routes the team into** — its plan for a full commercial greenfield build
*expands into* `research → docs → docs_confirm → spec → frontend →
preview_confirm → backend → quality → delivery`, with the gates of §4.2 /
§4.3 honored exactly. The chain is therefore canonical for that build, but
it is reached *by routing and planning*, not imposed on every turn: a chat
turn never enters it, a quick edit takes the fast path, and a bugfix
convenes no team (§9.4). This is the same scoping `UD-FLOW-001` now states
in §4.1. The `standard` profile of §8.4 *is* the full chain; the `seeai`
profile is its time-boxed variant; both remain plays the router can select.

## 10. Future work (V2 candidates)

Items considered for the V2 promotion to `MUST`:

- `UD-CODE-005` — accessibility token enforcement (alt text, aria-label, focus order)
- `UD-EVID-009` — model provenance trail (which model + version generated which lines)
- `UD-META-005` — remote audit endpoint (host pushes audit logs to an external evaluator)

These are explicitly **non-normative** in V1.

(The former V2 candidates `UD-FLOW-007` — role-critic team — and the role/
runtime evidence ideas have shipped and are now normative in §4.7 and §6.6.)

## Appendix A — Reserved keywords

The phase identifiers `research`, `docs`, `docs_confirm`, `spec`,
`frontend`, `preview_confirm`, `backend`, `quality`, `delivery`,
`build_fullstack`, `polish`, `handoff` are reserved and **MUST NOT** be
redefined by a conformant host.

The gate identifiers `docs_confirm`, `preview_confirm` are reserved.

The clause-ID prefix space `UD-*` is reserved for this specification.

## Appendix B — Change log

| Version | Date | Notes |
|---|---|---|
| 1.0.0-draft.1 | 2026-05-20 | Initial draft. Layers L1–L4 codified from the in-repo governance core and integration manager. |
| 1.0.0-draft.2 | 2026-05-22 | §7 host map narrowed to the three official SDK families; §9 rewritten for the Rust reference implementation (three execution modes, TUI). No normative clause changed. |
| 1.0.0-draft.3 | 2026-06-22 | Promoted shipped capabilities to normative clauses: `UD-FLOW-007` (role-critic team), `UD-FLOW-008` (trust tiers + irreversibility floor), `UD-ART-007` (PR artifact), `UD-EVID-006/007/008` (runtime / deploy / review-report evidence). §9 crate table updated to the ten-crate workspace; manifest `declared_by` synced to `umadev@1.0.x`. |
| 1.0.0-draft.4 | 2026-06-23 | Added §9.3 (continuous-session driving model) and §9.4 (team-of-roles collaboration model) describing how the reference implementation drives one long-lived base session per run and realizes `UD-FLOW-007` as a director-led team over a shared blackboard. **Non-normative**: no clause added, changed, or renumbered — both sections describe the reference driver and cite only existing clauses. |
| 1.0.0-draft.5 | 2026-06-24 | Added §9.5 (director-driven turn model: route → plan → schedule → deliver) describing how the reference implementation decides whether/how deeply to engage the flow contract per turn, and clarified the **scope** of `UD-FLOW-001` in §4.1: the phase chain is the `standard`-profile *full commercial build* — the deepest play the directing Agent routes to and its plan expands into — not a funnel every turn is forced through. Reconciles the everyday director/router/plan product behaviour with the normative chain. **Non-normative**: no clause added, changed, renumbered, or weakened — `UD-FLOW-001` stays a MUST for the build it governs; §9.5 and the §4.1 scoping paragraph cite only existing clauses. |
| 1.0.0-draft.6 | 2026-07-13 | Added §3.6 `UD-CODE-006` (architecture-fitness floor: god-file gate, architecture-doc layer-dependency rules, added-code clone advisory), matching the shipped `umadev_agent::arch_fitness` gate. The `-005` slot in the `UD-CODE-*` family stays reserved for the §10 accessibility candidate, so the family numbering skips to 006. No existing clause changed, renumbered, or weakened. |
