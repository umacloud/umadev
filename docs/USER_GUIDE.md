# UmaDev User Guide

> Authoritative product spec: [`../spec/UMADEV_HOST_SPEC_V1.md`](../spec/UMADEV_HOST_SPEC_V1.md).
> This guide is the user-facing walkthrough; where they differ, the spec wins.

## What is UmaDev?

UmaDev is **a coding agent that works like a real dev team** — product manager, architect, UI/UX designer, frontend, backend, QA, security, and DevOps, each doing its own specialty on a shared blackboard. It is *firmware over a borrowed brain*. A **coordinator** seat (the team's technical lead) drives your already-configured base CLI. Exactly five receive first-class, deep integration: **Claude Code, Codex, OpenCode, Grok Build, and Kimi Code**. The first three use vendor-specific protocol drivers; Grok Build and Kimi Code use official ACP v1 interfaces through a hardened core and isolated vendor profiles. These are peer implementation paths, not support tiers. The coordinator uses that base to **route** your request, show you a **plan**, inject the team's **firmware** (engineering craft + anti-AI-slop taste + your project's knowledge + a map of your existing code), **schedule the team** step by step with a deterministic acceptance floor, and hand back a **proof-pack**. (Want a different model? Configure it inside the selected base; UmaDev owns no provider SDK or model endpoint.)

UmaDev itself doesn't write code: the brain stays in the base. The team decides what to produce and how deeply to engage, checks the result against a floor that can say no, and leaves an evidence trail. It owns **no model endpoint** and does not re-implement the base's agentic loop. Governance (no emoji icons, no hardcoded colors, no AI-template slop, contract alignment) is the safety net underneath the team.

## Quick Start

```bash
# 1. Install
npm install -g @umatech/umadev

# 2. Initialize a project
cd my-project
umadev init

# 3. Launch the TUI
umadev
```

On first launch, pick one of the five base CLIs and complete that vendor's own login or provider setup before using it. UmaDev reuses the resulting local configuration; it neither performs the login nor stores the credential. Then type your requirement and press Enter.

UmaDev does not use a base CLI version allowlist. `--version` proves only that
the selected program can be launched and supplies diagnostics. Authority comes
from the live protocol, the selected permission profile, and fail-closed runtime
rules. Unsupported optional features degrade individually instead of refusing
the whole base.

### Base capabilities and platform boundaries

All five bases below receive first-class lifecycle integration. Three drivers
use vendor-specific protocols; Grok Build and Kimi Code use official ACP v1
JSON-RPC/stdio interfaces through one bounded core and isolated launch,
authentication, permission, resume, and capability profiles. ACP is not a
fallback tier.

At the product layer, all five run as persistent, bidirectional sessions. The
UmaDev TUI remains interactive while base questions, approvals, tool events, and
follow-up turns cross the same live session. “Headless” and “non-interactive” in
the table describe only the background machine-protocol child (which does not
render the vendor's own TUI), never a one-shot prompt or a non-interactive user
experience.

| Base id | Transport | Login/configuration before UmaDev | Permission mapping | Cross-process resume | Vendor CLI platform note |
|---|---|---|---|---|---|
| `claude-code` | Vendor-specific bidirectional `stream-json` | Complete Claude Code login/setup in `claude` | Plan=`plan`; Guarded=`default`; Auto=`bypassPermissions`; UmaDev still keeps its irreversible-action floor | Exact `--resume <id>` | Use a platform supported by the installed Claude Code CLI |
| `codex` | Vendor-specific `codex app-server` JSON-RPC | `codex login` or Codex's own supported credential configuration | Plan=`read-only`; Guarded/Auto default to `danger-full-access`, with on-request/pre-authorized approvals respectively; an explicit project sandbox may only narrow them | Exact `thread/resume` | Use a platform supported by the installed Codex CLI; inherited enterprise/parent sandboxes can still restrict local ports or network |
| `opencode` | Vendor-specific loopback HTTP + SSE | `opencode auth login` or OpenCode provider configuration | Plan deny-by-default (including delegation); Guarded ask-by-default; Auto allow, with tool events still audited | Exact persisted-session reattach after permission refresh | Any installed version; use a platform supported by that OpenCode CLI |
| `grok-build` | ACP v1 via `grok --no-auto-update … agent stdio`; bounded firmware uses the official `--rules` common flag | Headless ACP reuses an existing cached login token or `XAI_API_KEY`; after `initialize`, UmaDev authenticates only with an explicitly available non-interactive method and never auto-selects OAuth or opens a browser | Plan adds `plan` + read-only sandbox + read-only tools and disables subagents; Guarded uses prompts; Auto explicitly pre-approves | Fresh-session handoff today; negotiated resume/load is used only after effective-sandbox attestation and native preflight can both be proved | Any official version; official installers cover macOS/Linux/WSL and native Windows PowerShell |
| `kimi-code` | Official `kimi acp` v1 JSON-RPC/stdio with live capability negotiation | Run `kimi login` yourself first; UmaDev only revalidates the on-disk token through ACP and never runs a login command or opens a browser | Plan=`plan`; Guarded/Auto keep Kimi `default`; UmaDev locally resolves ordinary Auto approvals, retains the irreversible floor, and renders Kimi's question-over-permission bridge as real choices | Standard `session/resume`, with advertised `session/load` fallback; workspace/profile identity mismatch fails closed | Any official version; Windows tools require Git Bash or `KIMI_SHELL_PATH` |

ACP stabilized `session/resume` in April 2026 and distinguishes it from
`session/load`: resume reconnects context without replaying history, while load
restores and streams history. UmaDev therefore negotiates the two capabilities
separately instead of treating either method as universally available. See the
[official ACP announcement](https://agentclientprotocol.com/announcements/session-resume-stabilized).

ACP also stabilizes `session/close`. UmaDev sends it before process cleanup only
when the live vendor handshake advertises `sessionCapabilities.close`; old or
unresponsive agents fall through to the same bounded reap path. The audited
Kimi adapter does not advertise close, so its child is shut down through the
bounded process-tree path.

### Structured input and live steering

Attachments are ordered typed blocks, not hidden `@path` text. Claude Code and
Codex accept images natively and accept a generic file only when the user
explicitly chooses bounded UTF-8 text materialization. OpenCode sends images and
files as native file parts. Grok Build and Kimi Code send images or embedded resources only
when that installed ACP agent advertised the corresponding content capability;
otherwise the block is rejected before the file is read. One turn is limited to
32 blocks, 16 attachments, 8 MiB per attachment, and 20 MiB total. Symlinks,
non-regular files, extension/magic mismatches, and files that change while being
validated are rejected.

Only Codex currently exposes a proven same-turn typed steer (`turn/steer`). On
Claude Code, input typed while work is active is queued for the next turn. On
OpenCode it is not presented as guaranteed same-turn steering, and each ACP
profile reports steering unsupported unless a future negotiated protocol
surface proves otherwise. The TUI shows the actual per-block delivery receipt
(`native`, `materialized text`, or `unsupported`) and never turns a rejected
attachment into a silent textual reference. It also distinguishes a flushed
transport frame from an exactly correlated vendor acknowledgement; neither
means the model has started or completed the work. A path the user manually writes as
an `@mention` remains plain text and is labelled as a reference, not as an
uploaded attachment.

UmaDev's own prebuilt binary supports macOS (Apple Silicon/Intel), Linux
(x86_64/ARM64, glibc >= 2.31 or musl/Alpine), and Windows x86_64; Windows on ARM runs the x64
binary through OS emulation. That does not manufacture support in a vendor CLI:
both `umadev` and the selected base must be available in the same environment.

`umadev install --base ...` installs a UmaDev governance integration: Claude
Code project hooks, source-audited Kimi Code hooks scoped to the exact project
root, or the repository pre-commit hook. It does **not** install, update,
authenticate, or license any of the five base CLIs.

For Grok Build, run any interactive `grok login` flow yourself before starting
UmaDev, or set `XAI_API_KEY` for headless use. UmaDev does not initiate OAuth,
open a Google/xAI login page, or copy an interactive authorization code.

For Kimi Code, install the official current release with
`npm install -g @moonshot-ai/kimi-code`, run `kimi login` yourself, then
start UmaDev. On Windows, install Git for Windows or point `KIMI_SHELL_PATH` at
`bash.exe`. UmaDev sets `KIMI_CODE_NO_AUTO_UPDATE=1` on the ACP child so the
audited binary cannot replace itself during a session.

If `kimi --version` prints `kimi, version 0.53` (with a comma), PATH is still
resolving the retired Python `kimi-cli`, not Kimi Code. Run `which -a kimi` on
macOS/Linux or `where kimi` on Windows, remove/reorder the legacy entry, or set
`UMADEV_KIMI_BIN` to the official executable. UmaDev reports this collision
before starting ACP instead of attempting to drive the incompatible command.

The full-screen TUI requires both terminal stdin and terminal stdout. If either
stream is piped or stdout is redirected (for example, `umadev > output.txt`),
UmaDev prints CLI help and never enables raw mode or writes terminal control
frames to the redirected file.

Ordinary conversation is model-routed before any writer acts. UmaDev asks the
selected base model on a fresh read-only child session whether the turn is Chat,
Explain, QuickEdit, Debug, or Build and how deeply it should run. The model may
both scale work up and recognize that code-like or feature-like wording is only a
question; deterministic matching is a conservative fallback only when that typed
consult is unavailable. Chat/Explain stay on a read-only execution surface. A
model-routed write-capable turn still has to pass the run lock, trust mode, governance, and
irreversible-action confirmation rules. It must also carry the exact valid typed
authorization `mutating`; a missing, blank, or unknown authorization fails closed
to read-only Explain. Plan mode is always read-only and cannot be widened by a
model verdict.

In Plan mode, ordinary conversation can inspect the repository and produce a
read-only plan. Explicit execution commands such as `/run`, `/goal`, and an
execution-style resume/continue do not acquire a writer lock or start a base
writer; they return a non-executed planning result and never show a build as
Done. Switch to Guarded or Auto when you actually want the plan executed.

The execution mapping is deliberately small and predictable:

- **Chat / Explain:** read-only; no writer lock, Director, or QC.
- **QuickEdit / Fast Debug:** the resident single writer makes the smallest
  scoped change and runs targeted verification after the last code write; no
  role team or full QC. A write without an observed successful post-write check
  ends as Failed. The write alone does not turn the task into a Director run or
  produce a full-build completion card.
- **Every Build / Standard or Deep Debug:** enters the Director workflow, whose
  owned plan, gates, team and bounded QC scale to the request. A Fast Build may
  therefore still be a single lean step rather than the full nine phases.

Only a valid live model decision can enter the Director. If the model consult is
unavailable, times out, or returns invalid typed output, deterministic fallback
stays on the proportional resident path and cannot by itself start the Director,
a role team, or full post-build QC. It may recognize only an unmistakable,
explicitly scoped current-user request; it never inherits authority from a missing
or malformed model field. Conversation history, old plans, TODOs, run
notes and project documents are context only: the current request is the sole
authority for new work.

While a run is active, only an explicit correction to the current task is steered
into its writer. Questions and future or ambiguous tasks wait as ordinary turns
in FIFO order and are routed after the run settles, so asking “why?” cannot
silently become a revision. There is one deliberate gate-local exception: while
a confirmation gate is open, a question is answered immediately by a separate
read-only query; it does not approve, revise, or advance the gate. The gate only
becomes interactive after the writer session has finished its current boundary.

Cancelling an active run stops it, clears the vendor base-session resume hand-off,
and records a control boundary in conversation memory so the next turn cannot
silently continue the cancelled request. Already deferred FIFO turns remain
available for fresh routing.

## The 9-Phase Pipeline (the deep play, not every turn)

This chain is the **deepest play the coordinator routes to** for a full commercial greenfield build. A greeting stays chat; a one-line edit or fast, narrowly scoped bugfix takes the resident path; deeper debugging receives a proportional Director plan. Only a full product requirement expands that plan into this complete chain. (See the intent router in "What is UmaDev?" above, and spec §4.1 / §9.5.)

```
research → docs → ⏸ docs_confirm → spec → frontend → ⏸ preview_confirm → backend → quality → delivery
```

| Phase | What happens | Expert role |
|---|---|---|
| research | Competitive analysis, user discovery, design direction | Product Researcher |
| docs | PRD + Architecture + UI/UX design system | PM + Architect + Designer |
| docs_confirm | **GATE** — you review the 3 docs before coding starts | You |
| spec | Sprint breakdown, coding standards, task list | Engineering Manager |
| frontend | Base implements frontend with approved design tokens | Frontend Lead |
| preview_confirm | **GATE** — you review the frontend before backend | You |
| backend | Base implements API routes, database, auth, tests | Backend Lead |
| quality | 17 automated checks + 5-dimension visual review | QA Lead |
| delivery | Proof-pack zip with README + compliance mapping | Release Engineer |

> The 9 phases target a full commercial-grade delivery. Small tasks have a lighter path: the selected base model classifies the request and the coordinator trims or expands the plan to fit. A fast, narrowly scoped bugfix stays team-free; a deeper Debug may use the Director without being pushed through the whole PRD / architecture / UIUX chain. Force the light path for a trivial change with `/quick`.

## TUI Commands

### Base
| Command | Description |
|---|---|
| `/claude` | Switch to Claude Code CLI |
| `/codex` | Switch to Codex CLI |
| `/opencode` | Switch to OpenCode CLI |
| `/grok` or `/grok-build` | Switch to Grok Build CLI (ACP v1) |
| `/offline` | Offline templates — internal CI / no-base fallback, not a product mode |

### Design
| Command | Description |
|---|---|
| `/design` | Browse available design systems |
| `/design <name>` | Select a design system |
| `/template <name>` | Select a seed template |

### Pipeline
| Command | Description |
|---|---|
| `/continue` or `c` | Approve the current gate (Plan mode never resumes execution) |
| `/revise <text>` | Request changes at a gate |
| `/cancel` | Stop the active run without resuming it on the next turn |
| `/run [--slug <slug>] <req>` | Start a new run in Guarded/Auto; use `--slug` to override the project slug; Plan mode remains read-only |
| `/redo` | Re-run current requirement |
| `/diff <artifact>` | View an artifact (prd/architecture/uiux) |

### Inspect
| Command | Description |
|---|---|
| `/status` | Pipeline progress + quality score |
| `/export` | Export proof-pack |
| `/config` | View all settings |
| `/knowledge` | Browse knowledge files |
| `/doctor` | Self-test |
| `/verify` | Workspace conformance |

### General
| Command | Description |
|---|---|
| `/help` | All commands |
| `/clear` | Clear chat history |
| `/quit` | Exit |

## Design Systems

UmaDev ships 5 design systems. Select one before running to get deterministic visual output:

| Name | Best for |
|---|---|
| `modern-minimal` | SaaS, dev tools, dashboards |
| `editorial-clean` | Blogs, content sites, portfolios |
| `tech-utility` | CLI companions, monitoring, data tools |
| `soft-warm` | Consumer apps, education, wellness |
| `bold-geometric` | Brand launches, creative agencies |

## Seed Templates

| Name | Structure |
|---|---|
| `saas-landing` | Nav → Hero → Trust → Features → Pricing → FAQ → Footer |
| `dashboard` | Sidebar + KPI cards + Charts + Data table |
| `blog-content` | Featured article + Grid + Newsletter |
| `e-commerce` | Gallery + Product info + Variants + Reviews + Related |
| `auth-system` | Login + Signup + Forgot + MFA + Reset |
| `settings-page` | Sidebar tabs + Profile + Security + Billing |
| `docs-site` | Sidebar nav + Content + Code blocks + Search |

## Configuration

### `.umadevrc` (project-level)

```toml
[quality]
threshold = 85              # quality gate pass threshold (default: 90)
skip_checks = ["dark_mode"] # skip specific checks

[pipeline]
skip_phases = ["research"]  # skip phases you don't need
max_review_rounds = 2       # limit auto-fix cycles (default: 3)

[experts]
custom_knowledge = "team-standards/"  # additional knowledge directory
```

### `~/.umadev/config.toml` (user-level)

```toml
backend = "claude-code"
# umadev owns no model — the base runs on its own configured model.
# To change the model, change it in the base's own config (not here).
design_system = "modern-minimal"
seed_template = "dashboard"
```

## Quality Gate

Director completion is evidence-based. A blocked, active, pending, or otherwise
incomplete plan, dirty final QC, or findings left when the round/time budget is
exhausted finishes as **Failed with blocking evidence**, never Done. Only a clean
terminal plan receives the full completion surface. QuickEdit and Fast Debug use
their smaller post-write targeted-verification contract instead of Director QC.

UmaDev runs 17 automated checks:

| Category | Checks |
|---|---|
| Artifacts | Research, PRD, Architecture, UIUX — content structure validation |
| Cross-reference | PRD↔Architecture route alignment, API URL consistency |
| Code quality | Emoji check, hardcoded colors, anti-AI-slop patterns |
| Design | UIUX token count, dark mode presence, design system completeness |
| Evidence | Audit log, tool-call log, discovery section |
| Depth | Acceptance criteria count, API route count |

## Expert Knowledge

Each pipeline phase is backed by a specialist's methodology:

| Expert | Knowledge | Used in |
|---|---|---|
| Product Manager | RICE scoring, AC format, edge cases, HEART metrics | Research, PRD |
| Architect | API design standards, security checklist (OWASP), auth patterns | Architecture |
| UI/UX Designer | Token architecture, interaction principles, WCAG 2.1, responsive | UIUX, Frontend |
| Frontend Lead | Component architecture, state management, error handling, performance | Frontend |
| Backend Lead | API handler pattern, database practices, JWT flow, logging standards | Backend |
| QA Lead | Test pyramid, AC→test conversion, pre-release checklist | Quality |
| DevOps | CI/CD pipeline, Docker, monitoring, rollback strategy | Delivery |

## FAQ

**Q: Do I need an API key?**
No. UmaDev drives your already-logged-in AI coding CLI. It uses your existing subscription.

**Q: What if the base times out?**
UmaDev retries once. If it still fails, it falls back to an offline template with TODO markers. You can `/redo` to try again.

**Q: Can I customize the quality checks?**
Yes, via `.umadevrc`. Set `skip_checks` to disable specific checks, or `threshold` to change the pass score.

**Q: Does it work offline?**
Offline is a fallback, not the product. Without a base reachable, it generates structured templates with TODO markers — useful for planning, CI smoke tests, or demos, but not a substitute for real development. Real delivery always runs through one of the five bases.
