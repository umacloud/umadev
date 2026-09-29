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
    ".npmrc",
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
#[must_use]
pub fn check_sensitive_path(file_path: &str, _content: &str) -> Decision {
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
 "UmaDev: write to sensitive path `{file_path}` blocked (UD-SEC-001).                      A parent segment (`{seg}`) holds version-control internals, secrets,                      or toolchain config — overwriting it can corrupt the repo or leak                      credentials. If this is intentional, exclude this path from the                      governance hook or run the host outside UmaDev's supervision."
                ),
            );
        }
    }
    // 2. Trailing-path-suffix match: `.env`, `id_rsa`, `settings.json`, etc.
    //    matched against the END of the normalized path so both `.env` and
    // `apps/api/.env` are caught.
    for suffix in SENSITIVE_PATH_SUFFIXES {
        if lower == *suffix || lower.ends_with(&format!("/{suffix}")) {
            return Decision::block(
 "UD-SEC-001",
                format!(
 "UmaDev: write to sensitive file `{file_path}` blocked (UD-SEC-001). `{suffix}` typically holds secrets, credentials, or toolchain config.                      If this is intentional and not a real secret, rename the file or                      exclude it from the governance hook."
                ),
            );
        }
    }
    Decision::pass()
}
