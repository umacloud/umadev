# Security policy

## Supported versions

Security fixes are made on the latest `1.0.x` release. Users should reproduce
and report a security issue on the newest published UmaDev version whenever it
is safe to do so.

## Reporting a vulnerability

Please do not open a public issue for an unpatched vulnerability, credential
exposure, or a reproducible permission-boundary bypass.

Use GitHub's private **Security → Report a vulnerability** flow for
`umacloud/umadev`. If that flow is unavailable, email the maintainer address in
the workspace package metadata (`11964948@qq.com`) with the subject
`[UmaDev security]`.

Include only the minimum evidence needed to reproduce the issue:

- UmaDev version, install method, OS, terminal, and selected base;
- the exact trust/sandbox mode and whether `--yes` was used;
- a minimal throwaway repository or redacted steps;
- the observed impact and any known workaround.

Never send API keys, login cookies, private source, or an unredacted
`.umadev/audit/` directory. We will coordinate disclosure after a fix is
available; please avoid publishing exploit details beforehand.

## Security boundaries worth reporting

- a `plan` or read-only critic session mutating the workspace;
- a base receiving broader native permissions than the selected UmaDev mode;
- a destructive, publish, deploy, push, credential-exfiltration, or
  workspace-escape action bypassing the reversibility floor;
- release/update artifacts accepted without their required integrity check;
- secrets or raw private prompts copied into global learned memory, logs, or a
  proof pack;
- command, path, archive, terminal-sequence, or MCP input injection that crosses
  its documented boundary.

## Release authenticity

### 1.0.74 npm incident

The npm-only `umadev@1.0.74` publication was not produced from this repository,
has no matching Git tag or GitHub release, and contained an install-time
JavaScript payload. It is malicious and must not be installed or executed. The
last safe public release before the incident is `1.0.73`; `1.0.75` is the first
recovery release produced by the hardened tokenless publishing pipeline.

If `1.0.74` was installed, disconnect the host from the network, preserve a
copy of the npm logs for investigation, remove the package, rotate npm and
developer credentials from a known-clean device, and reinstall a verified safe
release. Do not treat leftover `bun-dl-*` temporary directories alone as proof
of compromise; confirm against the package version, lifecycle payloads, and
the indicators below.

Known indicators:

- npm tarball SHA-256:
  `1990199f10112b3851f6da4a04bb392bb44b4ef42b2feaa7cc3839eceb07e3c5`;
- `setup.mjs` SHA-256:
  `fd3ca4007b225fdf8de7af4345a19179d5efa8c4bb9205f88cda806e5684b1eb`;
- `math_init.js` SHA-256:
  `9fc2570b7cef51c1b8df116d144d11ff4096357be7d2c4c6367cfc2509cf1bcc`.

The updater now accepts a release only when the official npm registry returns
the expected inert package shape, exact platform-package version set, SHA-512
integrity, registry signature, and npm provenance attestation. Release jobs
also reject lifecycle scripts and unexpected package files before any registry
write.

Some third-party npm mirrors may continue serving a stale cached copy of the
withdrawn `1.0.74`. Install or recover through the official registry explicitly:
`npm install -g @umatech/umadev`. A mirror reporting
a higher version is not release evidence; require the matching Git tag, GitHub
Release, integrity, signatures, and provenance.

Tag releases always require a protected publishing identity. Native
signing is enabled only when the repository variable `SIGN_RELEASE=true`; in
that mode the release fails before artifact construction unless every signing
credential is configured. macOS executables are then signed with a Developer ID
Application certificate, hardened runtime, and a secure timestamp, submitted
with `notarytool`, and assessed by Gatekeeper. Windows executables are then
Authenticode-signed, RFC 3161 timestamped with SHA-256, and verified with
SignTool before checksums, npm packages, or GitHub attestations are created.

The release workflow expects these repository or protected-environment secrets:

- `APPLE_CERTIFICATE_P12_BASE64`, `APPLE_CERTIFICATE_PASSWORD`,
  `APPLE_SIGNING_IDENTITY`;
- `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_SPECIFIC_PASSWORD`;
- `WINDOWS_CERTIFICATE_PFX_BASE64`, `WINDOWS_CERTIFICATE_PASSWORD`.
- `NPM_TOKEN`, a freshly issued granular token scoped to the nine public UmaDev
  packages, stored only in the protected `npm-production` environment.

npm publication uses explicit token authentication only inside the protected
`npm-production` publish step. The workflow writes a temporary npmrc containing
only an environment-variable placeholder, never the token bytes, and keeps
`id-token: write` plus npm provenance enabled so every package remains linked to
the GitHub tag workflow. It rejects credentials injected into any earlier job,
an unselected authentication mode, missing provenance, or a userconfig outside
the runner's temporary directory. The environment secret must be deleted and
the npm token revoked after release verification. Trusted Publishing remains
the preferred steady-state replacement once all nine package connections are
configured.

Certificates and passwords are decoded only on their native ephemeral runner and
are deleted before the job ends. A manual non-tag workflow run may build unsigned
test artifacts, but only a `v*` tag can publish a release and every such tag must
pass both native signature gates.
