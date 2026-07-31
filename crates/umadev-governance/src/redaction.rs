//! Shared secret redaction for persisted and user-visible diagnostics.

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

fn assignment_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?P<prefix>(?:authorization|proxy[-_]?authorization|api[-_]?key|access[-_]?token|refresh[-_]?token|auth[-_]?token|id[-_]?token|session[-_]?token|client[-_]?secret|secret[-_]?key|password|passwd|passphrase|private[-_]?key)\s*[\"']?\s*[:=]\s*)[^\r\n]+"#,
        )
        .expect("static sensitive-assignment regex is valid")
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

fn token_assignment_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?P<prefix>(?P<key>[A-Za-z][A-Za-z0-9_-]*token)\s*[\"']?\s*[:=]\s*)[^\r\n]+"#,
        )
        .expect("static token-assignment regex is valid")
    })
}

/// Redact common credential assignments, bearer values, private keys, and token prefixes.
#[must_use]
pub fn redact_text(text: &str) -> String {
    let without_pem = pem_regex().replace_all(text, "[redacted private key]");
    let without_uri_userinfo =
        uri_userinfo_regex().replace_all(&without_pem, "${prefix}[redacted]${suffix}");
    let without_assignments =
        assignment_regex().replace_all(&without_uri_userinfo, "${prefix}[redacted]");
    let without_tokens =
        token_assignment_regex().replace_all(&without_assignments, |captures: &Captures<'_>| {
            let key = captures.name("key").map_or("", |value| value.as_str());
            let normalized = normalized_key(key);
            if is_pagination_key(&normalized) || is_token_metric_key(&normalized) {
                captures
                    .get(0)
                    .map_or("", |value| value.as_str())
                    .to_string()
            } else {
                format!(
                    "{}[redacted]",
                    captures
                        .name("prefix")
                        .map_or("token=", |value| value.as_str())
                )
            }
        });
    let without_bearer = bearer_regex().replace_all(&without_tokens, |captures: &Captures<'_>| {
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
    let without_prefixed = prefixed_token_regex().replace_all(&without_bearer, REDACTED);
    provider_secret_regex()
        .replace_all(&without_prefixed, REDACTED)
        .into_owned()
}

/// Recursively redact sensitive JSON keys and string values.
#[must_use]
pub fn redact_json(value: Value) -> Value {
    let mut remaining_nodes = MAX_JSON_REDACTION_NODES;
    redact_json_bounded(value, 0, &mut remaining_nodes)
}

fn redact_json_bounded(value: Value, depth: usize, remaining_nodes: &mut usize) -> Value {
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
                        if is_sensitive_key(&key) {
                            (key, Value::String(REDACTED.to_string()))
                        } else {
                            (key, redact_json_bounded(value, depth + 1, remaining_nodes))
                        }
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
                    .map(|value| redact_json_bounded(value, depth + 1, remaining_nodes))
                    .collect(),
            )
        }
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
        assert_eq!(redacted["headers"], REDACTED);
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
}
