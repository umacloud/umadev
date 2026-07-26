//! Control-flow nesting analysis for `UG-LINT-004`.

use super::Decision;

/// Ban more than five nested control-flow scopes.
///
/// Structural braces belonging to modules, types, functions, data literals,
/// strings, comments, and JSX do not count. This keeps the rule aligned with
/// decision complexity rather than a file's lexical container depth.
#[must_use]
pub fn check_deep_nesting(file_path: &str, content: &str) -> Decision {
    let ext = file_path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "java" | "kt" | "rs" | "go" | "c" | "cpp" | "h"
    ) {
        return Decision::pass();
    }
    if super::looks_like_secret_test_path(file_path) {
        return Decision::pass();
    }
    let content = if ext == "rs" {
        super::rust_shipping_prefix(content)
    } else {
        content
    };

    let tokenized = crate::tokenizer::Tokenized::new(content);
    let code = tokenized.code_only_preserving_lines(content);
    let mut stack: Vec<bool> = Vec::new();
    let mut control_depth = 0usize;
    let mut max_depth = 0usize;
    let mut max_depth_line = 1usize;
    let mut line = 1usize;
    let mut opener = String::new();
    for ch in code.chars() {
        match ch {
            '{' => {
                let control = is_control_flow_opener(&opener);
                stack.push(control);
                control_depth += usize::from(control);
                if control_depth > max_depth {
                    max_depth = control_depth;
                    max_depth_line = line;
                }
                opener.clear();
            }
            '}' => {
                if stack.pop().unwrap_or(false) {
                    control_depth = control_depth.saturating_sub(1);
                }
                opener.clear();
            }
            ';' => opener.clear(),
            '\n' => {
                line = line.saturating_add(1);
                opener.push(ch);
            }
            _ => {
                opener.push(ch);
                if opener.len() > 512 {
                    let mut split = opener.len() - 256;
                    while split < opener.len() && !opener.is_char_boundary(split) {
                        split += 1;
                    }
                    opener.drain(..split);
                }
            }
        }
    }

    if max_depth <= 5 {
        return Decision::pass();
    }
    Decision::block(
        "UG-LINT-004",
        format!(
            "UmaDev: excessively deep nesting at `{file_path}:{max_depth_line}` (UG-LINT-004). \
             The code nests {max_depth} control-flow levels deep — code this nested is \
             unreadable and error-prone. Extract inner logic into helper functions, use early \
             returns (guard clauses), or flatten with `&&`/optional chaining. Target ≤4 levels \
             of nesting."
        ),
    )
}

fn is_control_flow_opener(prefix: &str) -> bool {
    let normalized = prefix
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let tail = normalized
        .rsplit([';', '{', '}'])
        .next()
        .unwrap_or(&normalized)
        .trim_start();
    [
        "if ", "if(", "else", "for ", "for(", "while ", "while(", "loop", "match ", "match(",
        "switch ", "switch(", "try", "catch ", "catch(", "do",
    ]
    .iter()
    .any(|keyword| tail.starts_with(keyword))
}
