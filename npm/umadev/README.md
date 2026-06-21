# UmaDev

> **AI 编码的项目总监 Agent** — drives the Claude Code / Codex you already
> logged into through a 9-phase commercial delivery pipeline.
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
Instead it **drives** the host CLI you already use (`claude`, `codex`)
through a deterministic 9-phase pipeline:

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
