//! Value-shape guards for the bypass-immune hardcoded-secret floor (UD-SEC-003).
//!
//! The floor cannot be switched off in `.umadev/rules.toml`, so it may only block
//! a value that has the shape of a real credential. A secret-looking NAME is also
//! followed by plenty of things that are not credentials: code that reads the
//! secret from somewhere else (`settings.OPENAI_API_KEY`, `process.env.X`, the
//! LiteLLM `os.environ/AZURE_API_KEY` reference), a format placeholder in a URL
//! template (`%s`, `{settings.WX_SECRET}`), a type annotation (`api_key: string;`)
//! and a bare constant name (`OPENAI_API_KEY_PRODUCTION`).

use super::{is_placeholder_value, looks_like_low_entropy_slug};

/// The first credential-shaped value that follows a contiguous `prefix`
/// (`api_key=`, `secret=`, `access_token=`, …) anywhere in `content`.
///
/// `lower` is the ASCII-lowercased `content` (same byte offsets). Every
/// occurrence is examined, so a harmless first occurrence (`api_key=settings.X`)
/// cannot hide a real key assigned later in the same file.
pub(super) fn prefix_secret<'a>(lower: &str, content: &'a str, prefix: &str) -> Option<&'a str> {
    lower.match_indices(prefix).find_map(|(index, _)| {
        let value = token_after(content.get(index + prefix.len()..)?);
        is_credential_token(value).then_some(value)
    })
}

/// The value after a prefix, cut at the first character that cannot belong to an
/// opaque credential token. A key is a run of `[A-Za-z0-9_+/=-]`, so the cut drops
/// everything that marks code or a template instead of a literal: the `.` of a
/// dotted path, `%`/`{` placeholders, `&` query separators, and `,;)}` or
/// whitespace ending an expression.
fn token_after(after: &str) -> &str {
    let trimmed = after.trim_start_matches(['=', ':', ' ', '"', '\'']);
    let end = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '=')))
        .unwrap_or(trimmed.len());
    &trimmed[..end]
}

/// Whether a prefix value is an opaque credential token rather than an
/// identifier, a path, a slug or an example.
fn is_credential_token(value: &str) -> bool {
    // `token_after` keeps ASCII only, so the byte length is the char count.
    value.len() > 20
        && !is_placeholder_value(value)
        && !value.starts_with('/')
        && !looks_like_low_entropy_slug(value)
        && !looks_like_code_identifier(value)
}

/// A bare code identifier (`OPENAI_API_KEY_PRODUCTION`, `openaiApiKeyFromSettings`,
/// `load_api_key_from_keychain`): letters joined by `_`/`-` with no digits, or an
/// upper-case constant name. Real keys almost always carry digits.
fn looks_like_code_identifier(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphabetic() || matches!(b, b'_' | b'-'))
        || is_constant_name(value)
}

/// `ACCESS_TOKEN`, `USER_INFO`, `X-ACCESS-TOKEN`: upper-case words joined by `_`
/// or `-`, mostly letters. A random upper-case key with separators
/// (`AB12-CD34-EF56-GH78`) is digit-heavy and does not qualify.
fn is_constant_name(value: &str) -> bool {
    let segments_ok = value.contains(['_', '-'])
        && value.split(['_', '-']).all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        });
    let letters = value.bytes().filter(u8::is_ascii_uppercase).count();
    let digits = value.bytes().filter(u8::is_ascii_digit).count();
    segments_ok
        && value.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        && digits * 3 <= letters
}
