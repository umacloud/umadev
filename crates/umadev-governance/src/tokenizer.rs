//! Lightweight JS/TSX tokeniser for governance analysis.
//!
//! UmaDev's code-rule checks (UD-CODE-001/002/005) used to scan the raw
//! source text with regexes. That produced two classes of false result:
//! - **False positives**: a hex color `#fff` or emoji in a *comment* or a
//!   *string literal that is clearly data* (e.g. a test fixture) triggered
//!   a block.
//! - **False negatives**: a color split across a template literal
//!   (`` `#${c}` ``) slipped past the single-token regex.
//!
//! A full TSX parser (`swc`) is the ideal tool but its proc-macro dependency
//! tree is currently incompatible with a clean `cargo` build (syn version
//! drift in `swc_visit_macros`). Rather than pin a fragile version matrix,
//! this module implements the *minimal* lexical analysis the governance
//! rules need: it walks the source once, classifying every character into
//! "code", "string-literal", "jsx-text", or "comment", and exposes filtered
//! views the rules query.
//!
//! This is intentionally NOT a parser — it does not build an AST, does not
//! understand scope, and does not validate syntax. It is a state machine
//! that knows just enough about JS/TSX delimiters to separate the regions
//! the rules care about. That keeps it dependency-free and ~150 lines.

/// One contiguous region of source, classified by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// Executable code (default).
    Code,
    /// Inside a string / template / char literal — the *content* matters
    /// (a hardcoded color in a string is still a violation), but the
    /// surrounding quotes are stripped so the rules don't re-match them.
    StringLiteral,
    /// Text between JSX tags (`<button>SEARCH</button>` → `SEARCH`). This is
    /// where emoji-as-icon violations actually live.
    JsxText,
    /// Line (`//…`) or block (`/*…*/`) comment — governance rules MUST skip
    /// these (a `#fff` in a comment is not a violation).
    Comment,
}

/// A tokenised view of source: each `(start, end, Region)` span uses UTF-8
/// byte offsets. The rules iterate these to scan only the regions they care
/// about without rescanning the source to translate character offsets.
#[derive(Debug, Clone)]
pub struct Tokenized {
    spans: Vec<(usize, usize, Region)>,
}

pub(crate) fn allows_es5_bootstrap(src: &str) -> bool {
    src.lines()
        .take(20)
        .any(|line| line.trim() == "// umadev-governance: allow-es5-bootstrap")
}

impl Tokenized {
    /// Tokenise `src`. Produces a non-overlapping, ordered span list covering
    /// the entire source. Cheap to call (single pass).
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new(src: &str) -> Self {
        let bytes: Vec<char> = src.chars().collect();
        let n = bytes.len();
        let mut spans: Vec<(usize, usize, Region)> = Vec::new();
        let mut i = 0;
        // Track whether we're inside JSX text context. We enter JSX text when
        // we see `>` (end of an opening/self-closing tag) and exit on `<`
        // (start of the next tag). We only treat it as JSX text if there's
        // actual non-whitespace content — avoids spamming empty spans.
        let mut code_start = 0usize;
        let flush_code =
            |spans: &mut Vec<(usize, usize, Region)>, start: &mut usize, end: usize| {
                if end > *start {
                    spans.push((*start, end, Region::Code));
                }
                *start = end;
            };

        while i < n {
            let c = bytes[i];
            // --- Comments ---
            if c == '/' && i + 1 < n {
                // Line comment.
                if bytes[i + 1] == '/' {
                    flush_code(&mut spans, &mut code_start, i);
                    let start = i;
                    i += 2;
                    while i < n && bytes[i] != '\n' {
                        i += 1;
                    }
                    spans.push((start, i, Region::Comment));
                    code_start = i;
                    continue;
                }
                // Block comment.
                if bytes[i + 1] == '*' {
                    flush_code(&mut spans, &mut code_start, i);
                    let start = i;
                    i += 2;
                    while i + 1 < n && !(bytes[i] == '*' && bytes[i + 1] == '/') {
                        i += 1;
                    }
                    i = (i + 2).min(n);
                    spans.push((start, i, Region::Comment));
                    code_start = i;
                    continue;
                }
            }
            // --- String literals (', ", `) ---
            if c == '\'' || c == '"' || c == '`' {
                flush_code(&mut spans, &mut code_start, i);
                let quote = c;
                let start = i;
                i += 1;
                while i < n {
                    if bytes[i] == '\\' {
                        // Skip the escaped char; a trailing `\` at EOF must not
                        // step past the end (the span end indexes `byte_offsets`).
                        i = (i + 2).min(n);
                        continue;
                    }
                    if bytes[i] == quote {
                        i += 1;
                        break;
                    }
                    // Template literals can span lines and contain ${…}; we
                    // treat the whole `` ` … ` `` as one string span, which
                    // is conservative (a color inside ${} still flags).
                    i += 1;
                }
                spans.push((start, i, Region::StringLiteral));
                code_start = i;
                continue;
            }
            i += 1;
        }
        flush_code(&mut spans, &mut code_start, n);

        // Second pass: split Code spans into Code vs JsxText. JSX text is the
        // region between `>` (tag close) and the next `<`. We approximate by
        // scanning each Code span for `>` … `<` gaps.
        let mut refined: Vec<(usize, usize, Region)> = Vec::new();
        for &(s, e, region) in &spans {
            if region != Region::Code {
                refined.push((s, e, region));
                continue;
            }
            refine_code_span(s, e, &bytes, &mut refined);
        }

        // Lexing is easiest in character space, while every later slice must
        // be O(1). Translate the finished spans once instead of walking
        // `src.char_indices()` again for every comment/string span. The old
        // per-span translation made a comment-heavy large file quadratic.
        let byte_offsets = src
            .char_indices()
            .map(|(byte, _)| byte)
            .chain(std::iter::once(src.len()))
            .collect::<Vec<_>>();
        let spans = refined
            .into_iter()
            .map(|(start, end, region)| (byte_offsets[start], byte_offsets[end], region))
            .collect();

        Self { spans }
    }

    /// Iterate spans matching `filter`, yielding the substring slices. Used
    /// by rules that want "all string literals" or "all JSX text".
    pub fn regions<'a>(&'a self, src: &'a str, filter: Region) -> impl Iterator<Item = &'a str> {
        self.spans.iter().filter_map(move |&(s, e, r)| {
            if r == filter {
                Some(byte_slice(src, s, e))
            } else {
                None
            }
        })
    }

    /// All source MINUS comments — for rules that want to scan everything a
    /// human could see (code + strings + JSX text) but skip comments.
    /// Returns the concatenation of non-comment spans.
    #[must_use]
    pub fn without_comments(&self, src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        for &(s, e, r) in &self.spans {
            if r != Region::Comment {
                out.push_str(byte_slice(src, s, e));
            }
        }
        out
    }

    /// Executable-code spans only, with strings, comments, and JSX text
    /// removed.  This is the right view for structural rules such as nesting:
    /// braces shown in a diagnostic string or a comment are data, not syntax.
    #[must_use]
    pub fn code_only(&self, src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        for piece in self.regions(src, Region::Code) {
            out.push_str(piece);
            // Keep adjacent regions from accidentally forming a new token.
            out.push(' ');
        }
        out
    }

    /// Return executable code while preserving the source's line structure.
    ///
    /// Non-code regions become spaces, except that their newlines are retained.
    /// Rules that report a source line can therefore analyze the sanitized view
    /// without letting comments, strings, or JSX text influence the result.
    #[must_use]
    pub fn code_only_preserving_lines(&self, src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        for &(start, end, region) in &self.spans {
            for ch in byte_slice(src, start, end).chars() {
                if region == Region::Code || ch == '\n' {
                    out.push(ch);
                } else {
                    out.push(' ');
                }
            }
        }
        out
    }

    /// Only JSX text spans concatenated — where emoji-as-icon violations live.
    #[must_use]
    pub fn jsx_text(&self, src: &str) -> String {
        let mut out = String::new();
        for piece in self.regions(src, Region::JsxText) {
            out.push_str(piece);
        }
        out
    }
}

fn refine_code_span(
    start: usize,
    end: usize,
    source: &[char],
    refined: &mut Vec<(usize, usize, Region)>,
) {
    let span = &source[start..end];
    let mut cursor = 0usize;
    let mut last = 0usize;
    while cursor < span.len() {
        if span[cursor] != '>' {
            cursor += 1;
            continue;
        }
        let mut next_tag = cursor + 1;
        while next_tag < span.len() && span[next_tag] != '<' {
            next_tag += 1;
        }
        let text = &span[cursor + 1..next_tag];
        let has_content = text.iter().any(|ch| !ch.is_whitespace());
        let looks_like_code = text
            .iter()
            .any(|ch| matches!(ch, ';' | '{' | '}' | '(' | ')' | '=' | '\''));
        if has_content && !looks_like_code {
            if cursor + 1 > last {
                refined.push((start + last, start + cursor + 1, Region::Code));
            }
            refined.push((start + cursor + 1, start + next_tag, Region::JsxText));
            last = next_tag;
        }
        cursor = next_tag;
    }
    if last < span.len() {
        refined.push((start + last, end, Region::Code));
    }
}

/// Slice a span that was translated to UTF-8 byte offsets at tokenization
/// time. A caller accidentally pairing a tokenization with different source
/// text fails open to an empty slice rather than panicking on stale offsets.
fn byte_slice(src: &str, start: usize, end: usize) -> &str {
    if start >= end {
        return "";
    }
    src.get(start..end).unwrap_or("")
}

/// Strip Rust double-quoted string content while leaving apostrophes alone.
/// Rust apostrophes introduce chars, labels, or lifetimes, so a generic
/// JavaScript-style single-quote stripper can erase real Rust syntax.
pub(crate) fn strip_double_quoted_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_str = false;
    let mut prev = '\0';
    for ch in line.chars() {
        if ch == '"' && prev != '\\' {
            in_str = !in_str;
            out.push(' ');
        } else if in_str {
            out.push(' ');
        } else {
            out.push(ch);
        }
        prev = ch;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_comment_isolated() {
        let t = Tokenized::new("const x = 1; // color: #fff");
        let no_comments = t.without_comments("const x = 1; // color: #fff");
        assert!(!no_comments.contains("#fff"));
        assert!(no_comments.contains("const x"));
    }

    #[test]
    fn block_comment_isolated() {
        let t = Tokenized::new("/* #9333ea is a placeholder */ const y = 2;");
        let no_comments = t.without_comments("/* #9333ea is a placeholder */ const y = 2;");
        assert!(!no_comments.contains("#9333ea"));
        assert!(no_comments.contains("const y"));
    }

    #[test]
    fn string_literal_kept() {
        // A color inside a string IS a violation (kept), but the quotes are
        // stripped from the surrounding context.
        let t = Tokenized::new("const c = '#9333ea';");
        let no_comments = t.without_comments("const c = '#9333ea';");
        assert!(no_comments.contains("#9333ea"));
    }

    #[test]
    fn code_only_excludes_strings_comments_and_jsx_text() {
        let src = "if (ready) { /* { */ const shown = \"}\"; <p>Label</p>; }";
        let t = Tokenized::new(src);
        let code = t.code_only(src);
        assert!(!code.contains("/*"));
        assert!(!code.contains("Label"));
        assert!(!code.contains('"'));
        assert_eq!(code.matches('{').count(), 1);
        assert_eq!(code.matches('}').count(), 1);
    }

    #[test]
    fn line_preserving_code_view_keeps_diagnostic_coordinates() {
        let src = "fn f() {\n  // hidden\n  let text = \"hidden\nvalue\";\n  if ready {}\n}\n";
        let t = Tokenized::new(src);
        let code = t.code_only_preserving_lines(src);
        assert_eq!(code.lines().count(), src.lines().count());
        assert_eq!(code.lines().nth(4).map(str::trim), Some("if ready {}"));
        assert!(!code.contains("hidden"));
        assert!(!code.contains("value"));
    }

    #[test]
    fn jsx_text_extracted() {
        let src = "const el = <button>🔍 Search</button>;";
        let t = Tokenized::new(src);
        let jsx = t.jsx_text(src);
        assert!(jsx.contains("Search"));
    }

    #[test]
    fn emoji_in_jsx_text_detected() {
        let src = "<div className=\"card\">🚀 Launch</div>";
        let t = Tokenized::new(src);
        let jsx = t.jsx_text(src);
        assert!(jsx.contains("🚀"));
    }

    #[test]
    fn emoji_in_comment_not_in_jsx() {
        let src = "// 🚀 todo\nconst x = 1;";
        let t = Tokenized::new(src);
        let jsx = t.jsx_text(src);
        assert!(!jsx.contains("🚀"));
    }

    #[test]
    fn template_literal_color_kept() {
        let src = "const c = `#${channel}`;";
        let t = Tokenized::new(src);
        let no_comments = t.without_comments(src);
        assert!(no_comments.contains('#'));
    }

    #[test]
    fn escaped_quote_in_string() {
        let src = "const s = 'it\\'s #fff here';";
        let t = Tokenized::new(src);
        let no_comments = t.without_comments(src);
        assert!(no_comments.contains("#fff"));
    }

    #[test]
    fn rust_double_quote_strip_keeps_lifetimes() {
        let line = "struct Owner<'a> { host: &'a str, url: \"http://localhost\" }";
        let stripped = strip_double_quoted_literals(line);
        assert!(stripped.contains("Owner<'a>"));
        assert!(stripped.contains("host: &'a str"));
        assert!(!stripped.contains("localhost"));
    }

    #[test]
    fn empty_source() {
        let t = Tokenized::new("");
        assert!(t.without_comments("").is_empty());
    }

    #[test]
    fn multiple_jsx_children() {
        let src = "<ul><li>A</li><li>B</li></ul>";
        let t = Tokenized::new(src);
        let jsx = t.jsx_text(src);
        assert!(jsx.contains('A'));
        assert!(jsx.contains('B'));
    }

    #[test]
    fn nested_tags_text_only_in_leaf() {
        let src = "<div><span>Click</span></div>";
        let t = Tokenized::new(src);
        assert!(t.jsx_text(src).contains("Click"));
    }

    #[test]
    fn unterminated_string_does_not_panic() {
        let src = "const s = 'unterminated";
        let t = Tokenized::new(src);
        // Must not loop forever or panic.
        let _ = t.without_comments(src);
    }

    #[test]
    fn tokenizer_trailing_backslash_in_open_string_does_not_panic() {
        // A file whose LAST char is a backslash while the lexer is inside an
        // unterminated quote (an apostrophe in JSX text opens a "string" that
        // runs to EOF) used to step the cursor past the end and index out of
        // bounds when the spans were translated to byte offsets.
        for src in ["a = '\\", "<p>Don't</p>\nx \\", "const s = \"\\", "`\\"] {
            let t = Tokenized::new(src);
            assert_eq!(t.without_comments(src), src, "{src:?}");
            let _ = t.jsx_text(src);
        }
        // The design scan tokenizes UI files without a panic guard of its own.
        let _ = crate::design::scan_design_quality("src/App.tsx", "<p>Don't</p>\nx \\");
    }

    #[test]
    fn ts_generic_not_treated_as_jsx_tag() {
        // Regression: a TypeScript generic `<T,>` was misread as a JSX
        // tag-open/close, so the code after it was scanned as JSX text.
        // With the generic guard (code punctuation in the span → Code), the
        // arrow-function body stays classified as Code.
        let src = "const f = <T,>(x: T): T => x;";
        let t = Tokenized::new(src);
        let jsx = t.jsx_text(src);
        assert!(
            jsx.is_empty() || !jsx.contains("=> x"),
            "TS generic body must NOT be JSX text; got: {jsx:?}"
        );
        // And a real JSX text node is still captured.
        let src2 = "const el = <div>Hello world</div>;";
        let t2 = Tokenized::new(src2);
        assert!(t2.jsx_text(src2).contains("Hello world"));
    }

    #[test]
    fn byte_slice_leading_empty_span_returns_empty() {
        // Regression: a leading empty span (start==0, end==0) previously
        // made end_byte stay 0, then the EOF guard set end_byte = src.len(),
        // returning the ENTIRE file for an empty span. It must return "".
        let src = "const x = 1;";
        assert_eq!(byte_slice(src, 0, 0), "");
        // Mid-source empty span is also empty, not the rest of the file.
        assert_eq!(byte_slice(src, 2, 2), "");
        // Inverted span (start > end) is empty, not negative-indexed garbage.
        assert_eq!(byte_slice(src, 5, 2), "");
        // Normal spans still work.
        assert_eq!(byte_slice(src, 0, 5), "const");
        // Span ending exactly at EOF returns the suffix.
        let n = src.len();
        assert_eq!(byte_slice(src, n - 2, n), "1;");
    }

    #[test]
    fn byte_slice_multibyte_source() {
        // Must not panic on valid multibyte byte boundaries or stale offsets.
        let src = "const 标签 = '🔍';";
        let start = src.find('标').unwrap_or(0);
        let end = start + "标签".len();
        let label = byte_slice(src, start, end);
        assert_eq!(label, "标签");
        assert_eq!(byte_slice(src, start + 1, end), "");
    }
}
