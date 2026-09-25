//! Value-shape guards for the bypass-immune hardcoded-secret floor (UD-SEC-003).
//!
//! The floor cannot be switched off in `.umadev/rules.toml`, so it may only block
//! a value that has the shape of a real credential. A secret-looking NAME is also
//! followed by plenty of things that are not credentials: code that reads the
//! secret from somewhere else (`settings.OPENAI_API_KEY`, `process.env.X`, the
//! LiteLLM `os.environ/AZURE_API_KEY` reference), a format placeholder in a URL
//! template (`%s`, `{settings.WX_SECRET}`), a type annotation (`api_key: string;`),
//! a bare constant name (`OPENAI_API_KEY_PRODUCTION`), a storage-key or
//! header-name constant (`'ACCESS_TOKEN'`, `'X-Access-Token'`), and user-facing
//! text written in another script (`"密码错误，请重新输入"`).

use regex::Regex;
use std::sync::OnceLock;

use super::{is_placeholder_value, looks_like_low_entropy_slug, looks_like_url_or_path};

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

/// Match a NAMED secret assignment: a key NAME (`api_key`/`secret`/`token`/
/// `password`/…) followed by `=`/`:` (with any spacing, and optionally a quoted
/// name as in JSON) and a QUOTED value. This is the form a contiguous
/// `name=value` prefix scan misses: `const API_KEY = "…"` (spaces) and
/// `"apiKey": "…"` (quote-colon). Returns `(matched_name, value_char_len)` for
/// the first non-placeholder hit. The quoted-value requirement keeps it off
/// `process.env.X` references and bare code expressions.
pub(super) fn named_secret_match(content: &str) -> Option<(String, usize)> {
    for caps in named_secret_regex().captures_iter(content) {
        let (Some(name), Some(value)) = (caps.get(1), caps.get(2)) else {
            continue;
        };
        let value = value.as_str();
        if is_placeholder_value(value) {
            continue;
        }
        // Same guards the entropy fallback already applies (see
        // [`is_high_entropy_secret`]): a value that is a URL / data-URI /
        // filesystem path, or a low-entropy lowercase kebab-/snake-case slug
        // (a design token like `color-primary-strong`, an identifier, a
        // pagination cursor) is NOT a credential — it must not hard-block on
        // the un-overridable secret floor merely because it sits under a
        // `token`/`auth`/`secret` name. A genuine secret-shaped value
        // (`sk-ant-…`, `AKIA…`, a mixed-case / high-entropy base64 or hex blob)
        // has no `://`/`/` and mixes case or entropy, so it still blocks here.
        // Chinese UI text and `ACCESS_TOKEN` / `X-Access-Token` key names are
        // not credentials either.
        if looks_like_url_or_path(value)
            || looks_like_low_entropy_slug(value)
            || is_non_credential_literal(value)
        {
            continue;
        }
        return Some((name.as_str().to_string(), value.chars().count()));
    }
    None
}

/// Compiled detector for a named secret key assigned a quoted literal value.
///
/// `["']?` around the name allows a JSON quoted key (`"apiKey":`); `\s*[:=]\s*`
/// allows any spacing (`const API_KEY = "…"`); the value class excludes
/// whitespace and structural punctuation so it stops at the literal's end and
/// never runs into surrounding code. The 12-char value floor keeps it off short,
/// low-signal values. The NAME (`\b`-bounded) is the high-signal part — `secret`
/// will not match inside `secret_key`, which forces the longer alternative.
fn named_secret_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r#"(?i)["']?\b("#,
            r"api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token|refresh[_-]?token",
            r"|access[_-]?key|client[_-]?secret|private[_-]?key|password|passwd|pwd",
            r"|secret|token|auth",
            r#")\b["']?\s*[:=]\s*["']([^\s"',;(){}]{12,})["']"#,
        ))
        .expect("named-secret regex is well-formed")
    })
}

/// Whether a QUOTED value under a secret name (`token: '…'`, `"password": "…"`)
/// is plainly not a credential: text in a non-Latin script (credentials are
/// ASCII), an upper-case constant name (`ACCESS_TOKEN`, `NEXT_PUBLIC_SECRET`), or
/// an HTTP header name (`Authorization`, `X-Access-Token`).
fn is_non_credential_literal(value: &str) -> bool {
    !value.is_ascii() || is_constant_name(value) || is_header_name(value)
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

/// `Authorization`, `X-Access-Token`, `Admin-Token`: capitalised words joined by
/// `-`, the shape of an HTTP header name.
fn is_header_name(value: &str) -> bool {
    value.split('-').all(|word| {
        let mut bytes = word.bytes();
        bytes.next().is_some_and(|b| b.is_ascii_uppercase())
            && bytes.all(|b| b.is_ascii_lowercase())
    })
}
