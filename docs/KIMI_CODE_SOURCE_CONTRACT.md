# Kimi Code source contract

UmaDev's `kimi-code` base follows the official MIT-licensed
[`MoonshotAI/kimi-code`](https://github.com/MoonshotAI/kimi-code) source, not to
the retired `MoonshotAI/kimi-cli` repository and not to a guessed command-line
wrapper.

## Audited source baseline

- source version: `@moonshot-ai/kimi-code@0.31.0`
- source-audited commit: `17dfd49768f753a4f0fe97d8e7d3317dab560575`
- real npm artifact tested on Linux/macOS/Windows: `@moonshot-ai/kimi-code@0.31.0`
- release tag for that artifact: `@moonshot-ai/kimi-code@0.31.0`
- ACP adapter: `@moonshot-ai/kimi-acp-adapter@0.3.6`
- ACP SDK declared by upstream: `@agentclientprotocol/sdk@^0.23.0`
- machine entrypoint: `kimi acp`

The fixed source commit and markers are checked by
`crates/umadev-host/tests/kimi_source_drift.rs` locally and by the
`kimi-source-contract` CI job. This is a drift alarm, not a runtime allowlist:
every official Kimi Code version is accepted, and optional controls are derived
from the live session's advertised `configOptions`.

The audit through `0.31.0` covers session creation/resume/load/list, model,
thinking and mode controls, permissions, cancellation, streaming tool events,
native skills and terminal-owned login. Kimi's internal external-hook
registration changed from a mutable decision object to an event veto API;
UmaDev's source fixture follows that semantic change, while the ACP transport
and UmaDev's runtime version-independent capability negotiation remain
version-independent.

The separate `kimi-published-contract` CI matrix installs that audited npm
package on Linux, macOS, and Windows, launches its real `kimi acp` entrypoint
with an isolated `KIMI_CODE_HOME`, and verifies the unauthenticated handshake.
It must return the audited identity and terminal-owned `kimi login` guidance
without opening a browser, starting a model session, or touching the project.
This closes the gap between source-marker review and the artifact users
actually install. The tag-release workflow repeats the same three-platform
published-package gate, and every release binary build depends on it, so a tag
cannot bypass the distribution check.

## Transport and lifecycle

UmaDev owns the interactive TUI and starts Kimi as a background ACP v1
JSON-RPC subprocess. It performs the complete lifecycle:

1. `initialize` and official identity validation; the version is diagnostic only;
2. on-disk login revalidation through `authenticate(methodId="login")`;
3. `session/new`, or standard `session/resume`/`session/load` for a compatible
   persisted session;
4. canonical `session/set_config_option` for model and mode;
5. `session/prompt` streaming text, thought, tool, plan and configuration
   updates;
6. ordinary `session/request_permission` round trips through UmaDev's approval
   UI, while Kimi's source-specific `AskUserQuestion` reuse of that method is
   decoded into a real structured question picker and returns the exact
   `q0_opt_*` / `q0_skip` option id;
7. idempotent `session/cancel`, followed by bounded child-process teardown.

Kimi does not implement ACP `session/close`; UmaDev therefore closes stdin and
reaps the full child process tree. No provider SDK or UmaDev-owned API key is
used.

Kimi also advertises ACP `session/list`. Its audited implementation optionally
filters the user's on-disk Kimi sessions by `cwd` and returns
`sessionId`/`cwd`/`title`/`updatedAt`. UmaDev deliberately does not merge that
vendor-global history into its `/sessions` command: `/sessions` lists only
UmaDev-owned project chats whose base id, workspace, base identity and effective
authority were persisted together. A Kimi session created outside UmaDev has no
such attestation and is not silently imported or resumed. Sessions created by
UmaDev remain fully resumable through the standard `session/resume` or
`session/load` path recorded with that chat.

## Authority and permissions

Kimi's four native modes are `default`, `plan`, `auto`, and `yolo`. UmaDev maps
its profiles deliberately:

| UmaDev profile | Kimi mode | Result |
|---|---|---|
| Plan | `plan` | Kimi plan mode plus UmaDev read-only policy |
| Guarded | `default` | Kimi manual approvals routed to UmaDev |
| Auto | `default` | UmaDev auto-resolves ordinary in-tree approvals, escalates only its irreversible floor, and never silently enables Kimi `auto`/`yolo` |

Keeping Kimi in `default` does not make UmaDev Auto less autonomous than
Guarded: Kimi continues to expose authority-bearing requests over ACP, and the
local trust policy resolves the ordinary ones without opening a prompt. A
Kimi question is never auto-approved merely because it shares the permission
transport shape.

The shared ACP client advertises no filesystem or terminal reverse-I/O
capability. Kimi therefore keeps command and file execution inside its own
process boundary. Grok private metadata, authentication, queue, background and
folder-trust extensions are never sent to Kimi.

The audited release also exposes native `PreToolUse` and `PostToolUse` hooks
in `config.toml`. UmaDev merge-installs Write/Edit/Bash guards and audit rows.
Because Kimi's hook registry is user-level, every command contains the
canonical project root; the hook process emits an immediate fail-open allow for
other workspaces. Existing hooks, comments, model/provider configuration and
credentials are preserved, writes are private and atomic, and uninstall removes
only the three rows for that project. This is a defense-in-depth pre-apply
surface, not a replacement for ACP approval policy or the final quality gate.

## Authentication

UmaDev never runs `kimi login`, never calls `kimi acp --login`, and never opens
a browser. If the ACP login check reports no usable token, the session stops
before `session/new` and shows the user the explicit terminal command
`kimi login`. This prevents the surprise browser/login loop previously seen
with other tools.

## Models, thinking, attachments and MCP

- The model catalog and current model come from Kimi's returned
  `configOptions`; requested models must exist in that catalog and be confirmed
  by the response.
- Kimi's `TodoList` display becomes the standard ACP `plan` whole-snapshot.
  UmaDev renders it as a separate base plan, including pending, active and
  completed state, so it cannot overwrite the director-owned team plan.
- Kimi maps a source `tool.progress` status update to a title-only
  `tool_call_update`. UmaDev keeps this as a non-terminal tool-card title
  replacement, never mislabels it as stdout, and preserves `toolCallId` through
  the host, team pipeline and TUI. Interleaved progress, output snapshots and
  terminal results therefore update only their owning tool row. Because Kimi
  carries the final tool output on the terminal update, UmaDev retains the
  TUI's 8 KiB process-log budget instead of truncating it to a 512-character
  diagnostic; normal rendering still folds long output.
- Kimi's full `config_option_update` snapshot atomically replaces the live model
  catalog and refreshes the current model, thinking toggle and audited
  `default`/`plan` mode in UmaDev; unrecognized authority modes are never
  guessed or reclassified.
- Kimi exposes thinking as an `off`/`on` configuration option. The audited
  adapter maps `on` to the selected model's default or middle supported effort
  internally, while an always-thinking model omits `off`. UmaDev retains and
  displays this enabled/disabled capability independently; the parser also
  accepts a future advertised non-`off` value without inventing one. Because each
  `configOptions` message is a complete replacement, a model that omits the
  control clears the prior state, while an always-thinking model remains
  visibly locked on instead of showing a switch that cannot work. `/thinking
  on|off` uses the resident `session/set_config_option` control plane, verifies
  the returned complete snapshot, and never sends the setting as model chat.
- Kimi plan review remains a structured human decision even in UmaDev Auto:
  every source-defined plan option, Revise and Reject-and-Exit round-trips by
  its exact `plan_*` id. Headless, timeout, Esc and malformed-option paths return
  `cancelled`; no policy path silently selects the first plan variant.
- Permission routing recognizes Kimi's human-input surface before validating
  its full payload. `AskUserQuestion` requires sequential `q0_opt_0..N` rows
  followed by the exact `q0_skip`; plan review requires the exact ordered
  `plan_*` shape. Malformed or drifted human-input requests fail closed and can
  never fall through to ordinary tool approval.
- Ordinary approval likewise requires Kimi's exact ordered `approve_once`,
  `approve_always`, and `reject` rows with their source-defined labels and
  semantic kinds. A widened or reordered permission surface is rejected rather
  than guessed.
- Negotiated ACP image and embedded-resource capabilities drive native image and
  file delivery. Kimi's official adapter accepts UTF-8 text embedded resources
  but explicitly drops blob resources, so UmaDev sends text files natively and
  rejects binary generic files before a turn instead of reporting a false
  delivery. Images retain their separate negotiated native path.
- Project MCP settings use Kimi's native `.kimi-code/mcp.json` and
  `mcpServers` schema. UmaDev parse-merges one named server atomically and
  preserves unknown fields and existing entries.
- Kimi discovers project skills from `.kimi-code/skills` first and the shared
  `.agents/skills` fallback, then publishes the session-specific catalog over
  `available_commands_update`. UmaDev replaces its live base-command snapshot,
  exposes every non-conflicting `/skill:*` row with its description, and sends
  the complete typed command and arguments back through the resident ACP
  session without trimming. If a Kimi built-in collides with an UmaDev product
  command, the product command remains deterministic and the native command is
  still reachable explicitly as `/base /<command>`. UmaDev neither scans nor
  rewrites Kimi's skill directories, so Kimi remains the authority for
  discovery, ordering and activation.

## Platform contract

- macOS/Linux/Windows executable: `kimi` (Windows npm `.cmd` launch is handled
  by UmaDev's shared process resolver).
- installation: `npm install -g @moonshot-ai/kimi-code` (no UmaDev version pin).
- Windows shell tools require Git for Windows/Git Bash. Upstream probes standard
  Git locations, Scoop/portable installs and `KIMI_SHELL_PATH`, and fails before
  a session when no usable `bash.exe` exists.
- `KIMI_CODE_NO_AUTO_UPDATE=1` is set for the ACP child so a running audited
  process cannot replace itself underneath a session.

## Honest limitation

The audited Kimi adapter filters raw child-agent events before they reach ACP.
UmaDev therefore does not claim native Kimi subagent lifecycle visibility.
UmaDev's own team roles still run as independent, isolated fork sessions. This
claim can be upgraded only after upstream exposes a source-verifiable child
lifecycle channel.

The audited adapter maps provider filtering and hook blocking to ACP
`refusal`, which UmaDev renders as a failed turn. Other non-auth SDK failures
are logged by Kimi but resolve on the ACP wire as `end_turn`; ACP 0.23 has no
dedicated failed stop reason. UmaDev therefore does not invent a failure reason
that the wire did not carry: deterministic postconditions, tool failures and
the normal verification gates remain authoritative for whether work actually
completed. This limitation can be removed only when upstream exposes a typed
failure on the ACP channel.
