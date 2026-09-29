//! Shared secret redaction for persisted and user-visible diagnostics.
//!
//! Redaction is a storage and logging concern, not a filter on what UmaDev
//! decides with. Approval and trust decisions, governance scans and the live
//! transcript all work on the unredacted text: a redacted command can hide
//! what is being authorized (`API_TOKEN=[redacted]` stands for anything after
//! the `=`), and redacted file content hides the very secret the
//! hardcoded-secret rule exists to catch. Only persisted records, logs and
//! diagnostics pass through here.
//!
//! What gets replaced is a secret *value* of a concrete shape: a known token
//! prefix (`sk-`, `ghp_`, `github_pat_`, `xox*-`, `AKIA…`, …), a PEM private
//! key block, the credential after `Bearer`/`Basic` in an authorization value,
//! URI userinfo, and the literal assigned to a credential-named key when that
//! literal looks like a secret rather than code. Keys, quotes, separators and
//! the rest of the line are kept, so JSON stays valid and code stays readable:
//! `password: string;` and `"csrfToken": getToken()` are left alone. The
//! tradeoff is that a short, letters-only value (`PASSWORD=changeme`) or a
//! secret under an unrecognised key name is not caught; the known-prefix and
//! PEM patterns do not depend on the key at all.

use std::sync::OnceLock;

use regex::{Captures, Regex};
use serde_json::Value;

const REDACTED: &str = "[redacted]";
const MAX_JSON_REDACTION_DEPTH: usize = 64;
const MAX_JSON_REDACTION_NODES: usize = 32_768;

fn normalized_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_pagination_key(key: &str) -> bool {
    matches!(
        key,
        "cursor"
            | "nextcursor"
            | "pagecursor"
            | "paginationcursor"
            | "cursortoken"
            | "pagetoken"
            | "nextpagetoken"
            | "paginationtoken"
            | "continuationtoken"
            | "resumetoken"
    )
}

fn is_token_metric_key(key: &str) -> bool {
    matches!(
        key,
        "inputtokens"
            | "outputtokens"
            | "totaltokens"
            | "cachedtokens"
            | "reasoningtokens"
            | "maxtokens"
            | "tokencount"
            | "inputtokencount"
            | "outputtokencount"
            | "tokenusage"
            | "tokenbudget"
    )
}

fn is_sensitive_key(key: &str) -> bool {
    let key = normalized_key(key);
    if is_pagination_key(&key) || is_token_metric_key(&key) {
        return false;
    }
    if matches!(
        key.as_str(),
        "env" | "environment" | "environmentvariables" | "headers" | "httpheaders"
    ) {
        return true;
    }
    if matches!(
        key.as_str(),
        "token"
            | "authorization"
            | "proxyauthorization"
            | "apikey"
            | "accesstoken"
            | "refreshtoken"
            | "authtoken"
            | "idtoken"
            | "sessiontoken"
            | "apitoken"
            | "password"
            | "passwd"
            | "pwd"
            | "passphrase"
            | "secret"
            | "clientsecret"
            | "secretkey"
            | "credential"
            | "credentials"
            | "cookie"
            | "setcookie"
            | "privatekey"
            | "privatekeypem"
    ) {
        return true;
    }
    [
        "token",
        "authorization",
        "apikey",
        "accesstoken",
        "refreshtoken",
        "authtoken",
        "idtoken",
        "sessiontoken",
        "apitoken",
        "password",
        "passphrase",
        "clientsecret",
        "secret",
        "secretkey",
        "secretaccesskey",
        "credential",
        "privatekey",
    ]
    .iter()
    .any(|suffix| key.ends_with(suffix))
}

/// A credential-named key followed by `:` or `=` and the literal assigned to
/// it: double-quoted, single-quoted, or a bare token. The quote is captured
/// separately so only the value between the quotes is replaced.
fn assignment_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r#"(?i)(?P<prefix>(?P<key>[A-Za-z0-9_-]*(?:authorization|api[-_]?key|access[-_]?key|"#,
            r#"secret[-_]?key|client[-_]?secret|private[-_]?key|password|passwd|passphrase|"#,
            r#"token|secret))["']?\s*[:=]\s*)"#,
            r#"(?:"(?P<dq>(?:[^"\\\r\n]|\\.)*)"|'(?P<sq>[^'\r\n]*)'|(?P<bare>[^\s"'`,;(){}\[\]<>]+))"#,
        ))
        .expect("static sensitive-assignment regex is valid")
    })
}

/// The credential in an HTTP authorization value (`Authorization: Basic …`).
/// The scheme is kept; only the credential after it is replaced.
fn authorization_scheme_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?P<prefix>\b(?:proxy-)?authorization["']?\s*[:=]\s*["']?(?:basic|digest|token|negotiate)\s+)[A-Za-z0-9._~+/=-]{4,}"#,
        )
        .expect("static authorization-scheme regex is valid")
    })
}

fn pem_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?is)-----BEGIN [^-\r\n]*PRIVATE KEY-----.*?(?:-----END [^-\r\n]*PRIVATE KEY-----|\z)",
        )
        .expect("static private-key regex is valid")
    })
}

fn bearer_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?P<prefix>\bbearer\s+)(?P<value>[A-Za-z0-9._~+/=-]{8,})")
            .expect("static bearer regex is valid")
    })
}

fn uri_userinfo_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?P<prefix>\b[a-z][a-z0-9+.-]{0,31}://)[^/@\s?#]{1,1024}(?P<suffix>@)")
            .expect("static URI-userinfo regex is valid")
    })
}

fn prefixed_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:ghp_|github_pat_|sk-|xai-)[A-Za-z0-9._~+/=-]{8,}")
            .expect("static token-prefix regex is valid")
    })
}

fn provider_secret_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)(?:",
            r"sk-(?:proj-)?[A-Za-z0-9_-]{20,}",
            r"|(?:sk_|pk_)[A-Za-z0-9_]{16,}",
            r"|stripe_[A-Za-z0-9]{16,}",
            r"|github_pat_[A-Za-z0-9_]{20,}",
            r"|(?:ghp_|gho_|ghs_|ghu_|ghr_)[A-Za-z0-9]{20,}",
            r"|glpat-[A-Za-z0-9_-]{20,}",
            r"|xox[bpars]-[A-Za-z0-9-]{10,}",
            r"|AIza[A-Za-z0-9_-]{30,}",
            r"|SG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}",
            r"|npm_[A-Za-z0-9]{36}",
            r"|(?:AKIA|ASIA)[0-9A-Z]{16}",
            r"|eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]*",
            r")",
        ))
        .expect("static provider-secret regex is valid")
    })
}

/// Words that follow a credential-named key in code or prose without being a
/// secret: type annotations, literals and common placeholders.
const NON_SECRET_WORDS: &[&str] = &[
    "string",
    "str",
    "number",
    "int",
    "integer",
    "bool",
    "boolean",
    "any",
    "unknown",
    "object",
    "bytes",
    "text",
    "none",
    "null",
    "nil",
    "undefined",
    "true",
    "false",
    "optional",
    "required",
    "secretstr",
    "redacted",
    "placeholder",
    "changeme",
    "example",
    "exampletoken",
    "xxx",
];

/// Whether `value`, assigned to a credential-named key, looks like a secret
/// literal rather than code, a type or a placeholder. `next` is the first
/// non-blank character after a bare value, so an identifier that is followed
/// by `;`, `}`, `)` or `(` reads as code (`password: string;`,
/// `token = make_token()`).
fn is_secret_literal(value: &str, quoted: bool, next: Option<char>) -> bool {
    let value = value.trim();
    if value.is_empty()
        || NON_SECRET_WORDS.contains(&value.to_ascii_lowercase().as_str())
        || value.starts_with(['$', '<', '{', '%', '['])
        || value
            .chars()
            .all(|c| matches!(c, '*' | 'x' | 'X' | '.' | '-' | '_'))
    {
        return false;
    }
    if quoted {
        return true;
    }
    if value.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let identifier = value.split('.').all(|part| {
        part.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    if identifier
        && (value.contains('.') || matches!(next, Some(';' | '}' | ')' | '(' | ',' | '[')))
    {
        return false;
    }
    // A bare word of letters only is a type or variable name far more often
    // than a secret (`token: Token`, `password = hashed`); long ones are kept
    // in scope because generated secrets can be alphabetic.
    value.chars().count() >= 16
        || value
            .chars()
            .any(|c| !(c.is_ascii_alphabetic() || c == '_'))
}

fn redact_assignment(text: &str, captures: &Captures<'_>) -> String {
    let whole = captures.get(0).expect("a match has group 0");
    let key = normalized_key(captures.name("key").map_or("", |key| key.as_str()));
    if is_pagination_key(&key) || is_token_metric_key(&key) {
        return whole.as_str().to_string();
    }
    let prefix = captures.name("prefix").map_or("", |prefix| prefix.as_str());
    let (value, quote) = if let Some(value) = captures.name("dq") {
        (value.as_str(), Some('"'))
    } else if let Some(value) = captures.name("sq") {
        (value.as_str(), Some('\''))
    } else {
        (
            captures.name("bare").map_or("", |value| value.as_str()),
            None,
        )
    };
    let next = text[whole.end()..]
        .chars()
        .find(|c| !matches!(c, ' ' | '\t'));
    if !is_secret_literal(value, quote.is_some(), next) {
        return whole.as_str().to_string();
    }
    match quote {
        Some(quote) => format!("{prefix}{quote}{REDACTED}{quote}"),
        None => format!("{prefix}{REDACTED}"),
    }
}

/// Replace secret values of a concrete shape in `text`, keeping everything
/// around them. See the module documentation for what counts as a secret.
#[must_use]
pub fn redact_text(text: &str) -> String {
    let without_pem = pem_regex().replace_all(text, "[redacted private key]");
    let without_uri_userinfo =
        uri_userinfo_regex().replace_all(&without_pem, "${prefix}[redacted]${suffix}");
    let without_bearer =
        bearer_regex().replace_all(&without_uri_userinfo, |captures: &Captures<'_>| {
            let value = captures.name("value").map_or("", |value| value.as_str());
            if matches!(
                value.to_ascii_lowercase().as_str(),
                "authentication" | "credentials" | "placeholder" | "exampletoken"
            ) {
                captures
                    .get(0)
                    .map_or("", |value| value.as_str())
                    .to_string()
            } else {
                format!(
                    "{}[redacted]",
                    captures
                        .name("prefix")
                        .map_or("Bearer ", |value| value.as_str())
                )
            }
        });
    let without_scheme =
        authorization_scheme_regex().replace_all(&without_bearer, "${prefix}[redacted]");
    let without_prefixed = prefixed_token_regex().replace_all(&without_scheme, REDACTED);
    let without_providers = provider_secret_regex().replace_all(&without_prefixed, REDACTED);
    assignment_regex()
        .replace_all(&without_providers, |captures: &Captures<'_>| {
            redact_assignment(&without_providers, captures)
        })
        .into_owned()
}

/// Recursively redact JSON for persistence. The shape is kept: every scalar
/// beneath a sensitive key (`password`, `headers`, `env`, …) is replaced in
/// place, and other strings go through [`redact_text`].
#[must_use]
pub fn redact_json(value: Value) -> Value {
    let mut remaining_nodes = MAX_JSON_REDACTION_NODES;
    redact_json_bounded(value, 0, &mut remaining_nodes, false)
}

fn redact_json_bounded(
    value: Value,
    depth: usize,
    remaining_nodes: &mut usize,
    sensitive: bool,
) -> Value {
    if depth >= MAX_JSON_REDACTION_DEPTH || *remaining_nodes == 0 {
        return Value::String(REDACTED.to_string());
    }
    *remaining_nodes -= 1;
    match value {
        Value::Object(map) => {
            if map.len() > *remaining_nodes {
                return Value::String(REDACTED.to_string());
            }
            Value::Object(
                map.into_iter()
                    .map(|(key, value)| {
                        let sensitive = sensitive || is_sensitive_key(&key);
                        let value =
                            redact_json_bounded(value, depth + 1, remaining_nodes, sensitive);
                        (key, value)
                    })
                    .collect(),
            )
        }
        Value::Array(values) => {
            if values.len() > *remaining_nodes {
                return Value::String(REDACTED.to_string());
            }
            Value::Array(
                values
                    .into_iter()
                    .map(|value| redact_json_bounded(value, depth + 1, remaining_nodes, sensitive))
                    .collect(),
            )
        }
        Value::String(text) if sensitive && !text.is_empty() => Value::String(REDACTED.to_string()),
        Value::Number(_) if sensitive => Value::String(REDACTED.to_string()),
        Value::String(text) => Value::String(redact_text(&text)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_without_hiding_metrics_or_pagination() {
        let text = "api_key=sk-live-secret-value\ninput_tokens=42\npage_token=page-4";
        let redacted = redact_text(text);
        assert!(!redacted.contains("sk-live-secret-value"));
        assert!(redacted.contains("input_tokens=42"));
        assert!(redacted.contains("page_token=page-4"));
    }

    #[test]
    fn redacts_nested_json_by_key_and_value_shape() {
        let value = serde_json::json!({
            "headers": {"Authorization": "Bearer live-secret-token"},
            "message": "use ghp_1234567890abcdef",
            "token_usage": 99
        });
        let redacted = redact_json(value);
        assert_eq!(redacted["headers"]["Authorization"], REDACTED);
        assert_eq!(redacted["message"], "use [redacted]");
        assert_eq!(redacted["token_usage"], 99);
    }

    #[test]
    fn redacts_standalone_provider_credentials_and_jwts() {
        let secrets = [
            "AKIA1234567890ABCDEF",
            "xoxb-1234567890-secret",
            "sk_live_1234567890abcdef",
            "eyJabcdefghijk.eyJabcdefghijk.signature",
        ];
        let redacted = redact_text(&secrets.join(" "));
        for secret in secrets {
            assert!(!redacted.contains(secret));
        }
    }

    #[test]
    fn redacts_uri_userinfo_without_changing_normal_urls() {
        let text = concat!(
            "postgres://app:super-secret@db.example.test/main\n",
            "mongodb://opaque-token@db.example.test/data\n",
            "https://example.test:8443/path?q=1"
        );
        let redacted = redact_text(text);

        assert!(redacted.contains("postgres://[redacted]@db.example.test/main"));
        assert!(redacted.contains("mongodb://[redacted]@db.example.test/data"));
        assert!(redacted.contains("https://example.test:8443/path?q=1"));
        assert!(!redacted.contains("super-secret"));
        assert!(!redacted.contains("opaque-token"));
    }

    #[test]
    fn deeply_nested_json_is_replaced_before_recursion_can_exhaust_the_stack() {
        let mut value = Value::String("deep-value".to_string());
        for _ in 0..(MAX_JSON_REDACTION_DEPTH * 4) {
            value = serde_json::json!({"safe": value});
        }

        let redacted = redact_json(value);
        let mut cursor = &redacted;
        let mut observed_depth = 0;
        while let Some(next) = cursor.get("safe") {
            observed_depth += 1;
            cursor = next;
        }

        assert!(observed_depth <= MAX_JSON_REDACTION_DEPTH);
        assert_eq!(cursor, REDACTED);
    }

    #[test]
    fn oversized_json_subtrees_are_replaced_instead_of_partially_walked() {
        let values = (0..=MAX_JSON_REDACTION_NODES)
            .map(|value| Value::from(value as u64))
            .collect();

        assert_eq!(redact_json(Value::Array(values)), REDACTED);
    }

    #[test]
    fn code_and_structure_around_secret_names_survive() {
        for text in [
            "interface User { password: string; name: string }",
            "def login(password: str, token: Token) -> None:",
            "const token = makeToken(user);",
            "password = form.password",
            "export PASSWORD=$DB_PASSWORD",
            "api_key: ${{ secrets.API_KEY }}",
            "\"password\": \"\"",
            "\"credentials\": \"include\"",
            "max_tokens=4096 input_tokens=12",
        ] {
            assert_eq!(redact_text(text), text, "code was rewritten");
        }
    }

    #[test]
    fn only_the_secret_value_is_replaced() {
        assert_eq!(
            redact_text(r#"{"csrfToken":"abc123","next":1}"#),
            r#"{"csrfToken":"[redacted]","next":1}"#
        );
        assert_eq!(
            redact_text("PASSWORD=hunter2 npm start"),
            "PASSWORD=[redacted] npm start"
        );
        assert_eq!(
            redact_text("password: 'correct horse'\nuser: ada"),
            "password: '[redacted]'\nuser: ada"
        );
        assert_eq!(
            redact_text("Authorization: Basic dXNlcjpwYXNz"),
            "Authorization: Basic [redacted]"
        );
        let json = redact_text(r#"{"githubToken":"SYNTH_7f31","cursor":"c-2"}"#);
        let parsed: Value = serde_json::from_str(&json).expect("still valid JSON");
        assert_eq!(parsed["githubToken"], REDACTED);
        assert_eq!(parsed["cursor"], "c-2");
    }

    #[test]
    fn sensitive_json_keys_keep_their_shape() {
        let redacted = redact_json(serde_json::json!({
            "env": {"HOME": "/home/u", "PORT": 8080},
            "password": "",
            "enabled": true
        }));
        assert_eq!(redacted["env"]["HOME"], REDACTED);
        assert_eq!(redacted["env"]["PORT"], REDACTED);
        assert_eq!(redacted["password"], "");
        assert_eq!(redacted["enabled"], true);
    }
}
