# UmaDev — npm distribution

This directory packages UmaDev for `npm install -g umadev`. End
users only ever see the `umadev` name; the multi-package layout is
the standard pattern used by `esbuild`, `biome`, `swc`, `@tailwindcss/oxide`,
etc. to ship a prebuilt Rust binary via npm without forcing every user
to download every platform's binary.

## Layout

```
npm/
├── umadev/                        ← the user-facing package (`npm i -g umadev`)
│   ├── package.json                  ← optionalDependencies for every platform
│   ├── bin/cli.js                    ← thin JS shim, resolves + execs the binary
│   └── README.md                     ← end-user README rendered on the npm page
├── cli-darwin-arm64/                 ← per-platform sub-package (one binary each)
│   └── package.json                  ←   os: ["darwin"], cpu: ["arm64"]
├── cli-darwin-x64/                   ← Intel Mac
├── cli-linux-x64/                    ← Linux x86_64
├── cli-linux-arm64/                  ← Linux ARM (Pi, Graviton, ARM cloud)
├── cli-linux-musl-x64/               ← Alpine/musl Linux x86_64
├── cli-linux-musl-arm64/             ← Alpine/musl Linux ARM64
├── cli-win32-x64/                    ← Windows x86_64
├── knowledge-corpus/                 ← platform-independent curated knowledge
└── scripts/
    ├── stage.sh                      ← stage a binary + tag/commit digest manifest
    ├── release-provenance.mjs        ← write/verify all platform proof manifests
    ├── smoke.sh                      ← local end-to-end smoke test
    └── publish.sh                    ← preflight + publish all 9 runtime packages
```

## Why this works (the npm magic)

Every platform sub-package declares `os` / `cpu` in its `package.json`:

```json
{ "os": ["darwin"], "cpu": ["arm64"] }
```

The main `umadev` package lists all seven platform packages and the knowledge
corpus under `optionalDependencies`. When a user runs `npm i -g umadev`, npm:

1. Tries to install every `optionalDependency`.
2. Silently skips any whose `os` / `cpu` does not match the current host.
3. Uses `libc` metadata on Linux to distinguish glibc from musl; older package
   managers may retain both variants, and the launcher still selects the correct one.

The JS shim resolves the platform package bound to that exact main-package
install (nested first, then hoisted) and uses `child_process.spawnSync` to
exec its binary. `stdio: 'inherit'` preserves the TTY so the ratatui UI works.

## How a release flows

1. CI builds `umadev` for each target (see `.github/workflows/release.yml`).
2. For each target a tag-push CI run calls
   `npm/scripts/stage.sh <platform> <binary>`. Staging also writes a published
   SHA-256 manifest that binds the exact binary bytes to the npm version,
   release tag, target triple, and full source commit.
3. `npm/scripts/publish.sh` verifies the version lock and all seven provenance
   manifests against the current tag/commit, stages the knowledge corpus in a
   clean temporary directory, and packs all nine tarballs before the first
   irreversible publish. Manual `workflow_dispatch` runs validation only and
   cannot publish, including when it is dispatched against a tag.
4. Exact versions are first published under a temporary `staging` dist-tag.
   After every registry integrity check passes, `latest` is promoted in
   dependency order with the main `umadev` package last. A rerun skips an
   already-published tarball only when its integrity exactly matches, and an
   older release is forbidden from moving `latest` backwards.

## Local smoke test (M8 verification)

```bash
./npm/scripts/smoke.sh
```

Builds umadev release-mode for the host platform, stages it into
the matching `cli-<platform>/bin/`, then invokes
`node npm/umadev/bin/cli.js --version` and asserts the binary's
real version string came through. Local staging records `tag: "local"`; the
publish preflight rejects that manifest, so smoke-test bytes cannot be mistaken
for tag-built release bytes.

## Maintenance

The version in **every** npm `package.json`, both website changelog heads, and
the workspace `Cargo.toml#workspace.package.version` must agree. The release
tag must be `v<version>`. `verify-version-lock.mjs` enforces this; `smoke.sh`
also fails if the resolved binary reports a different version.

`publish.sh` packs all nine published packages from temporary release roots,
injects the repository `LICENSE` into each tarball, and verifies that notice
before the first registry mutation. Production publishing is accepted only in
the GitHub tag-push workflow; local execution is restricted to `--dry-run` from
the matching tag.

When bumping versions, update:
- `Cargo.toml` (`[workspace.package].version`; `bump-version.sh` then refreshes the lockfile)
- `npm/umadev/package.json`
- `npm/cli-*/package.json` (×7)
- `npm/knowledge-corpus/package.json` and `npm/model-e5-small/package.json`
- `npm/umadev/package.json#optionalDependencies` versions
- the first Chinese and English release entries in
  `umadev-website/src/app/content.ts`

`npm/scripts/bump-version.sh` updates Cargo and npm manifests; the changelog
entry remains an intentional human-authored release note and is checked before
CI can publish.
