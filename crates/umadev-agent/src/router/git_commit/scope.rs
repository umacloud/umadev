use std::collections::HashSet;

use super::quote::{QuoteEvent, QuoteTracker};

#[derive(Debug)]
struct GitScopeToken {
    value: String,
    quoted: bool,
}

pub(super) fn parse_git_commit_paths(text: &str) -> Option<Vec<String>> {
    let tokens = tokenize_git_scope(text)?;
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for token in tokens {
        let normalized = token.value.replace('\\', "/");
        if !git_scope_token_is_path(&normalized, token.quoted) {
            return None;
        }
        if seen.insert(normalized.clone()) {
            paths.push(normalized);
        }
    }
    Some(paths)
}

fn tokenize_git_scope(text: &str) -> Option<Vec<GitScopeToken>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote_tracker = QuoteTracker::new(true);
    let mut token_was_quoted = false;
    let flush =
        |tokens: &mut Vec<GitScopeToken>, current: &mut String, token_was_quoted: &mut bool| {
            let value = current
                .trim_matches(|ch: char| {
                    ch.is_whitespace()
                        || matches!(
                            ch,
                            ',' | '，' | '、' | ';' | '；' | ':' | '：' | '(' | ')' | '（' | '）'
                        )
                })
                .to_string();
            if !value.is_empty() {
                tokens.push(GitScopeToken {
                    value,
                    quoted: *token_was_quoted,
                });
            }
            current.clear();
            *token_was_quoted = false;
        };

    for ch in text.chars() {
        match quote_tracker.step(ch) {
            QuoteEvent::Opened => token_was_quoted = true,
            QuoteEvent::Closed | QuoteEvent::EscapePrefix => {}
            QuoteEvent::Inside | QuoteEvent::Escaped => current.push(ch),
            QuoteEvent::LiteralEscape => {
                current.push('\\');
                current.push(ch);
            }
            QuoteEvent::Outside
                if ch.is_whitespace()
                    || matches!(
                        ch,
                        ',' | '，' | '、' | ';' | '；' | ':' | '：' | '(' | ')' | '（' | '）'
                    ) =>
            {
                flush(&mut tokens, &mut current, &mut token_was_quoted);
            }
            QuoteEvent::Outside => current.push(ch),
        }
    }
    if !quote_tracker.is_balanced() {
        return None;
    }
    flush(&mut tokens, &mut current, &mut token_was_quoted);
    Some(tokens)
}

fn git_scope_token_is_path(token: &str, quoted: bool) -> bool {
    if token.is_empty() || token.starts_with('-') || token.chars().any(char::is_control) {
        return false;
    }
    if quoted {
        return true;
    }
    let lower = token.to_lowercase();
    if token.contains('/')
        || token.starts_with('.')
        || matches!(
            lower.as_str(),
            "makefile"
                | "dockerfile"
                | "license"
                | "licence"
                | "readme"
                | "cargo.lock"
                | "gemfile"
                | "rakefile"
                | "procfile"
                | "justfile"
        )
    {
        return true;
    }
    let Some((stem, extension)) = token.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && !extension.is_empty()
        && extension
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '+'))
}

/// Lowercase projection of user-authored control text with every quoted region
/// blanked out. Deterministic intent belts use this instead of matching raw
/// prose, so a label such as “read-only analysis” cannot become authority.
pub(in crate::router) fn unquoted_lowercase_text(requirement: &str) -> String {
    let lower = requirement.trim().to_lowercase();
    let mut outside = String::with_capacity(lower.len());
    let mut quotes = QuoteTracker::new(true);
    for ch in lower.chars() {
        match quotes.step(ch) {
            QuoteEvent::Outside => outside.push(ch),
            QuoteEvent::Opened
            | QuoteEvent::Inside
            | QuoteEvent::EscapePrefix
            | QuoteEvent::Escaped
            | QuoteEvent::LiteralEscape
            | QuoteEvent::Closed => outside.push(' '),
        }
    }

    outside
}

pub(in crate::router) fn git_commit_control_text(requirement: &str) -> String {
    let outside = unquoted_lowercase_text(requirement);

    let mut end = outside.len();
    for marker in [
        "消息写",
        "消息为",
        "消息是",
        "提交消息",
        "提交信息",
        "提交訊息",
        "commit message",
        "with message",
        "message:",
        "message：",
    ] {
        if let Some(index) = outside.find(marker) {
            end = end.min(index);
        }
    }
    outside[..end].trim().to_string()
}

pub(in crate::router) fn git_commit_scope_text(requirement: &str) -> String {
    let text = requirement.trim();
    let mut end = text.len();
    for marker in [
        "消息写",
        "消息为",
        "消息是",
        "提交消息",
        "提交信息",
        "提交訊息",
        "commit message",
        "with message",
        "message:",
        "message：",
        " -m ",
        " -m\"",
        " -m'",
        " --message",
    ] {
        if let Some(index) = find_unquoted_case_insensitive(text, marker) {
            end = end.min(index);
        }
    }
    text[..end].trim().to_string()
}

fn find_unquoted_case_insensitive(text: &str, marker: &str) -> Option<usize> {
    let mut quotes = QuoteTracker::new(true);
    for (index, ch) in text.char_indices() {
        if matches!(quotes.step(ch), QuoteEvent::Outside)
            && marker_matches_case_insensitive(&text[index..], marker)
        {
            return Some(index);
        }
    }
    None
}

pub(super) fn marker_matches_case_insensitive(text: &str, marker: &str) -> bool {
    if marker.is_ascii() {
        text.get(..marker.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(marker))
    } else {
        text.starts_with(marker)
    }
}
