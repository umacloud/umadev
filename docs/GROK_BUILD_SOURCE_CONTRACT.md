# Grok Build source contract

> Status: rolling source audit plus version-independent runtime adaptation.
>
> This document records the source-level contract that UmaDev must satisfy for
> its Grok Build base. A behavior described in the official source is not
> automatically an implemented UmaDev behavior. The status tables below use
> **Implemented**, **Partial**, and **Pending** deliberately; only an acceptance
> test may move a row to Implemented.

## 1. Audited upstream baseline

| Item | Audited value |
|---|---|
| Official repository | <https://github.com/xai-org/grok-build> |
| Audited commit | `47348d13ec4508dcfe440e34c6d511bb02998fb2` |
| Grok Build version | `0.2.112` |
| `agent-client-protocol` | `0.10.4`, with upstream's `unstable` feature |
| `agent-client-protocol-schema` resolved by upstream | `0.11.4` |
| ACP wire protocol negotiated by this build | V1 |
| Audit date | 2026-07-26 |

The version is declared by the upstream `xai-grok-pager`,
`xai-grok-pager-bin`, `xai-grok-shell`, and related crates. The ACP dependency
is pinned in the upstream workspace `Cargo.toml`; the resolved schema version is
recorded in its `Cargo.lock`.

This commit is the latest source snapshot used to detect upstream drift. It is
not a runtime version gate. Every official peer reporting `grokShell: true` may
run; standard features come from ACP advertisement/session state, and private
messages pass only typed, bounded parsers with method-level fallback.

The audited `0.2.112` release artifacts are retained by content
as cross-platform regression fixtures, rather than trusting a mutable installer:

| Official artifact | SHA-256 |
|---|---|
| `grok-0.2.112-linux-x86_64` | `c2867112f7d89366123fe68a55a23dfb027d3602fc5b5b9cd5c080dacb4a2503` |
| `grok-0.2.112-linux-aarch64` | `d21f1aaaba7f2930db0ef7d5a9dc3f814a94c54af208e091f72a239cac02ba39` |
| `grok-0.2.112-macos-aarch64` | `5cf05fe670b1818561daf7566b580a5de6b81149166499d61072e49640b541a4` |
| `grok-0.2.112-macos-x86_64` | `9d00be3e3e28d1ce3c36f26d65c40aac2623917aabc6a20b3bda21a269d8ea24` |
| `grok-0.2.112-windows-x86_64.exe` | `2469bd182af212c7fcb84f2981999e4e8a6a7a2e4172bad3ae7f787a1f11407c` |

Ordinary and tag CI download the exact Linux x64, macOS arm64 and Windows x64
artifacts directly from `https://x.ai/cli/`, verify these digests and the exact
`grok 0.2.112 (<revision>)` version line, then run an isolated real-process ACP
handshake. The test requires the typed authentication offer and proves that no
browser-capable authentication RPC or project write occurs before explicit
confirmation. The other two hashes lock the official artifact bytes for the
architectures UmaDev can encounter even though GitHub-hosted runners cannot
execute those foreign-architecture binaries.

The previous audit point was `8adf9013a0929e5c7f1d4e849492d2387837a28d`
(`0.2.101`). Versions 0.2.102 through 0.2.106 substantially changed
authentication single-flight/cancellation, prompt admission and background
wake handling, permission-path normalization, model defaults, MCP setup, and
session-state transfer. ACP remains `0.10.4` with schema `0.11.4`, but that fact
alone is not treated as private-wire compatibility. The method inventory,
source-shaped DTO markers, exact artifacts, and isolated real ACP handshake
were rerun for this pin. New optional client-driven methods such as
`x.ai/mcp/setup` and `x.ai/session/import` are not advertised as UmaDev features
merely because the vendor pager uses them.

The latest source re-audit additionally compared `0.2.109` through `0.2.112`.
The public ACP version and schema remain unchanged. Interjection, lifecycle
notifications, turn completion, permission choices and authentication ordering
remain wire-compatible. Subagent execution moved behind a coordinator actor and
the task snapshot gained an optional description, but the external
`x.ai/subagent/*`, `x.ai/task/*` and `_x.ai/session/update` contracts retained
their typed fields and cancellation/reconnect semantics. The drift tests pin the
new coordinator route so a later internal refactor cannot silently weaken
resynchronization.

## 2. Contract vocabulary and gates

### 2.1 Status vocabulary

- **Implemented**: the adapter behavior exists and has a source-shaped
  regression or integration test.
- **Partial**: part of the wire shape or behavior exists, but at least one
  source-defined path, outcome, race, or recovery case is missing.
- **Pending**: the behavior is not safe to advertise or rely on yet.
- **Not advertised**: UmaDev intentionally sets the corresponding client
  capability to false, so Grok must use its own implementation instead of a
  reverse client RPC.

### 2.2 Mandatory capability rules

Standard ACP behavior is enabled only from fields actually returned by
`initialize` or the live session response. A Grok-specific behavior is handled
only when all of these are true:

1. the selected base is `grok-build`;
2. `initialize._meta.grokShell` is exactly `true`;
3. the exact method and payload have a typed, bounded parser;
4. outbound optional methods either have live advertisement or treat
   method-not-found as a local capability downgrade;
5. the feature's acceptance tests are green.

An old, future, prerelease, build-metadata, missing, or non-SemVer version label
does not block Grok. Unknown methods or changed payloads degrade only that
feature. A method-not-found response to an optional private method is a
capability result, not a fatal session error.

No `x.ai` behavior is described as ACP-standard. ACP is a transport protocol,
not evidence that one CLI was derived from another product.

## 3. Canonical process launch

The upstream CLI has three argument layers. Mixing them can parse successfully
while silently leaving the agent configuration unchanged.

```text
grok \
  --no-auto-update \
  --cwd <absolute-workspace> \
  --permission-mode <plan|default|bypassPermissions> \
  --sandbox <read-only|off> \
  agent \
  --no-leader \
  [--model <model-id>] \
  [--always-approve] \
  stdio
```

Layer ownership is fixed:

| Layer | Arguments |
|---|---|
| Top-level `PagerArgs` | `--no-auto-update`, `--cwd`, `--permission-mode`, `--sandbox` |
| `agent` subcommand `AgentArgs` | `--no-leader`, `--model`, `--always-approve` |
| Agent mode subcommand | `stdio` |

`--always-approve` is present only for UmaDev Auto. `--model` is omitted when
the user did not select one. Firmware/rules are not placed on argv; they are
sent as `session/new.params._meta.rules`, so they remain byte-exact and do not
appear in process listings.

UmaDev must force `--no-leader`. The pinned source otherwise reads the user's
`[cli] use_leader` configuration and may turn the spawned stdio process into a
bridge to a pre-existing shared leader. That changes extension envelopes,
session ownership, and—most importantly—makes the launched process's sandbox
insufficient to prove the permissions of the real executor. Leader support may
be added later only as an explicit, separately negotiated product mode.

Current status: **Implemented**. Source-shaped launch tests lock all three
argument layers, the three permission profiles, `--no-auto-update`, and the
unconditional `--no-leader` boundary. Windows additionally rejects npm shell
shims and accepts only the resolved native Grok executable.

## 4. ACP standard methods versus Grok extensions

### 4.1 Standard ACP surface used by the pinned build

Client to agent:

- `initialize`
- `authenticate`
- `session/new`
- `session/load`
- `session/set_mode`
- `session/prompt`
- `session/cancel` notification
- `session/set_model`

Agent to client:

- `session/request_permission`
- `session/update` notification
- `fs/read_text_file`
- `fs/write_text_file`
- `terminal/create`
- `terminal/output`
- `terminal/wait_for_exit`
- `terminal/kill`
- `terminal/release`

`session/resume` and `session/close` types in a newer schema do not prove that
this Grok build supports them. UmaDev may call an optional standard method only
when the installed agent advertises the matching capability. This pinned build
advertises `loadSession: true`; it does not advertise standard resume or close.

### 4.2 Raw extension method rule

The logical extension name never contains the ACP transport prefix:

```text
logical method: x.ai/interject
direct stdio wire method: _x.ai/interject
```

ACP 0.10.4 adds one leading underscore when sending an `ExtRequest` or
`ExtNotification`, and strips one leading underscore when decoding it. UmaDev
writes raw JSON-RPC, so it must add the underscore itself. It must never send
`x.ai/interject` as if it were a standard method.

The same rule applies in the reverse direction. For example, Grok's logical
reverse request `x.ai/ask_user_question` normally arrives on direct stdio as
`_x.ai/ask_user_question` with the real request fields directly in `params`.

### 4.3 Leader wrapper

The pinned leader gateway can wrap an extension as follows:

```json
{
  "method": "_x.ai/ask_user_question",
  "params": {
    "method": "x.ai/ask_user_question",
    "params": {
      "sessionId": "session-1",
      "toolCallId": "tool-1",
      "questions": []
    }
  }
}
```

Every classifier must therefore conceptually use `logical_method(frame)` and
`inner_params(frame)`, and must validate that wrapper and inner names agree.
UmaDev's required `--no-leader` makes this envelope unreachable in normal
operation, but the parser must still fail safely if a bridge or future explicit
leader mode produces it.

Current status: **Implemented**. Direct and leader-wrapped extension envelopes
share one strict normalizer; disagreeing wrapper names, foreign sessions, and
malformed response-shaped frames are rejected by fixtures.

## 5. Initialize, authentication, and session lifecycle

The pinned `initialize` response has these source-backed properties:

- protocol V1;
- `agentCapabilities.loadSession = true`;
- prompt `embeddedContext = true`;
- MCP HTTP and SSE support;
- no standard image flag, even though the pinned prompt parser accepts image
  blocks;
- `_meta.grokShell = true`;
- `_meta.agentVersion`, `_meta.defaultAuthMethodId`, `_meta.modelState`, and
  `_meta.availableCommands`;
- additional private metadata that must not be treated as a general capability
  list.

Authentication selection is agent-owned, but startup must also account for a
source-defined cached-token race:

1. use `_meta.defaultAuthMethodId` only when it is present in `authMethods` and
   suitable for non-interactive startup;
2. for this pinned source, do **not** call `authenticate` again for an advertised
   `cached_token`: `initialize` already refreshes, advertises, and selects it,
   while a later expiry can make `authenticate(cached_token)` fall through to
   interactive `grok.com` after discarding the caller's `headless` metadata;
3. call `authenticate` for a genuinely non-interactive `xai.api_key` method;
4. never re-derive API-key versus OIDC priority from local environment alone;
5. background preload and CLI/headless startup must stop with an actionable
   authentication-required result when only `grok.com`/`oidc` remains.

An explicit, user-authorized inline login is a separate pre-session state
machine. The pinned Grok source itself opens a browser in both loopback and
device flows; it exposes no `suppress_browser` capability. UmaDev can guarantee
that it never starts that flow before user confirmation, but cannot truthfully
promise that Grok will refrain from opening a browser after confirmation until
the upstream CLI adds a negotiated no-open option.

For `session/new`, UmaDev sends the absolute `cwd`, optional selected model, and
firmware in `_meta.rules`. The pinned source folds rules into its native
`<human_rules>` system-prompt section. Rules are creation-only: `session/load`
must retain the original session prompt rather than silently replacing it.

For recovery, use `session/load` only when `loadSession` was advertised. A
future standard resume is used only if its capability is explicitly advertised.
The expected session id is installed before a load request because Grok emits
replay frames before the load response.

Current status: **Implemented for the advertised source contract**. Initialize,
new/load, replay-handshake finalization, cached-token-safe preload, and typed
model/mode/command state have source-shaped fixtures. The resident TUI also
owns the redaction-safe pre-session state machine: an unauthenticated preload
can publish only a typed offer; an exact fresh initialize must revalidate the
chosen method; and only the user's explicit confirmation can send a
browser-capable authentication RPC. Loopback/device challenges, URL copy/open,
code submission, cancellation, timeout, stale generations and a single retry of
the original turn are covered. Interactive browser completion remains a manual
release check because the upstream process—not UmaDev—opens that browser.

### 5.1 Dynamic state and native slash commands

The command catalog is session state, not a static UmaDev command list. The
pinned source advertises an initial complete catalog in
`initialize._meta.availableCommands` and can replace it later through its
session update rails. Model catalogs and command catalogs are complete
replacements: an empty replacement clears old entries, and state from an old
backend/session must not leak into a new one.

Grok resolves a leading slash only inside `session/prompt`. Its resolver owns
built-ins, prompt-only commands, skills, feature gates, aliases, display text,
and future commands. UmaDev must therefore pass a selected native command to the
resident session as user input; it must not translate the command into an
ordinary chat instruction or run it through UmaDev's intent router, Director,
firmware rewriting, automatic review, or retry machinery.

Resolution precedence is fixed:

1. an UmaDev command keeps its documented meaning;
2. a currently advertised base command that does not conflict is sent natively;
3. `/base /<command> ...` explicitly selects the base command when names
   conflict (for example, Grok and UmaDev both expose `/compact`);
4. an unadvertised command remains an unknown command and is never guessed.

The wrapper removes only its own dispatch prefix. The native slash payload,
including argument spacing and trailing whitespace, remains byte-exact on the
wire. Attachments stay forbidden on command submissions until the selected
base advertises and tests a command-specific structured-input contract.

Native commands still use the same resident event pump as ordinary turns:
permissions, host requests, questions, plan exit, tools, state updates,
cancellation, liveness, process cleanup, and workspace postconditions all
remain active. A native command is conservatively treated as potentially
mutating, but it is not automatically re-sent after an ambiguous failure.

Current status: **Implemented** for the advertised native-command surface.
Initial/live/replay model, mode, tool, and command state reaches the TUI without
transcript rows and is cleared on session boundaries. Static UmaDev precedence,
`/base` collision escape, dynamic palette entries, byte-exact command payloads,
attachment rejection, and busy-turn typed FIFO dispatch have regression tests.

## 6. Permission profiles and Plan

| UmaDev profile | Process arguments | Truthful runtime contract |
|---|---|---|
| Plan | `--permission-mode plan --sandbox read-only` | Requests Grok plan behavior and its built-in read-only sandbox. This is not proof that the OS enforced read-only access. |
| Guarded | `--permission-mode default --sandbox off` | Requests normal Grok permission prompts and no built-in sandbox. The user chooses the exact option when Grok retains a boundary. |
| Auto | `--permission-mode bypassPermissions --sandbox off`, plus AgentArgs `--always-approve` | Requests automatic approval and no built-in sandbox. Managed policy or other higher-priority constraints can still retain a prompt or enforce a stricter sandbox. |

`session/set_mode` is used for Grok's plan/default conversational mode. It does
not replace the launch-time sandbox or permission profile and must not be
treated as a privilege boundary.

The standard permission response is:

```json
{"result":{"outcome":{"outcome":"selected","optionId":"<exact-id>"}}}
```

or a standard cancelled outcome. `allow_once`, `allow_always`, `reject_once`,
and `reject_always` remain distinct. If Grok still sends a reverse permission
request while UmaDev requested Auto, that request is an effective upstream
safety boundary: UmaDev must show it to an interactive user, or return a safe
non-approval when no user is present. It must never auto-select `allow_once` in
that case. A generic Allow/Deny response without an exact option id must not
silently fall back to a persistent option.

ACP also distinguishes rejection from cancellation. An explicit Deny may select
an offered `reject_once`; `HostResponse::Cancelled` and every permission still
pending when UmaDev sends `session/cancel` must instead return the standard
`cancelled` outcome. Grok maps those to different tool/turn states.

The pinned `agent stdio` path does not prove that top-level `--tools` or
`--no-subagents` changes were threaded into the ACP agent configuration.
Therefore the Plan contract must not claim a read-only tool allowlist or disabled
native subagents until a source-backed launch seam and tests prove it.

The launch sandbox is a requested process profile, not proof that an OS kernel
actually enforced it. The pinned source implements Linux Landlock/seccomp and
macOS Seatbelt, but macOS child-network restriction is a no-op and the Windows
enforcement path is a stub. A built-in profile may also warn and continue when
application fails. In addition, Grok's Plan permission mode does not itself
make every Bash invocation read-only, and higher-priority user/managed rules can
resolve a tool decision before an UmaDev permission request is observed. The
installed agent exposes no ACP field that reports configured-versus-applied
sandbox state, so UmaDev must label the requested profile honestly and must not
claim hard Plan isolation, Windows kernel sandboxing, or macOS child-network
isolation.

Current status: **Implemented for the truthful available surface**. Launch
profiles, exact option-id round trips, refusal to invent a persistent grant,
retained Auto boundaries, and true cancel-vs-reject encoding are covered.
Requested versus effective sandbox state is modeled separately and the UI
leaves unreported filesystem/network/local-port access Unknown. The vendor does
not currently expose an effective-mode acknowledgement, so that state cannot be
upgraded by inference. UmaDev intentionally makes no Plan tool-allowlist, hard
read-only, full-access, or disabled-subagent claim that the source cannot prove.

### 6.1 Resume safety identity

A reusable base session is identified by more than its opaque session id. The
identity must include the backend, canonical workspace, UmaDev permission
profile, requested sandbox profile, effective sandbox evidence, and the result
of any vendor-native resume preflight. A change to any immutable field forces a
fresh base process.

The pinned Grok top-level `--resume <id>` path compares its saved sandbox
profile with the requested `--sandbox` value before dispatching the `agent`
subcommand. Calling only ACP `session/load` inside a newly launched agent skips
that native check. Even the top-level check proves configured-profile
consistency, not that the OS successfully applied the sandbox. Therefore
UmaDev must not persistently resume a Grok ACP session unless both the native
preflight and effective sandbox identity are attested. With the current pinned
wire contract there is no effective-sandbox attestation, so the safe fallback
is a fresh Grok session plus explicit transcript/artifact handoff; legacy or
evidence-free session ids are never loaded silently.

### 6.2 Folder Trust

Folder Trust is a pinned xAI extension, not standard ACP. A release-stamped Grok
binary may gate project MCP servers, hooks, plugins, LSP, agents, and other
code-executing project configuration until the folder is trusted. A local
unversioned source build can leave the feature inert, so acceptance requires a
release-stamped fixture rather than assuming the feature is absent.

UmaDev may advertise the client capability only for the official Grok identity
when a live interactive trust surface is wired:

```json
{"x.ai/folderTrust":{"interactive":true}}
```

The reverse request is `x.ai/folder_trust/request` with `sessionId`, `cwd`,
`workspace`, and `configKinds`. Only an explicit user choice may return
`{"outcome":"trust"}`. Closing the UI, timeout, malformed input, a foreign
workspace/session, or any unknown outcome stays untrusted. Granting trust can
hot-reload MCP/plugins/hooks, while the pinned source requires a new session for
LSP and for sessions created while another trust modal was already open.

Current status: **Implemented for official Grok interactive sessions without a
version allowlist**. Headless sessions still do not advertise the capability. The
resident TUI carries the typed reverse request, displays the bounded workspace
and gated configuration kinds, and grants only an explicit Trust choice;
foreign scope, malformed input, close, timeout, cancellation and transport
failure all keep the folder gated. The pre-`session/new` request race is
deferred until the exact session scope exists. Hermetic release-stamped fixtures
cover both outcomes, and the official `0.2.101` binary rejection path has been
exercised without allowing project configuration.

## 7. User questions

Logical reverse method: `x.ai/ask_user_question`
Direct raw wire method: `_x.ai/ask_user_question`

Request:

```json
{
  "sessionId": "session-1",
  "toolCallId": "tool-1",
  "questions": [
    {
      "id": "q1",
      "question": "Choose one",
      "multiSelect": false,
      "options": [
        {
          "id": "a",
          "label": "A",
          "description": "Why A",
          "preview": "Optional preview"
        }
      ]
    }
  ],
  "mode": "default"
}
```

Accepted response keys are the original question text, and selected values are
option labels:

```json
{
  "outcome": "accepted",
  "answers": {"Choose one": ["A"]},
  "annotations": {"Choose one": {"notes": "optional free-form text"}}
}
```

The full outcome set is:

- `accepted`, with `answers` and optional annotations;
- `chat_about_this`, with `partial_answers`;
- `skip_interview`, with `partial_answers`;
- `cancelled`, as a successful user outcome rather than a JSON-RPC error.

Plan-mode UI must expose Chat about this and Skip interview. Option preview and
free-form notes must survive the typed runtime and TUI boundary.

Current status: **Implemented**. Direct and strictly matched leader-wrapped
requests share one parser; accepted, cancelled, chat-about-this and
skip-interview preserve partial answers, previews, annotations and free-form
notes through the typed runtime and TUI. Foreign sessions, malformed choices,
timeout and abandoned UI state fail closed.

## 8. Exit plan mode

Logical reverse method: `x.ai/exit_plan_mode`
Direct raw wire method: `_x.ai/exit_plan_mode`

Request:

```json
{
  "sessionId": "session-1",
  "toolCallId": "tool-1",
  "planContent": "# Plan"
}
```

Response:

```json
{"outcome":"approved"}
```

or:

```json
{"outcome":"cancelled","feedback":"revise this section"}
```

The source also accepts `abandoned`. UmaDev must not relabel an abandoned plan
as approved or cancelled.

Current status: **Implemented**. Approved, cancelled with feedback and abandoned
remain distinct through direct and strictly matched wrapped requests; no close,
timeout or malformed response can be relabeled as approval.

## 9. Mid-turn interjection and queue semantics

Logical request method: `x.ai/interject`
Direct raw wire method: `_x.ai/interject`

Request:

```json
{
  "sessionId": "session-1",
  "text": "Use the new API instead",
  "interjectionId": "uma-unique-id",
  "content": [
    {"type":"text","text":"Use the new API instead"}
  ]
}
```

Successful response:

```json
{"status":"queued"}
```

If `content` contains a non-empty text block, the pinned source uses that text
instead of the legacy `text` field and preserves image blocks. The interjection
is consumed at a safe point in the active turn. If it arrives while idle or
after the last safe point, Grok converts it into a front-of-queue standalone
prompt; it must not be silently lost.

The originating client uses `interjectionId` to deduplicate the later
`x.ai/session/interjection` broadcast. UmaDev should advertise same-turn steer
only after a queued response has been observed. Method-not-found changes the
capability to Unsupported and the text is retained as a visible next-turn FIFO
item.

The wider private queue surface includes remove, reorder, clear, edit, and
interject notifications plus `x.ai/queue/changed`. Those operations require
queue id/version/owner conflict handling and are not implied by basic
interjection support.

Current status: **Implemented**. Typed same-turn interjection, structured
content, queued acknowledgement, method-not-found downgrade, authoritative
queue snapshots and version-bound edit/remove/reorder/clear operations are
implemented. Stale versions and unknown ids trigger resynchronization instead
of optimistic local mutation, and native queue rows remain separate from
UmaDev's own future-input FIFO.

## 10. Replay and durable completion

The pinned `session/load` sends historical notifications before it returns its
RPC response. Two replay rails exist:

- standard `session/update`;
- private raw `_x.ai/session/update`, carrying the rich Grok update and
  `_meta.isReplay = true` when marked as replay.

Live rich updates use logical `x.ai/session_notification`; persisted rich
updates replay as logical `x.ai/session/update`. They are parallel private rails,
not aliases for standard ACP `session/update`.

Replay requirements:

1. set expected root session id before sending load;
2. keep reading responses while replay is in progress;
3. do not put hundreds of historical presentation events into a bounded live UI
   channel before `start()` returns;
4. do not re-render transcript content already restored by UmaDev;
5. do reconstruct unfinished background tasks, native subagents, current model,
   mode, and pending interactions;
6. deduplicate live and persisted copies by `_meta.eventId` where supplied;
7. complete handshake state on every success/error path;
8. release only the reconstructed live state after load returns;
9. consume durable `turn_completed` so a reconnect or lost prompt response
   cannot leave an endless spinner.

The load test must emit at least 512 completed tool/rich frames before the load
response. It passes only if the handshake completes without deadlock, historical
text is not duplicated, and unfinished lifecycle state is reconstructed.

Current status: **Implemented for the advertised wire**. A 512-plus-frame
source-shaped load fixture proves the response cannot deadlock behind replay,
historical presentation is suppressed, background processes, live subagents,
pending child interactions, prompt queue and latest model/mode/command state are
released, and durable completion is first-wins. Product-level persistent Grok
resume remains deliberately disabled because the vendor still provides no
effective-sandbox attestation; this prevents a correct replay implementation
from being misused as a permission proof.

## 11. Usage contract

The prompt response's `_meta.usage` is the whole-prompt ledger. Its
`inputTokens` already includes cache reads; `outputTokens` includes completion
usage, including reasoning as defined by the source ledger. `cachedReadTokens`
and `reasoningTokens` are subsets and must not be added again.

Sibling `_meta.inputTokens`, `_meta.outputTokens`, `_meta.cachedReadTokens`, and
`_meta.reasoningTokens` describe only the last model call. Whole-prompt
`_meta.usage` takes precedence. The durable private `turn_completed.usage` is a
fallback when the prompt RPC result is lost.

Cost is trustworthy only when it is present and both `usageIsIncomplete` and
`costIsPartial` are false. Missing cost means unknown, never zero.

Current status: **Implemented**. Whole-prompt nested usage takes precedence,
durable rich-rail completion is the bounded fallback, cache/reasoning subsets
are never double-counted, malformed or contradictory totals degrade to
estimated/unknown quality, and incomplete or partial cost is never displayed as
an exact zero or exact charge.

## 12. Background tasks and native subagents

The rich private session update enum includes, among other events:

- `task_backgrounded` and `task_completed`;
- `subagent_spawned`, `subagent_progress`, and `subagent_finished`;
- scheduled task and monitor events;
- `pending_interaction` and `interaction_resolved`;
- durable `turn_completed`;
- retry, compaction, model change, and goal progress.

A prompt RPC returning does not prove that background work is complete, but an
ordinary background process is also not evidence that the conversational turn
is unfinished. UmaDev must reconstruct and expose its lifecycle independently
from turn settlement. A long-lived dev server or monitor remains visible and
stoppable after the turn settles; it must never keep the input locked or leave
the product on an endless "waiting for port" line. Only an explicit user wait,
or a separately typed foreground dependency owned by the active turn, may
delay turn convergence.

The pinned control surface is explicit and session-scoped:

- `x.ai/task/list` accepts `sessionId` and returns an
  `ExtMethodResult` whose `result.tasks` contains full `TaskSnapshot` objects;
- `x.ai/task/kill` accepts `sessionId` plus `taskId` and returns
  `result.taskId` plus one of `killed`, `already_exited`, or `not_found`;
- `x.ai/subagent/list_running` accepts the root `sessionId` and returns the
  current child identities and bounded progress counters;
- `x.ai/subagent/get` accepts `subagentId` with optional blocking/timeout
  controls and returns a typed running/completed/failed/cancelled snapshot;
- `x.ai/subagent/cancel` accepts `subagentId` and distinguishes `cancelled`,
  `already_finished` (with the terminal status), and `not_found`.

The ordinary task-list payload also contains raw command, cwd, output, and
output-file fields. UmaDev must not copy those fields directly into its durable
state or status bar: only a bounded, redacted display projection may cross the
host boundary. `not_found` and `already_exited` are convergent outcomes, not
proof that a new kill succeeded. A transport/error envelope leaves the row in
an Unknown/reconciling state until an authoritative list replaces it.

For subagents, `subagent_spawned` establishes a trusted mapping among
`parent_session_id`, `subagent_id`, and `child_session_id`. Only a declared child
may send child-scoped updates or interactions. A child permission or question
must be routed to that child, not rejected merely because its session id differs
from the root. `subagent_finished` closes the lifecycle only for terminal status
`completed`, `failed`, or `cancelled`. Live and replay copies are deduplicated by
event id.

UmaDev must not advertise `SubagentVisibility::Lifecycle` until direct live,
replay reconstruction, child routing, and premature-settle tests all pass.

Current status: **Implemented** for native-subagent routing and ordinary
background-process control. A controlled root/descendant
graph now covers nested spawn/reparent, progress, child-scoped permission,
question and exit-plan routing, replay/resync, terminal retirement, cancellation,
delayed root completion, and single-consumption `will_wake` convergence. Ordinary
background-process start/finish/live reconstruction is typed and cannot block
turn settlement. `/processes` fetches the native authoritative list and
`/processes stop <id>` reaches `x.ai/task/kill` only after a fresh same-session
ownership check; terminal-facing rows exclude command, cwd, output, and file
paths. Real-binary multi-OS smoke remains part of the release matrix in §16.

## 13. Terminal and tool output

UmaDev currently advertises standard client terminal capability as false and
both client FS capabilities as false. Under the pinned source this means Grok
uses its own local `TerminalRunner` and `LocalFs`; this is intentional and must
not be described as missing filesystem or terminal access.

UmaDev additionally requests private incremental bash output and no-color
output. Output still arrives through correlated session updates:

1. `tool_call` opens `toolCallId` with its name and bounded raw input;
2. non-terminal `tool_call_update` content is emitted as correlated output
   deltas;
3. exactly one `completed` or `failed` update closes that id;
4. late updates may synthesize a missing open event but may not settle twice;
5. structured `rawOutput` is rendered through typed text/content/diff/terminal
   extraction, not an unbounded one-line JSON dump;
6. ANSI/control sequences are sanitized even when no-color was requested;
7. large output is bounded without dropping lifecycle or terminal state.

Because terminal capability is false, an unexpected standard reverse
`terminal/*` request receives method-not-found and cannot gain host execution
authority. If client terminal support is added later, the full create/output/
wait/kill/release state machine must be implemented first, including Unix shell
quoting and native Windows behavior.

Current status: **Implemented** for the advertised surface; reverse terminal
remains **Not advertised** by design. Correlated incremental output, complete
snapshot replacement/clearing, stateful ANSI sanitization, malformed byte
fallback, clipping, late start, and exactly-once settlement have regression
coverage.

## 14. Error, frame, and cancellation semantics

The pinned Grok line reader allows at most 64 MiB including the newline. An
oversized or invalid UTF-8 frame is fatal to the connection. UmaDev must drain
pending requests with one precise, redacted protocol error and terminate the
reader; it must not accept a forged response after an oversized frame.

Malformed JSON is bounded by an error budget and never logged with secrets.
Explicit non-2.0 JSON-RPC is rejected. Response ids that cannot match an
UmaDev-generated numeric id are response-shaped noise, not interactive server
requests.

Standard `session/cancel` defaults in the pinned source to cancelling subagents,
not killing background tasks, and not rewinding a pristine turn. UmaDev waits
for the prompt response or durable completion before accepting another turn. A
timeout moves the session to an explicit StillCancelling/failed state; it does
not pretend cancellation completed.

Current status: **Implemented** for the current session contract. Fatal frame
bounds, pending-request cleanup, terminal-aware cancellation, typed
StillCancelling failure, private close, EOF grace, and bounded Unix/Windows
process-tree reaping are covered by protocol and lifecycle tests.

## 15. Source references

The following pinned files are the primary audit map:

| Concern | Upstream source |
|---|---|
| ACP dependency/version | root `Cargo.toml:91-93`, `Cargo.lock` package entries |
| ACP standard message inventory | `crates/codegen/xai-acp-lib/src/message.rs:132-192,357-410,497-516` |
| Raw extension underscore rule | dependency `agent-client-protocol-0.10.4/src/lib.rs:221-235,527-540,609-633` |
| CLI argument ownership | `xai-grok-pager/src/app/cli.rs:250-301,632-697` |
| Leader selection | `xai-grok-pager-bin/src/main.rs:1072-1129` |
| Direct/wrapped leader envelopes | `xai-grok-shell/src/leader/server.rs:403-439` |
| Initialize and auth metadata | `xai-grok-shell/src/agent/mvp_agent/acp_agent.rs:270-438` |
| Official auth selection | `xai-grok-pager/src/acp/mod.rs:709-760` |
| Inline auth channels and cached-token fallthrough | `xai-grok-shell/src/agent/mvp_agent/acp_agent.rs:676-770` and `mvp_agent/agent_ops.rs:710-749` |
| Browser-opening behavior | `xai-grok-auth/src/oidc/login.rs` and `xai-grok-auth/src/device_flow.rs` |
| Session load/replay | `xai-grok-shell/src/agent/mvp_agent/acp_agent.rs:1488-1537,1934-1940` and `mvp_agent/mod.rs:1334-1439` |
| Native slash-command resolution | `xai-grok-shell/src/session/acp_session_impl/turn.rs:274-335` |
| Ask user question | `xai-grok-tools/src/implementations/grok_build/ask_user_question/types.rs:50-121` |
| Exit plan mode | `xai-grok-tools/src/implementations/grok_build/exit_plan_mode/types.rs:11-25` |
| Interjection | `xai-grok-shell/src/extensions/interject.rs:13-63` and `session/acp_session_impl/interjection.rs:20-172` |
| Queue wire and conflict handling | `xai-grok-shell/src/session/prompt_queue.rs`, `agent/ext_parsers.rs`, and `session/acp_session_impl/prompt_queue.rs` |
| Background/subagent control extensions | `xai-grok-shell/src/extensions/task.rs` and `xai-grok-pager/src/app/effects/mod.rs` |
| Rich usage/lifecycle enum | `xai-grok-shell/src/extensions/notification.rs:17-68,543-901` |
| Prompt response usage | `xai-grok-shell/src/agent/mvp_agent/mod.rs:390-480` |
| Cancel behavior | `xai-grok-shell/src/agent/mvp_agent/acp_agent.rs:3053-3093` |
| Client FS/terminal selection | `xai-grok-shell/src/agent/mvp_agent/agent_ops.rs:2833-2963` |
| Windows stdin handling | `crates/codegen/xai-acp-lib/src/stdin_reader.rs:1-40,78-215` |
| 64 MiB line semantics | `crates/codegen/xai-acp-lib/src/line_reader.rs:25-30,135-176` |

Line numbers are commit-scoped. They must be refreshed when the pinned commit
changes.

## 16. Drift audit procedure

Every new Grok source snapshot follows this sequence to keep adaptation current:

1. fetch the official repository and record exact commit, tag/version, clean
   worktree status, and remote URL;
2. record the resolved ACP crate and schema versions from upstream `Cargo.lock`;
3. diff the previous pinned commit against the candidate in:
   - ACP message/line/stdin code;
   - CLI argument definitions and `run_agent_command`;
   - `initialize`, auth, session new/load/prompt/cancel/set-model;
   - leader gateway envelope/routing;
   - ask, plan, interject, permission, terminal, and FS handlers;
   - rich session notification enum and pager notification dispatcher;
   - background task, queue, and subagent lifecycle code;
4. regenerate the standard/private method and notification inventory;
5. classify every changed field as standard ACP, private `x.ai`, or internal
   persistence detail—never infer from its name;
6. update source-shaped JSON fixtures for both direct and wrapped envelopes;
7. run the complete release matrix below on the candidate binary;
8. update this document and source line references without creating a runtime
   version allowlist;
9. keep all prior versions usable while the candidate matrix runs;
10. if any private contract cannot be verified, downgrade only that capability
    while retaining safe standard ACP and the rest of the base.

## 17. Release acceptance matrix

No Grok adapter release is complete until all applicable rows pass.

| Area | Required acceptance | Status at audit |
|---|---|---|
| Source identity | Exact vendor identity; every version accepted; version is diagnostics only | Implemented |
| argv | Exact layer/order for Plan, Guarded, Auto; `--no-leader` cannot be overridden by user config | Implemented |
| Initialize | Protocol, auth methods, load capability, model state, commands, and client capabilities round-trip | Implemented in source-shaped fixtures; exact published binary handshake is a Linux/macOS/Windows CI gate |
| Authentication | default id, API key, cached token, OIDC preference, expired token, no-auth cases; no interactive flow before explicit confirmation; disclose upstream browser open after confirmation | Implemented in hermetic transition tests; cached-token and isolated no-auth release paths passed on macOS, interactive browser completion remains a manual release check |
| New session | Absolute cwd, model, rules, mode, and session id | Implemented in fixtures and an authenticated macOS `0.2.101` live run |
| Load/replay | 512+ pre-response frames; no deadlock or duplicate transcript; unfinished state reconstructed | Implemented for the advertised wire in hermetic fixtures; product resume remains intentionally disabled until effective-sandbox attestation and native preflight are available |
| Standard updates | text, thought, plan, tools, mode, commands, model, user echo dedupe | Implemented in source-shaped direct/replay fixtures |
| Permissions | All four option kinds; exact option id; Auto retained-boundary prompt; reject/cancel distinction; no implicit persistent grant | Implemented in source-shaped fixtures; authenticated Auto tool execution passed on macOS |
| Folder Trust | Source-gated capability; explicit trust UI; foreign/close/error/timeout fail closed; release-stamped fixture | Implemented in hermetic fixtures and the official `0.2.101` binary's release-simulation path; pre-`session/new` request race covered |
| Sandbox truth | Requested profile versus applied state; resume-profile mismatch; honest Linux/macOS/Windows labels | Implemented (unreported effective state remains Unknown; Grok load fails closed) |
| Questions | accepted, chat_about_this, skip_interview, cancelled; preview and notes; direct/wrapped | Implemented in strict source-shaped fixtures |
| Exit plan | approved, cancelled+feedback, abandoned; direct/wrapped | Implemented in strict source-shaped fixtures |
| Interject | active safe point, idle fallback, end-of-turn race, echo dedupe, method-not-found fallback | Implemented in hermetic pinned-source fixtures |
| Queue | change notification plus edit/remove/reorder/clear version conflicts | Implemented in hermetic fixtures; official `0.2.101` binary enqueue/drain smoke passed on macOS |
| Usage | whole-prompt precedence, durable fallback, cache/reasoning non-double-count, incomplete cost | Implemented in source-shaped success/error/malformed fixtures |
| Background tasks | Ordinary long-lived work remains visible/stoppable without blocking turn settlement; explicit foreground waits still block | Implemented in hermetic fixtures; official `0.2.101` binary list/owned-stop/process-death smoke passed on macOS; authenticated Linux/Windows smoke pending |
| Subagents | Spawn/progress/finish, child mapping, child interaction, replay, dedupe, no premature settle | Implemented in hermetic direct/replay fixtures |
| Durable completion | Lost prompt response finalized once by `turn_completed` | Implemented |
| Tool output | Correlation, incremental output, late start, exactly-once settle, bounded structured rendering | Implemented |
| Cancellation | Delayed and wedged cancel; subagent semantics; no active-turn race | Implemented |
| Frames | 64 MiB boundaries, oversized fatality, malformed JSON budget, secret redaction | Implemented |
| Shutdown | Child and tool process cleanup; no orphan on Unix or Windows | Implemented |
| Terminal matrix | macOS, Linux, native Windows PowerShell/cmd/ConPTY; CJK paths, spaces, CRLF/LF | Automated PTY/ConPTY and byte contracts ship; graphical/manual matrix remains Pending |
| TUI matrix | CJK/emoji width, resize, Ctrl+C, question/plan panels, queue, model change, child folding | Automated state/render contracts ship; graphical/manual matrix remains Pending |
| Rust gates | workspace tests, clippy with `-D warnings`, fmt check | Implemented in ordinary and tag CI; a particular release is proven only by its own green tag run |
| Real binary smoke | Pinned Grok binary performs new/prompt/tool/permission/ask/plan/cancel/load on each OS family | Exact unauthenticated published-binary handshake is gated on Linux/macOS/Windows; full authenticated lifecycle passed on macOS, authenticated Linux/Windows remains Pending |

The table is intentionally conservative. “ACP connected” or “a prompt returned”
is not sufficient evidence for a mature integration.
