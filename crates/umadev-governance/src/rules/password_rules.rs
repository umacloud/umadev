//! Plaintext passwords: the `==` comparison that belongs to the bypass-immune
//! write floor (**UD-SEC-018**), and the lexical storage heuristic that is
//! overridable QC work (**UD-SEC-033**).

use super::{
    extension_of, looks_like_secret_test_path, rust_shipping_prefix, strip_string_literals,
    Decision,
};

/// The backend source these rules read, or `None` for another file type or a
/// test / fixture path. Rust files are read up to their trailing test module.
fn backend_source<'a>(file_path: &str, content: &'a str) -> Option<&'a str> {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "py" | "rb" | "go" | "java" | "rs"
    ) || looks_like_secret_test_path(file_path)
    {
        return None;
    }
    Some(if ext == "rs" {
        rust_shipping_prefix(content)
    } else {
        content
    })
}

/// **UD-SEC-018**: ban comparing a password in plaintext with `==` / `===`.
///
/// A stored password must be verified with `bcrypt.compare(input, hash)` (or
/// the argon2 / scrypt equivalent); `user.password === inputPassword` means
/// it is stored or handled in plaintext. Runs on backend source and belongs to
/// the irreversible write floor.
///
/// Only a comparison whose other side could be a password counts: a
/// confirm-password check (`data.password === data.confirmPassword`), an empty
/// or null check (`password === ''`, `req.Password == ""`), and a length check
/// (`len(password) == 0`) are ordinary validation. Storage without a visible
/// hash is judged separately by [`check_unhashed_password_storage`].
#[must_use]
pub fn check_plaintext_password(file_path: &str, content: &str) -> Decision {
    let Some(content) = backend_source(file_path, content) else {
        return Decision::pass();
    };
    let compares = content.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        let trimmed = lower.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            return false;
        }
        let code = strip_string_literals(&lower);
        !code.contains("bcrypt") && !code.contains("compare") && compares_password(&lower)
    });
    if !compares {
        return Decision::pass();
    }
    Decision::block(
        "UD-SEC-018",
        format!(
            "UmaDev: plaintext password comparison (UD-SEC-018). `{file_path}` compares a \
             password with `==`. Store only a bcrypt/argon2 hash and verify with \
             `bcrypt.compare(input, hash)`, never `==` — a plaintext comparison means the \
             password is stored or handled in plaintext, a credential-breach vector."
        ),
    )
}

/// **UD-SEC-033**: flag a password stored or created without a visible hash.
///
/// Persistence is correlated inside one logical statement or an adjacent
/// `owner.password = value; owner.save()` pair. An unrelated `HashMap::insert`,
/// plan save, or API example elsewhere in the file is not evidence of password
/// storage. Multi-line call arguments stay in one statement. A direct hash call
/// (bcrypt / argon2 / scrypt / pbkdf2 / Django `make_password`), a hash-named
/// password value, or a value assigned from a supported hasher is treated as
/// hashed. This is intentionally lexical: hashing that lives in the model (a
/// Mongoose pre-save hook, TypeORM `@BeforeInsert`, Rails
/// `has_secure_password`) is invisible here, so the finding is overridable QC
/// work that `.umadev/rules.toml` can disable — never the bypass-immune floor.
#[must_use]
pub fn check_unhashed_password_storage(file_path: &str, content: &str) -> Decision {
    let Some(content) = backend_source(file_path, content) else {
        return Decision::pass();
    };
    if !crate::security_context::contains_unhashed_password_storage(content) {
        return Decision::pass();
    }
    Decision::block(
        "UD-SEC-033",
        format!(
            "UmaDev: password stored without a visible hash (UD-SEC-033). `{file_path}` stores \
             or creates a password with no hashing call (bcrypt / argon2 / scrypt / \
             `make_password`) in the same statement. Hash it before storage. If the model \
             already hashes it (a Mongoose pre-save hook, TypeORM `@BeforeInsert`, Rails \
             `has_secure_password`), do not hash it twice: disable UD-SEC-033 for this \
             project in `.umadev/rules.toml`."
        ),
    )
}

/// Whether a lower-cased code line compares a password with `==` / `===`
/// against an operand that is not a confirmation field, an empty / null
/// literal, or a length.
fn compares_password(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let quoted = quoted_mask(&chars);
    let mut i = 0;
    while i + 1 < chars.len() {
        let operator = chars[i] == '='
            && chars[i + 1] == '='
            && !quoted[i]
            && !i
                .checked_sub(1)
                .is_some_and(|p| matches!(chars[p], '!' | '<' | '>' | '='));
        if !operator {
            i += 1;
            continue;
        }
        let end = if chars.get(i + 2) == Some(&'=') {
            i + 3
        } else {
            i + 2
        };
        let left = operand_before(&chars, i);
        let right = operand_after(&chars, end);
        if (names_password(&left) || names_password(&right))
            && !is_benign_operand(&left)
            && !is_benign_operand(&right)
        {
            return true;
        }
        i = end;
    }
    false
}

/// For each char, whether it is part of a quoted string literal (quotes included).
fn quoted_mask(chars: &[char]) -> Vec<bool> {
    let mut mask = Vec::with_capacity(chars.len());
    let mut quote: Option<char> = None;
    let mut previous = '\0';
    for &c in chars {
        match quote {
            Some(q) => {
                mask.push(true);
                if c == q && previous != '\\' {
                    quote = None;
                }
            }
            None if matches!(c, '\'' | '"' | '`') => {
                mask.push(true);
                quote = Some(c);
            }
            None => mask.push(false),
        }
        previous = c;
    }
    mask
}

/// Characters of an operand expression outside brackets: an identifier path
/// with `.`, `?.` and TypeScript's `!`.
fn operand_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '$' | '?' | '!')
}

/// The operand that ends right before `chars[end]`: a string literal, or an
/// identifier path with balanced call / index brackets (`len(password)`).
fn operand_before(chars: &[char], end: usize) -> String {
    let mut stop = end;
    while stop > 0 && chars[stop - 1].is_whitespace() {
        stop -= 1;
    }
    let Some(&last) = stop.checked_sub(1).and_then(|p| chars.get(p)) else {
        return String::new();
    };
    let mut start = stop - 1;
    if matches!(last, '\'' | '"' | '`') {
        while start > 0 {
            start -= 1;
            if chars[start] == last && (start == 0 || chars[start - 1] != '\\') {
                break;
            }
        }
        return chars[start..stop].iter().collect();
    }
    let mut depth = 0_usize;
    start = stop;
    while start > 0 {
        let c = chars[start - 1];
        match c {
            ')' | ']' => depth += 1,
            '(' | '[' if depth == 0 => break,
            '(' | '[' => depth -= 1,
            c if operand_char(c) || depth > 0 => {}
            _ => break,
        }
        start -= 1;
    }
    chars[start..stop].iter().collect()
}

/// The operand that starts at `chars[start]` (after whitespace).
fn operand_after(chars: &[char], start: usize) -> String {
    let mut begin = start;
    while chars.get(begin).is_some_and(|c| c.is_whitespace()) {
        begin += 1;
    }
    let Some(&first) = chars.get(begin) else {
        return String::new();
    };
    let mut end = begin + 1;
    if matches!(first, '\'' | '"' | '`') {
        while let Some(&c) = chars.get(end) {
            end += 1;
            if c == first && chars[end - 2] != '\\' {
                break;
            }
        }
        return chars[begin..end].iter().collect();
    }
    let mut depth = 0_usize;
    end = begin;
    while let Some(&c) = chars.get(end) {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' if depth == 0 => break,
            ')' | ']' => depth -= 1,
            c if operand_char(c) || depth > 0 => {}
            _ => break,
        }
        end += 1;
    }
    chars[begin..end].iter().collect()
}

/// The `.`-separated segments of an operand, without call / index brackets
/// and TypeScript's `!` / `?`.
fn segments(operand: &str) -> impl Iterator<Item = &str> {
    operand.split('.').map(|segment| {
        segment
            .split(['(', '['])
            .next()
            .unwrap_or(segment)
            .trim_matches(['!', '?'])
    })
}

fn names_password(operand: &str) -> bool {
    segments(operand).any(|segment| segment.ends_with("password"))
}

fn is_benign_operand(operand: &str) -> bool {
    let empty_or_null = operand.is_empty()
        || matches!(
            operand,
            "''" | "\"\"" | "``" | "null" | "undefined" | "nil" | "none" | "true" | "false"
        )
        || operand.bytes().all(|b| b.is_ascii_digit());
    let length = [
        ".length",
        ".length()",
        ".size",
        ".size()",
        ".count",
        ".len()",
    ]
    .iter()
    .any(|suffix| operand.ends_with(suffix))
        || ["len(", "strlen(", "mb_strlen(", "utf8.runecountinstring("]
            .iter()
            .any(|prefix| operand.starts_with(prefix));
    let confirmation = segments(operand).any(|segment| {
        ["confirm", "repeat", "retype", "again"]
            .iter()
            .any(|marker| segment.contains(marker))
            || matches!(
                segment,
                "repassword" | "re_password" | "password2" | "password_2"
            )
    });
    empty_or_null || length || confirmation
}
