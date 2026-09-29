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

/// Words every request carries that say nothing about WHAT it is about: English
/// function words and generic request verbs and nouns, and their Chinese
/// counterparts (filler bigrams such as 一个 / 做一, generic verbs such as
/// 支持 / 实现, generic product nouns such as 系统 / 应用). Memory curation
/// ignores them, so two unrelated requests never look alike merely because both
/// are phrased as requests.
const FILLER_TERMS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "using",
    "use",
    "add",
    "make",
    "build",
    "create",
    "app",
    "application",
    "system",
    "page",
    "please",
    "that",
    "this",
    "into",
    "from",
    "new",
    "a",
    "an",
    "of",
    "to",
    "in",
    "on",
    "it",
    "is",
    "一个",
    "做一",
    "做个",
    "一下",
    "帮我",
    "请帮",
    "我们",
    "这个",
    "那个",
    "需要",
    "可以",
    "进行",
    "使用",
    "添加",
    "增加",
    "新增",
    "制作",
    "构建",
    "创建",
    "开发",
    "实现",
    "支持",
    "包含",
    "应用",
    "系统",
    "页面",
    "程序",
    "功能",
    "项目",
];

/// Whether `term` (lowercase) is request filler rather than subject matter; see
/// [`FILLER_TERMS`].
pub(crate) fn is_filler_term(term: &str) -> bool {
    FILLER_TERMS.contains(&term)
}
