# UmaDev Host Specification, Version 1 (UMADEV_HOST_SPEC_V1)

> **Status:** Draft  
> **Version:** 1.0.0-draft.1  
> **Date:** 2026-05-20  
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
verifiable normative clauses. The user-facing CLI (`umadev install
<host>`, `umadev verify <host>`, `umadev audit`) is the coach
"handing over the playbook" and "watching from the sideline."

Hosts (Claude Code, Codex CLI, Cursor, Windsurf, Cline, Continue, Codex
App/Desktop, Droid CLI, Kiro, Trae, Qoder, Qwen Code, CodeBuddy, OpenCode,
Copilot Agent, Replit Agent, ...) are independent products. They MAY meet
this spec by adopting UmaDev's reference injectors, or by implementing
the rules natively. Both paths produce a conformant host.

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
- `HOST` — host surface mapping (§7)
- `META` — versioning and conformance declaration (§8)

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

## 7. Host surface mapping

Every clause in §3–§6 is layer-agnostic. This section is **non-normative
guidance** showing how each layer is realized on UmaDev's three
officially-supported host families. Other hosts MAY implement this spec
independently; the reference implementation (`umadev install <host>`)
ships plugin bundles only for the three families listed below.

### 7.1 Officially supported hosts

The reference implementation ships first-party plugin bundles for the
three host families that have an **official Agent SDK** as of
UMADEV_HOST_SPEC_V1's draft date (May 2026):

| Host family | Provider | SDK | Workspace install | User-scope install |
|---|---|---|---|---|
| **Claude Code** / Claude Desktop | Anthropic | Claude Agent SDK | `.claude/` | `~/.claude/` |
| **Codex CLI** / Codex Desktop | OpenAI | OpenAI Agents SDK | `AGENTS.md` + `.codex/` | `~/.codex/` |
| **Antigravity CLI** / Antigravity Desktop | Google | Antigravity SDK | `AGENTS.md` | `~/.antigravity/` |

Hosts outside this set (Cursor, Windsurf, Cline / Roo, Continue, Droid
CLI, Kiro IDE, Trae, Qoder, CodeBuddy, …) are explicitly **out of
scope** for the reference implementation. They MAY still implement this
specification by adopting the same `.umadev/` workspace layout and
the same hook commands; the reference plugin bundles will not be
shipped for them.

### 7.2 Reference surface inventory

| Surface | Claude Code | Codex CLI / Desktop | Antigravity CLI / Desktop |
|---|---|---|---|
| Persistent rules | `.claude/skills/umadev/SKILL.md`, `CLAUDE.md` | `AGENTS.md`, `skills/umadev/SKILL.md` | `AGENTS.md`, `skills/umadev/SKILL.md` |
| Slash command | `commands/umadev.md` | (host-defined; AGENTS.md guidance) | (host-defined; AGENTS.md guidance) |
| Pre-write hook | `hooks.PreToolUse` matcher `Write\|Edit` | `[[hooks.PreToolUse]]` in `.codex/config.toml` | TBD (Antigravity 2.0 hook contract is stabilising) |
| Post-write hook | `hooks.PostToolUse` matcher `Write\|Edit` | `[[hooks.PostToolUse]]` in `.codex/config.toml` | TBD |
| Prompt hook | `hooks.UserPromptSubmit` | `[[hooks.UserPromptSubmit]]` | TBD |
| Artifact dir | workspace `output/` | workspace `output/` | workspace `output/` |
| Evidence dir | workspace `.umadev/audit/` | same | same |

### 7.3 Reference clause → hook command

All three host families invoke the **same** `umadev hook <subcommand>`
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

Where a host lacks a surface required by a clause (e.g. Antigravity 2.0's
hook contract is not yet GA), the conformant injector MUST substitute
the next-best surface — for the Antigravity case, the bundled
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
  declared_by: umadev@4.4.0
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
workspace is seven crates:

| Crate | Role |
|---|---|
| `umadev` | The binary — clap CLI + the `tui` subcommand |
| `umadev-spec` | This specification as Rust data (clauses, phases, gates) |
| `umadev-governance` | Every enforceable rule in §3 / §6 — fail-open |
| `umadev-agent` | 9-phase pipeline runner, gate semantics, event stream |
| `umadev-runtime` | Runtime trait + OfflineRuntime + RuntimeKind (the host drivers impl Runtime; UmaDev owns no HTTP/model endpoint) |
| `umadev-host` | Drives a logged-in `claude` / `codex` / `opencode` CLI as a subprocess |
| `umadev-tui` | A ratatui terminal app over the engine event stream |

### 9.1 Execution modes

The reference implementation runs the §4 phase chain with one of two
interchangeable backends — the choice does **not** affect which clauses
fire, only where the generative work happens:

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

## 10. Future work (V2 candidates)

Items considered for the V2 promotion to `MUST`:

- `UD-CODE-005` — accessibility token enforcement (alt text, aria-label, focus order)
- `UD-FLOW-007` — multi-agent role declaration (PM / ARCHITECT / CODE / QA as named subagents)
- `UD-EVID-006` — model provenance trail (which model + version generated which lines)
- `UD-META-005` — remote audit endpoint (host pushes audit logs to an external evaluator)

These are explicitly **non-normative** in V1.

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
| 1.0.0-draft.2 | 2026-05-22 | §7 host map narrowed to the three official SDK families; §9 rewritten for the Rust 4.0 reference implementation (7 crates, three execution modes, TUI). No normative clause changed. |
