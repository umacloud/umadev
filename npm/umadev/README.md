# UmaDev

> **UmaDev: A coding agent that works like a real dev team, commanding one of five AI coding CLIs you already use** — product manager,
> architect, UI/UX designer, frontend, backend, QA, security, and DevOps, each doing
> its own specialty on a shared blackboard, borrowing that logged-in base as the
> brain. A **coordinator** seat schedules the
> team and enforces the gates (it routes, plans, and gates — it is not the headline);
> the base writes the code. You don't hire a director — you hire the whole team.
> **UmaDev needs no additional model API key; authenticate the selected base itself.**

## Install

```bash
npm install -g umadev
```

## Use

```bash
umadev                            # launch the interactive TUI
                                     # (probes the five supported bases)

umadev init                       # write umadev.yaml spec manifest

umadev run "做一个登录系统" \
            --backend grok-build     # any supported backend id; no TUI
umadev continue                   # approve the active gate
umadev revise "去掉 OAuth"        # request a revision

umadev verify                     # workspace conformance report
umadev doctor                     # self-test
umadev spec [--clauses]           # print UMADEV_HOST_SPEC_V1
umadev report                     # emit UD-EVID-004 compliance map
```

## Why this exists

UmaDev is **not** an LLM client. It does not call any AI API.
Instead it convenes a development team — eight role specialists that plan,
build, review, and sign off like a real team — over exactly five first-class
bases. Claude Code (`claude-code`), Codex (`codex`), and OpenCode (`opencode`)
use vendor-specific transports. Grok Build (`grok-build`) and Kimi Code
(`kimi-code`) use isolated profiles over the hardened ACP v1 transport core.
The brain, account, credentials, and model
stay in the selected base.

Authentication, permissions, and resume are vendor-specific. Grok Plan adds its
read-only sandbox, tool allowlist, and subagent fence. Kimi Code is source-audited
against an exact baseline, but runtime support is capability-negotiated rather
than version-allowlisted; it uses `kimi acp` and revalidates its existing login
without UmaDev opening a browser. Resume/load is used only when the corresponding
base contract advertises and authorizes it. See the repository README for
install/login commands, Windows notes, and the complete capability matrix.

The coordinator routes each request: a chat stays chat, a one-line edit takes
the fast path, and only a full product requirement expands into the team's
deepest play — the deterministic commercial delivery chain:

```
research → docs → ⏸ docs_confirm → spec → frontend → ⏸ preview_confirm → backend → quality → delivery
```

At each `⏸ gate`, UmaDev pauses and surfaces the artifacts (PRD,
architecture, UIUX, …) for you to review. After every code-producing
phase it runs the project's build / test command (e.g. `cargo check`,
`npm install`) and records the outcome in `.umadev/audit/verify.jsonl`
so a non-technical user can ship stable code without writing any.

The result is a `release/proof-pack-*.zip` containing every artifact,
every gate decision, and every audit row.

## Documentation

Full docs, design rationale, and the UMADEV_HOST_SPEC_V1 spec:
<https://github.com/umacloud/umadev>

## License

MIT
