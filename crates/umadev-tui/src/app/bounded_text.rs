use std::path::Path;

use unicode_segmentation::UnicodeSegmentation;

pub(super) fn read_utf8(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    String::from_utf8(umadev_state::fs::read_bounded(path, max_bytes)?).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed text is not UTF-8: {error}"),
        )
    })
}

/// Retain a bounded tail without starting it in the middle of one visible
/// grapheme. Scalar-aligned truncation is valid UTF-8 but can still leave an
/// orphan skin-tone modifier, combining mark, or ZWJ half at the front of a
/// long process log. Drop the whole intersected grapheme instead; the retained
/// tail therefore never exceeds `max_chars`.
pub(super) fn trim_tail(output: &mut String, max_chars: usize) {
    let chars = output.chars().count();
    if chars <= max_chars {
        return;
    }
    let must_drop = chars - max_chars;
    let mut dropped = 0usize;
    let mut byte = 0usize;
    for (start, grapheme) in output.grapheme_indices(true) {
        if dropped >= must_drop {
            byte = start;
            break;
        }
        dropped = dropped.saturating_add(grapheme.chars().count());
        byte = start.saturating_add(grapheme.len());
    }
    output.drain(..byte);
}

/// Take a scalar-count-bounded prefix, stopping before a grapheme that would
/// cross the cap. This may intentionally leave a little room unused: a visible
/// glyph is either retained whole or not retained at all.
pub(super) fn prefix_with_char_limit(text: &str, max_chars: usize) -> String {
    let mut output = String::new();
    let mut chars = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_chars = grapheme.chars().count();
        if chars.saturating_add(grapheme_chars) > max_chars {
            break;
        }
        output.push_str(grapheme);
        chars += grapheme_chars;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{prefix_with_char_limit, trim_tail};

    #[test]
    fn prefix_never_splits_an_extended_grapheme() {
        assert_eq!(prefix_with_char_limit("ab👩‍💻cd", 4), "ab");
        assert_eq!(prefix_with_char_limit("ab👩‍💻cd", 5), "ab👩‍💻");
    }

    #[test]
    fn tail_never_starts_inside_an_extended_grapheme() {
        let mut output = "ab👩‍💻cd".to_string();
        trim_tail(&mut output, 4);
        assert_eq!(output, "cd");
    }
}
