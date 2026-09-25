//! **UD-SEC-001**: the bypass-immune guard against writes into
//! version-control internals, secret stores and toolchain configuration.

use super::Decision;

/// Directory names that mark any write *inside* them as sensitive. Matched
/// as a path segment (so `.git/` matches `a/.git/b` AND `.git/b` but not
/// `digit.ts`). Part of the bypass-immune safety check (UD-SEC-001).
/// `checkpoints.git` is UmaDev's shadow checkpoint repository
/// (`.umadev/checkpoints.git`): a hook or `commondir` planted there would run
/// or redirect Git on the next automatic checkpoint.
const SENSITIVE_DIRS: &[&str] = &[
    ".git",
    ".ssh",
    ".aws",
    ".claude",
    ".vscode",
    "checkpoints.git",
];

/// Specific sensitive path *suffixes* (file/dir names) matched against the
/// normalized path. Each is matched as a trailing path component so it works
/// for both absolute (`/x/.env`) and relative (`.env`) targets.
const SENSITIVE_PATH_SUFFIXES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.development",
    ".umadevrc",
    "credentials",
    "credentials.json",
    "service-account.json",
    ".netrc",
    ".pypirc",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
];

/// Check whether a write targets a security-sensitive path. Implements
/// **UD-SEC-001**: a bypass-immune guard that blocks the host from writing
/// into version-control internals (`.git/`), secret stores (`.env`,
/// `~/.ssh/`, `~/.aws/`), or the host's own configuration (`.claude/settings`,
/// `.vscode/settings`). Unlike the code-style rules this is a SAFETY check,
/// not a quality check — it fires first and is exempt from any future
/// "skip governance" toggle, mirroring Claude Code's bypass-immune
/// safetyCheck (`utils/permissions/permissions.ts` step 1f/1g).
///
/// A project `.npmrc` is blocked only when `content` sets a registry
/// credential (`_authToken`, `_auth`, `_password`): the registry mirror,
/// `legacy-peer-deps` and pnpm hoisting settings it usually carries are not
/// secrets, npm reads no other file name, and `.umadev/rules.toml` cannot
/// exempt a floor path.
#[must_use]
pub fn check_sensitive_path(file_path: &str, content: &str) -> Decision {
    let normalized = file_path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    // 1. Segment match for sensitive directories: any path component equal to
    //    a SENSITIVE_DIRS entry (or settings.json *inside* .claude/.vscode)
    //    is blocked. Splitting on '/' avoids the `digit.ts` false positive a
    //    naive `.contains(".git")` would produce.
    for seg in lower.split('/') {
        if SENSITIVE_DIRS.contains(&seg) {
            return Decision::block(
                "UD-SEC-001",
                format!(
                    "UmaDev: write to sensitive path `{file_path}` blocked (UD-SEC-001). \
                     A parent segment (`{seg}`) holds version-control internals, secrets, \
                     or toolchain config — overwriting it can corrupt the repo or leak \
                     credentials. {FLOOR_ADVICE}"
                ),
            );
        }
    }
    // 2. Trailing-path-suffix match: `.env`, `id_rsa`, `settings.json`, etc.
    //    matched against the END of the normalized path so both `.env` and
    //    `apps/api/.env` are caught.
    let is_named = |name: &str| lower == name || lower.ends_with(&format!("/{name}"));
    if let Some(suffix) = SENSITIVE_PATH_SUFFIXES
        .iter()
        .find(|suffix| is_named(suffix))
    {
        return Decision::block(
            "UD-SEC-001",
            format!(
                "UmaDev: write to sensitive file `{file_path}` blocked (UD-SEC-001). \
                 `{suffix}` typically holds secrets or credentials. {FLOOR_ADVICE}"
            ),
        );
    }
    // 3. A project `.npmrc` only when it sets a registry credential.
    if is_named(".npmrc") && npmrc_sets_credential(content) {
        return Decision::block(
            "UD-SEC-001",
            format!(
                "UmaDev: write to sensitive file `{file_path}` blocked (UD-SEC-001). \
                 This `.npmrc` sets a registry credential (`_authToken` / `_auth` / \
                 `_password`). Reference an environment variable instead \
                 (`//registry.npmjs.org/:_authToken=${{NPM_TOKEN}}`); registry, mirror and \
                 install settings without a credential are allowed. {FLOOR_ADVICE}"
            ),
        );
    }
    Decision::pass()
}

/// The truthful way out of a UD-SEC-001 block: the floor ignores the project's
/// policy file, so nothing in it can let this write through.
const FLOOR_ADVICE: &str = "This safety floor ignores `.umadev/rules.toml`; if the change is \
     intended, make it yourself outside the UmaDev run.";

/// Whether `.npmrc` content assigns a literal registry credential. A value read
/// from the environment (`${NPM_TOKEN}`) is not one.
fn npmrc_sets_credential(content: &str) -> bool {
    content.lines().any(|line| {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().trim_matches(['"', '\'']);
        let credential_key = key.ends_with("_authtoken")
            || key.ends_with("_auth")
            || key.ends_with(":_password")
            || key == "_password";
        !line.starts_with(['#', ';'])
            && credential_key
            && !value.is_empty()
            && !value.starts_with('$')
    })
}
