//! Local syntactic context used by the high-signal security rules.
//!
//! These helpers deliberately avoid whole-file keyword correlation. They are
//! small lexical recognizers, not language parsers: ambiguous input fails open.

/// Return whether a stable, code-owned literal has a shape that the entropy
/// fallback must not confuse with a credential.
pub(crate) fn is_stable_non_secret_literal(value: &str) -> bool {
    looks_like_regex_source(value)
        || looks_like_release_artifact(value)
        || looks_like_isolate_sentinel(value)
}

/// Return whether one local persistence statement stores a password without a
/// visible hash operation or a value previously assigned from one.
pub(crate) fn contains_unhashed_password_storage(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    let statements = logical_statements(&lower);
    let mut hashed_values: Vec<String> = Vec::new();

    for (index, statement) in statements.iter().enumerate() {
        if has_password_hasher(statement) {
            if let Some(name) = assigned_identifier_before_hasher(statement) {
                hashed_values.push(name.to_string());
            }
        }

        if has_password_token(statement) && has_persistence_operation(statement) {
            let proven_hashed = has_password_hasher(statement)
                || password_value_is_hash_named(statement)
                || hashed_values
                    .iter()
                    .any(|name| contains_identifier(statement, name));
            if !proven_hashed {
                return true;
            }
        }

        let Some(owner) = unhashed_password_assignment_owner(statement) else {
            continue;
        };
        let Some(next) = statements.get(index + 1) else {
            continue;
        };
        if has_persistence_operation(next) && contains_identifier(next, owner) {
            return true;
        }
    }
    false
}

fn looks_like_regex_source(value: &str) -> bool {
    let escape_classes = [r"\s", r"\w", r"\d", r"\b", r"\x", r"\p{", r"\(", r"\["]
        .into_iter()
        .filter(|marker| value.contains(marker))
        .count();
    escape_classes >= 2
        && (value.starts_with('^')
            || value.ends_with('$')
            || value.contains("(?:")
            || value.contains("[^"))
}

fn looks_like_release_artifact(value: &str) -> bool {
    const TARGET_SUFFIXES: &[&str] = &[
        "-apple-darwin",
        "-unknown-linux-gnu",
        "-unknown-linux-musl",
        "-pc-windows-msvc",
        "-pc-windows-gnu",
    ];
    let lower = value.to_ascii_lowercase();
    lower
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && TARGET_SUFFIXES.iter().any(|suffix| lower.contains(suffix))
}

fn looks_like_isolate_sentinel(value: &str) -> bool {
    let Some(body) = value
        .strip_prefix(r"\u{2068}")
        .and_then(|rest| rest.strip_suffix(r"\u{2069}"))
    else {
        return false;
    };
    body.contains('-')
        && body
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn logical_statements(content: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut line_comment = false;
    let mut block_comment = false;
    let mut block_comment_star = false;
    let mut escaped = false;
    let mut nesting = 0_u16;

    for ch in content.chars() {
        current.push(ch);
        if line_comment {
            // Comment punctuation is prose, not call nesting.
        } else if block_comment {
            if block_comment_star && ch == '/' {
                block_comment = false;
            }
            block_comment_star = ch == '*';
        } else if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
        } else {
            match ch {
                '\'' | '"' | '`' => quote = Some(ch),
                '/' if current.ends_with("//") => line_comment = true,
                '*' if current.ends_with("/*") => {
                    block_comment = true;
                    block_comment_star = false;
                }
                '(' => nesting = nesting.saturating_add(1),
                ')' => nesting = nesting.saturating_sub(1),
                _ => {}
            }
        }

        let boundary = (ch == ';' && quote.is_none()) || (ch == '\n' && nesting == 0);
        if boundary {
            push_statement(&mut statements, &mut current);
        }
        // A line comment always ends at the physical newline, including when
        // it appears inside a multiline call whose parenthesis depth is still
        // non-zero. Keeping the flag set in that case makes the rest of the
        // file look like one comment-backed statement and can correlate an
        // unrelated password example with a later `save()` call.
        if ch == '\n' && line_comment {
            line_comment = false;
        }
        if ch == '\n' && boundary {
            quote = None;
            escaped = false;
        }
    }
    push_statement(&mut statements, &mut current);
    statements
}

fn push_statement(statements: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty()
        && !trimmed.starts_with("//")
        && !trimmed.starts_with('#')
        && !trimmed.starts_with('*')
    {
        statements.push(trimmed.to_string());
    }
    current.clear();
}

fn has_password_token(statement: &str) -> bool {
    ["password", "passwd", "pwd"]
        .into_iter()
        .any(|name| contains_password_identifier(statement, name))
}

fn contains_password_identifier(statement: &str, name: &str) -> bool {
    statement.match_indices(name).any(|(index, _)| {
        identifier_boundary(statement, index, name.len())
            && statement[..index]
                .bytes()
                .next_back()
                .is_none_or(|byte| !matches!(byte, b'/' | b'\\'))
    })
}

fn has_persistence_operation(statement: &str) -> bool {
    let sql = (statement.contains("insert into")
        || statement.contains("create user")
        || (statement.contains("update ") && statement.contains(" set ")))
        && has_password_token(statement);
    sql || ["insert", "insert_into", "save", "create"]
        .into_iter()
        .any(|name| contains_named_call(statement, name))
}

fn contains_named_call(statement: &str, name: &str) -> bool {
    statement.match_indices(name).any(|(index, _)| {
        identifier_boundary(statement, index, name.len())
            && statement[index + name.len()..]
                .trim_start()
                .starts_with('(')
    })
}

/// Password hashing calls. Django's `make_password` / `set_password` hash too.
const PASSWORD_HASHERS: &[&str] = &[
    "bcrypt",
    "argon2",
    "scrypt",
    "pbkdf2",
    "hashpassword",
    "hash_password",
    "password.hash",
    "hash(",
    "make_password",
    "set_password",
];

fn has_password_hasher(statement: &str) -> bool {
    PASSWORD_HASHERS
        .iter()
        .any(|marker| statement.contains(marker))
}

fn assigned_identifier_before_hasher(statement: &str) -> Option<&str> {
    let hasher_index = PASSWORD_HASHERS
        .iter()
        .filter_map(|marker| statement.find(marker))
        .min()?;
    let assignment = statement[..hasher_index].rfind('=')?;
    let bytes = statement.as_bytes();
    let mut end = assignment;
    while end > 0 && (bytes[end - 1].is_ascii_whitespace() || bytes[end - 1] == b':') {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    (start < end).then(|| &statement[start..end])
}

fn password_value_is_hash_named(statement: &str) -> bool {
    ["password", "passwd", "pwd"].into_iter().any(|name| {
        statement.match_indices(name).any(|(index, _)| {
            if !identifier_boundary(statement, index, name.len()) {
                return false;
            }
            let after = statement[index + name.len()..].trim_start_matches(['"', '\'', ' ', '\t']);
            let Some(value) = after.strip_prefix(':').or_else(|| after.strip_prefix('=')) else {
                return false;
            };
            let identifier: String = value
                .trim_start()
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            identifier.contains("hash") || identifier.contains("digest")
        })
    })
}

fn unhashed_password_assignment_owner(statement: &str) -> Option<&str> {
    if has_password_hasher(statement) || password_value_is_hash_named(statement) {
        return None;
    }
    for (index, _) in statement.match_indices("password") {
        if !identifier_boundary(statement, index, "password".len()) {
            continue;
        }
        let after = statement[index + "password".len()..].trim_start();
        if !after.starts_with('=') || after.starts_with("==") {
            continue;
        }
        let prefix = &statement[..index];
        let Some(dot) = prefix.rfind('.') else {
            continue;
        };
        if !prefix[dot + 1..].trim().is_empty() {
            continue;
        }
        let bytes = prefix.as_bytes();
        let mut start = dot;
        while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
            start -= 1;
        }
        if start < dot {
            return Some(&prefix[start..dot]);
        }
    }
    None
}

fn contains_identifier(text: &str, identifier: &str) -> bool {
    text.match_indices(identifier)
        .any(|(index, _)| identifier_boundary(text, index, identifier.len()))
}

fn identifier_boundary(text: &str, index: usize, len: usize) -> bool {
    let before = text[..index].bytes().next_back();
    let after = text[index + len..].bytes().next();
    before.is_none_or(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
        && after.is_none_or(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
}

#[cfg(test)]
mod tests {
    use super::contains_unhashed_password_storage;

    #[test]
    fn line_comment_inside_multiline_call_does_not_consume_following_code() {
        let source = r"
            let value = build(
                // This comment ends before the next argument.
                input,
            );
            /// owner.password = value; owner.save()
            let safe = true;
        ";
        assert!(!contains_unhashed_password_storage(source));
    }

    #[test]
    fn multiline_password_persistence_is_still_detected() {
        let source = r"
            db.insert(
                // Keeping the call multiline must not hide the field.
                { password: input_password },
            );
        ";
        assert!(contains_unhashed_password_storage(source));
    }
}
