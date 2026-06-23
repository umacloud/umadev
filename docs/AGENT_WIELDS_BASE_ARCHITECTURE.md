# UmaDev = an Agent that wields the base — director orchestrates a team

> Target-state architecture + phased migration. This supersedes the
> fixed-pipeline framing in `CONTINUOUS_SESSION_ARCHITECTURE.md`. The core model
> here is **NOT a pipeline**. It is a **director Agent that improvises a team**.

> **Simplification update (current):** the clean mental model is **USB / smart
> hardware**. UmaDev is a smart device with its own FIRMWARE — a senior team-director
> identity, engineering taste, accumulated knowledge, governance, and memory — but
> **no compute of its own**. Plugged into a base (claude / codex / opencode) over the
> continuous session, it borrows the base's intelligence and hands. The base is
> already a complete Agent: its model is the brain, its CLI tools are the body that
> builds, writes, runs, tests, fixes. Once the firmware is injected, the base ITSELF
> plays PM / architect / frontend / QA internally and builds the goal end to end —
> so UmaDev does **not** need an outside "marker scheduling protocol" to summon a
> team. The earlier `<<<umadev:summon …>>>` marker channel (and any framing where
> UmaDev grows its own build/run/fix machinery) is **retired**. UmaDev is pure
> firmware + two tiny firmware-only acts: governance (a background safety net on the
> base's writes) and a read-only honesty check (read disk to confirm real code
> landed). What UmaDev runs after a build is its OWN read-only QC (honesty floor +
> optional forked review); any blocking finding is fed back as a fix directive the
> base's body acts on, bounded. See `crates/umadev-agent/src/director_loop.rs`.

## 0. The identity (the thing every design choice must serve)

UmaDev is **a third-party Agent with agency of its own** — it thinks, judges,
holds a goal. What it lacks is *hands*: it cannot read a file or run a command
itself. So it **shares a brain with the base** and **wields the base as its
weapon** to operate on the world.

- It is **NOT** "the base is the brain, UmaDev is a shell" — that strips UmaDev
  of agency.
- It is **NOT** a governance tool that bolts rules/gates/audit onto the base
  (that was the predecessor's framing).
- It **IS**: an agentic **project director** + a **team it can summon** + the
  base as a shared brain and the hands that hold the weapon + the team's
  knowledge/standards as its *craft* + governance as a *background safety net*.

Concretely, the relationship is **swordsman and sword**:

```
UmaDev (the Agent)   = the swordsman — has will, judgement, a goal, a team's craft
the base (claude/…)  = the sword — supplies the intelligence to think and the hands to act
doing work           = the swordsman wields the sword to get the goal done
```

## 1. The core model: a director improvising a team — not a pipeline

There is **no** `research -> docs -> gate -> spec -> frontend -> preview -> backend
-> quality -> delivery` fixed chain. Instead:

**A user gives a goal. The director Agent (thinking through the base) decides, on
the spot, how to get it done — who on the team to bring in, in what order, how
much process to apply — exactly as a real senior director would.**

- A trivial ask: the director may just have the frontend engineer do it, end to
  end, and sanity-check the result.
- A real product: the director may decide to have the PM frame requirements,
  the architect set the approach, split frontend/backend, then have QA + security
  vet it — **because the director judged that this goal needs it**, not because a
  state machine forced nine phases.

The "how" is the director's live judgement (via the base), re-evaluated as work
unfolds. The nine-phase flow becomes **one play the director may choose**, not
the only road.

### 1.1 The director's orchestration loop (dynamic, not staged)

```
loop {
    // 1. UNDERSTAND + PLAN (think through the base, as the director)
    //    "What is the goal really? What's the shortest credible path? Who do I
    //     need on the team, and in what order? How much process does THIS goal
    //     warrant?"  -> a lightweight, revisable plan of moves, not 9 fixed phases.

    // 2. DELEGATE a move to a team member (or do it on the main session)
    //    Summon a role: inject that role's identity + craft, drive the base to do
    //    that slice of work (serial), OR fork() parallel roles to work/review.

    // 3. OBSERVE the result (the base's tool calls = the truth of what happened)
    //    Background safety net runs on every file write (governance hook).

    // 4. DECIDE (the director judges, via the base):
    //    Good enough? Need rework? Bring in another role? Pause and ask the user?
    //    Objective check: did real artifacts actually get produced (hard-gate)?

    // 5. CONTINUE until the goal is met — then report honestly.
}
```

Loop control is the **director's judgement** (via the base) bounded by **objective
checks** (did code actually get written; does it build; does the contract align).
The base proposes; the deterministic floor only ever *verifies reality* — it never
dictates the route.

### 1.2 Summoning a team member (the unit of delegation)

A "team member" is the base, forked or driven, wearing a role's identity + craft:

- **Serial doer**: on the main session, the director injects the role
  (`experts::phase_persona` / role prompt + relevant knowledge) and drives a turn
  to produce work. Single-writer — only one doer mutates the workspace at a time.
- **Parallel reviewer/worker**: the director `BaseSession::fork()`s an isolated
  read-only (or scratch) session per role and runs them concurrently — the exact
  mechanism `critics.rs` already uses for review, generalized so the director can
  invoke it *whenever it judges useful*, not only at two fixed gates.
- **Round-trip**: a reviewer's `RoleVerdict { accepts, blocking[], advisory[],
  evidence[] }` (critics.rs:43) flows back to the director, who folds blocking
  findings into a rework move on the doer — **bounded** by the director's
  judgement plus a hard round cap (the proven `MAX_REWORK_ROUNDS` + stall shape).

The team roster (PM / architect / UIUX / frontend / backend / QA / security /
devops + director) already exists as `RoleCritic` impls (critics.rs:175+) and
persona prompts (experts.rs:549+). The change is **who decides when to use them**:
today a fixed loop triggers them at gate phases; in the target the **director
decides**, live.

## 2. The building blocks already in the tree (reuse, don't rebuild)

| Block | Where | Role in target |
|---|---|---|
| `BaseSession` (send_turn/next_event/fork/respond/interrupt/end) | `umadev-runtime/src/lib.rs:445` | The weapon interface. `fork()` (469) = summon a parallel team member. KEEP. |
| `RoleCritic` + `RoleVerdict` + role impls | `umadev-agent/src/critics.rs:43,139,175+` | Team members. TRANSFORM: director-summonable any time, not gate-bound. |
| Personas + craft (SPEC_PREAMBLE, ANTI_SLOP_LAW, phase_persona, agentic_team_identity) | `umadev-agent/src/experts.rs:44,67,549,636` | A role's identity + the team's taste. KEEP/reuse as capability injection. |
| Knowledge retrieval | `umadev-knowledge` + `phases::agentic_knowledge_digest` | The team's experience, retrieved on demand. KEEP as a director/role capability. |
| Governance PreToolUse hook | `umadev/src/hook.rs` + `umadev-governance` | Background safety net on every write. KEEP — runs under everything, not in the prompt. |
| Hard-gate (zero real source -> fail) | `continuous.rs:331`, `acceptance::source_files` | Objective reality check after a "build" goal. KEEP as a verifier, not a phase. |
| Quality / verify / contract / coverage | `continuous.rs:308,699`; `umadev-contract`; `coverage` | Objective checks the director can RUN to confirm "is it actually done/correct". TRANSFORM into director-callable verifiers, not mandatory gate. |
| Single-writer run-lock | `run_lock.rs`, `runner.rs:1419,1565` | Safety: one doer mutates at a time. KEEP. |
| Audit (UD-EVID-002) | `umadev-governance::audit` | Evidence trail of every tool call. KEEP. |
| Trust tiers (plan/guarded/auto) | `umadev-agent::trust` | Whether the director auto-proceeds or pauses for the user. KEEP, generalized: the director decides *when* a checkpoint matters, bounded by the tier. |
| `fire_agentic` (TUI default) | `umadev-tui/src/lib.rs:1265` | Already the director-shaped path. GROW into the full orchestration loop. |
| `continuous.rs` run_block + block_phases + 25 gate/stop points | `continuous.rs:161,1003` | The fixed pipeline. DEMOTE: its phases become "plays" the director may pick; the fixed walk is removed as the default. |
| `planner::TaskKind`/phases | `umadev-agent/src/planner.rs` | A heuristic hint the director may consult. DEMOTE from "decides the fixed phase list" to "advisory prior." |

## 3. Retain / Transform / Remove

**RETAIN (the floor — safety, not process):** `BaseSession` + `fork()`;
single-writer run-lock; governance hook (background); audit; hard-gate as an
*objective reality check*; fail-open everywhere; no model endpoint of our own;
trilingual i18n; the role roster + personas + knowledge + lessons as
*capabilities*.

**TRANSFORM (from fixed trigger to director-summoned):**
- `review_and_rework` / `run_review_team` (continuous.rs:1333,1414): from
  "auto-fires at the docs/preview/quality gate" to "a capability the director
  invokes whenever it judges review is warranted."
- `run_quality_gate` / `governance_catchup` / contract / coverage
  (continuous.rs:699,819,943): from "mandatory phase steps" to "verifiers the
  director runs to confirm reality when a goal claims to be done."
- `planner` (planner.rs): from "decides the phase list" to "an advisory prior the
  director may read."
- Role personas (experts.rs): from "tied to a fixed phase" to "injected whenever
  the director assigns that role a move."

**REMOVE (the rigidity itself):**
- The fixed `block_phases` walk (continuous.rs:1003) as the **default** route for
  a goal, and the 25 hard gate/stop branches *as a forced chain*. (The code can be
  kept behind an explicit "run the full commercial play" opt-in during migration,
  then retired — see Wave 4.)
- The two-engine split (TUI `fire_agentic` vs `spawn_continuous_block`,
  lib.rs:1265 vs 522; CLI `cmd_run` -> continuous). Collapse into ONE
  director-driven engine; `/run` becomes "the director, told to treat this as a
  full commercial build," not a different engine.

## 4. The director's prompt + UmaDev's own QC (the simplified USB model)

The director is the base driven by an injected **firmware** — identity + craft —
and nothing else taught to it as a protocol:

- Identity: `experts::agentic_team_identity` — "You ARE UmaDev, a senior director
  leading a team; YOU decide the plan, who to bring in, and how much process this
  goal needs."
- Craft / taste: `experts::agentic_engineering_rules` (ANTI_SLOP_LAW distilled —
  no emoji icons, a real icon library, design tokens, clean layering) injected as
  the team's *taste*, not a MUST-NOT list; governance enforces the floor silently.
- Knowledge: the requirement-scoped digest, retrieved on demand.

`experts::director_with_team_tools` composes exactly this firmware (identity +
craft) for a `/run` build turn. The base is **not** taught any marker / lever
syntax — it builds end to end with the team inside its own head.

UmaDev's four levers (`summon` / `review` / `verify` / `checkpoint`) remain as
**internal Rust capabilities** in `director.rs` — but they are UmaDev's OWN calls,
not base-facing tools. After the base reports a build, `director_loop.rs` runs an
UmaDev-side QC pass:

- `verify(source-present)` — UmaDev reads disk to confirm real code landed (the
  tiny deterministic honesty floor). Zero source after a claimed build is decisive.
- `verify(build-test)` — an optional fact-read when a manifest is present; the FIX
  (and re-running build/test) is the base's body's job, asked via the fix directive.
- `review(quality)` — fork the cross-review team on read-only sessions; advisory
  blocking findings seed the fix directive.
- `checkpoint(question)` — pause for the user when the decision is theirs (trust
  tier). Retained as a capability for any caller that needs it.

Blocking findings fold into ONE fix directive fed back over the same session; the
base's body fixes with its own tools; re-QC. Bounded by `MAX_QC_ROUNDS`. No phase
enum drives it; UmaDev grows no "operating" machinery of its own.

## 5. Phased migration (incremental, each wave ships + verifies + reverts)

Goal: never tear down 3283 lines at once. Each wave is independently shippable,
keeps the floor intact, and is reversible behind a flag.

**Wave 1 — One engine.** Route `/run` and `Action::StartRun` through the same
director-driven agentic path as default input, instead of `spawn_continuous_block`.
Keep the full pipeline reachable behind an explicit opt-out
(`UMADEV_LEGACY_PIPELINE=1`) so nothing is lost.
Files: `umadev-tui/src/lib.rs` (522,1864), `umadev/src/main.rs` (cmd_run 2310).
Verify: a "build me X" goal runs through the director loop end to end; objective
hard-gate still fires; legacy flag still reaches the old pipeline.
Revert: flip the flag default.

**Wave 2 — Director tools.** Expose `summon` / `review` / `verify` / `checkpoint`
to the director (reusing `fork()`, `run_review_team`, `run_quality_gate`,
`source_files`, trust tiers under the hood). Director prompt upgraded to plan +
delegate.
Files: new `umadev-agent/src/director.rs` (orchestration tools over existing fns),
`experts.rs` (director prompt), `umadev-tui/src/lib.rs` (wire tools into the loop).
Verify: director can, on its own judgement, summon a role / run a review / run a
verify; each tool is fail-open; single-writer preserved.

**Wave 3 — Demote the planner + phases to advisory.** `planner` becomes a prior
the director may read; `phase_persona`/role prompts become role capabilities the
director injects per move. The fixed `block_phases` walk is no longer the route.
Files: `continuous.rs` (carve `run_block`'s loop out; keep the *capabilities*,
drop the *fixed walk*), `planner.rs` (advisory API).
Verify: simple goal -> director does it directly (no forced research/docs);
complex goal -> director chooses to bring in PM/architect/QA itself.

**Wave 4 — Retire the legacy pipeline.** Once the director loop is proven on real
bases across simple + complex goals, remove the `UMADEV_LEGACY_PIPELINE` path and
the dead fixed-walk code. Keep every transformed capability.
Files: delete the fixed-walk remnants in `continuous.rs`; update spec prose.
Verify: full workspace tests + real-base smoke (simple page in minutes; a real
product orchestrated by the director with the team it chose).

**Throughout:** floor invariants hold every wave (single-writer, governance hook,
audit, hard-gate reality check, fail-open, no endpoint). `cargo clippy --workspace
-- -D warnings` + `cargo test --workspace` + Windows cross green per wave.

## 6. What "done" looks like

- One engine: every goal — a hello, a code review, a full product — is the same
  director Agent wielding the base, differing only in how the director chose to
  orchestrate.
- No phase enum decides the route; the director does, live, and can bring in any
  team member in any order, serial or parallel, with rework as it judges.
- Knowledge/standards/governance are the director's craft + a silent safety net,
  never a forced funnel.
- The objective floor still guarantees honesty: if the director says "built it"
  but no real source exists, the hard-gate reality check says so.

The product stops being "a governance pipeline you feed a requirement" and becomes
"**a senior director Agent that wields the base to get your goal done, summoning
exactly the team the goal needs.**"
