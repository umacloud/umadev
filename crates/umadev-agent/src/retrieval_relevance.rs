//! Shared lexical relevance for bounded project-memory recall.

use std::collections::HashSet;

pub(crate) fn terms(text: &str) -> HashSet<String> {
    fn flush_ascii(buffer: &mut String, out: &mut HashSet<String>) {
        let term = buffer.trim_matches(['-', '_']).to_lowercase();
        const STOPWORDS: &[&str] = &[
            "the",
            "and",
            "for",
            "with",
            "from",
            "into",
            "this",
            "that",
            "then",
            "when",
            "item",
            "open",
            "current",
            "continue",
            "build",
            "implement",
            "update",
            "fix",
        ];
        if term.chars().count() >= 3 && !STOPWORDS.contains(&term.as_str()) {
            out.insert(term);
        }
        buffer.clear();
    }

    fn flush_cjk(buffer: &mut Vec<char>, out: &mut HashSet<String>) {
        for pair in buffer.windows(2) {
            out.insert(pair.iter().collect());
        }
        buffer.clear();
    }

    fn is_cjk(character: char) -> bool {
        matches!(
            character as u32,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
        )
    }

    let mut out = HashSet::new();
    let mut ascii = String::new();
    let mut cjk = Vec::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
            flush_cjk(&mut cjk, &mut out);
            ascii.push(character);
        } else if is_cjk(character) {
            flush_ascii(&mut ascii, &mut out);
            cjk.push(character);
        } else {
            flush_ascii(&mut ascii, &mut out);
            flush_cjk(&mut cjk, &mut out);
        }
    }
    flush_ascii(&mut ascii, &mut out);
    flush_cjk(&mut cjk, &mut out);
    out
}

pub(crate) fn shares_term(text: &str, query: &HashSet<String>) -> bool {
    terms(text).iter().any(|term| query.contains(term))
}
