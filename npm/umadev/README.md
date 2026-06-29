# UmaDev

> **UmaDev: A coding agent that works like a real dev team** — product manager,
> architect, UI/UX designer, frontend, backend, QA, security, and DevOps, each doing
> its own specialty on a shared blackboard, borrowing the Claude Code / Codex /
> OpenCode you already logged into as the brain. A **coordinator** seat schedules the
> team and enforces the gates (it routes, plans, and gates — it is not the headline);
> the base writes the code. You don't hire a director — you hire the whole team.
> **No API key needed.**

## Install

```bash
npm install -g umadev
```

## Use

```bash
umadev                            # launch the interactive TUI
                                     # (auto-detects logged-in claude / codex)

umadev init                       # write umadev.yaml spec manifest

umadev run "做一个登录系统" \
            --backend claude-code    # scripted form, no TUI
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
build, review, and sign off like a real team — over one of the three
first-class base CLIs you already use (`claude`, `codex`, `opencode`); the
brain stays in the base. Wider model coverage is the base's job (route it to
a third-party / local model), not a new UmaDev driver.

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
