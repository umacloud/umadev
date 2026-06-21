//! Deterministic design-quality detector (UD-CODE-002 family).
//!
//! A dependency-free, fail-open scanner for the "AI-slop" fingerprints that
//! independent design tools converge on: the AI indigo/purple palette,
//! gradient text, overused default fonts as the primary face, bounce/elastic
//! easing, marketing buzzword copy, and invented metrics. Every check is a
//! pure string/number scan over comment-stripped source — no DOM, no regex
//! engine — so it stays cheap and never panics.
//!
//! Severity is advisory: HARD = a near-certain slop tell that should be fixed,
//! SOFT = a quality signal worth flagging. Callers decide whether to block,
//! score, or warn — the governance contract stays fail-open (an exceptional
//! input yields an empty finding list, never an error).

use crate::tokenizer::Tokenized;

/// File extensions this detector scans — UI code AND stylesheets (design
/// tells live in both `.tsx` and `.css`).
const DESIGN_EXTS: &[&str] = &[
    "tsx", "ts", "jsx", "js", "vue", "svelte", "astro", "css", "scss", "sass", "less", "html",
];

/// Lowercased file extension after the last `.` (empty if none).
fn ext_of(file_path: &str) -> String {
    file_path
        .rsplit('.')
        .next()
        .filter(|e| *e != file_path)
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// How strongly a design finding should be treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesignSeverity {
    /// Near-certain AI-slop tell — should be fixed before shipping.
    Hard,
    /// Quality signal — flag for review.
    Soft,
}

/// One design-quality finding.
#[derive(Debug, Clone)]
pub struct DesignFinding {
    /// Stable rule id (e.g. `ai-purple`).
    pub rule: &'static str,
    /// How strongly to treat it.
    pub severity: DesignSeverity,
    /// Human-readable explanation + fix direction.
    pub note: String,
}

/// The canonical AI indigo/purple hexes every anti-slop tool flags. Their mere
/// presence in UI source (even in a token file) is a strong generic tell.
const AI_PURPLE_HEXES: &[&str] = &[
    "#6366f1", "#7c3aed", "#8b5cf6", "#a855f7", "#9333ea", "#7e22ce", "#6d28d9", "#764ba2",
    "#667eea", "#5a67d8", "#818cf8", "#a78bfa",
];

/// Default font families that read as "AI generated" when used as the PRIMARY
/// face. Allowed as a fallback in a stack, flagged as the lead font.
const OVERUSED_FONTS: &[&str] = &[
    "inter",
    "roboto",
    "open sans",
    "lato",
    "montserrat",
    "poppins",
    "nunito",
    "arial",
    "helvetica",
];

/// Marketing buzzwords that signal generic, non-product-specific copy.
const BUZZWORDS: &[&str] = &[
    "streamline your",
    "empower your",
    "supercharge",
    "unleash",
    "leverage the power",
    "best-in-class",
    "industry-leading",
    "enterprise-grade",
    "next-generation",
    "cutting-edge",
    "revolutionize",
    "game-changer",
    "game changer",
    "mission-critical",
    "world-class",
    "seamless experience",
    "future-proof",
];

/// Placeholder identities that must never ship in real copy.
const PLACEHOLDER_NAMES: &[&str] = &[
    "jane doe",
    "john doe",
    "john smith",
    "acme corp",
    "acme inc",
    "acme co",
];

/// Scan one UI source file for design-quality tells. Returns an empty list for
/// non-UI files or clean input. Fail-open: never errors, never panics.
#[must_use]
#[allow(clippy::too_many_lines)] // a flat checklist of independent detectors
pub fn scan_design_quality(file_path: &str, content: &str) -> Vec<DesignFinding> {
    let ext = ext_of(file_path);
    if !DESIGN_EXTS.contains(&ext.as_str()) {
        return Vec::new();
    }
    // Scan code + strings + JSX text, skipping comments — the same view the
    // emoji/color rules use, so a comment can't trip or hide a finding.
    let tz = Tokenized::new(content);
    let body = tz.without_comments(content);
    let lower = body.to_ascii_lowercase();

    let mut out = Vec::new();

    // 1. AI indigo/purple palette.
    if let Some(hex) = AI_PURPLE_HEXES.iter().find(|h| lower.contains(**h)) {
        out.push(DesignFinding {
            rule: "ai-purple",
            severity: DesignSeverity::Hard,
            note: format!(
                "AI-slop indigo/purple `{hex}` — the single most recognizable AI tell. \
                 Commit to a distinctive brand hue from the chosen design system instead."
            ),
        });
    }

    // 2. Gradient text (`background-clip: text` over a gradient) — no genre
    //    legitimately ships this; it's a hero "gradient headline" tell.
    let clips_text = (lower.contains("background-clip: text")
        || lower.contains("background-clip:text")
        || lower.contains("-webkit-background-clip: text")
        || lower.contains("-webkit-background-clip:text")
        || lower.contains("bg-clip-text"))
        && lower.contains("gradient");
    if clips_text {
        out.push(DesignFinding {
            rule: "gradient-text",
            severity: DesignSeverity::Soft,
            note: "Gradient text (background-clip: text over a gradient) reads as AI-generated — \
                   use a solid token color for headings."
                .into(),
        });
    }

    // 3. Overused default font as the PRIMARY face. Look at font-family / --font
    //    declarations and flag when the FIRST family is a generic default.
    if let Some(font) = overused_primary_font(&lower) {
        out.push(DesignFinding {
            rule: "overused-font",
            severity: DesignSeverity::Soft,
            note: format!(
                "`{font}` as the primary typeface is an AI default — pick a distinctive display \
                 font (it may remain in the fallback stack)."
            ),
        });
    }

    // 4. Bounce / elastic easing — an overshooting cubic-bezier.
    if has_overshoot_easing(&lower) {
        out.push(DesignFinding {
            rule: "bounce-easing",
            severity: DesignSeverity::Soft,
            note: "Bounce/elastic easing (overshooting cubic-bezier) reads as toy-like — use a \
                   crafted ease-out such as cubic-bezier(0.16, 1, 0.3, 1)."
                .into(),
        });
    }

    // 5. Marketing buzzword copy (≥2 distinct → generic voice).
    let hits: Vec<&str> = BUZZWORDS
        .iter()
        .filter(|b| lower.contains(**b))
        .copied()
        .collect();
    if hits.len() >= 2 {
        out.push(DesignFinding {
            rule: "buzzwords",
            severity: DesignSeverity::Soft,
            note: format!(
                "Generic marketing buzzwords ({}) — write product-specific copy that names the \
                 real benefit.",
                hits.join(", ")
            ),
        });
    }

    // 6. Invented metrics / fake social proof.
    if let Some(metric) = invented_metric(&lower) {
        out.push(DesignFinding {
            rule: "invented-metrics",
            severity: DesignSeverity::Soft,
            note: format!(
                "Invented metric \"{metric}\" — never ship unverifiable stats; use real numbers \
                 or remove the claim."
            ),
        });
    }

    // 7. AI cream/beige surface — the warm off-white default (`--paper/--sand`
    //    territory) that signals a templated palette.
    if let Some(hex) = cream_band_hex(&lower) {
        out.push(DesignFinding {
            rule: "cream-band",
            severity: DesignSeverity::Soft,
            note: format!(
                "AI cream/beige surface `{hex}` — a templated warm off-white. Use the chosen \
                 system's surface token (a near-white with the brand's own slight temperature)."
            ),
        });
    }

    // 8. Em-dash overuse — a strong machine-writing tell at scale.
    let em_dashes = body.matches('\u{2014}').count();
    if em_dashes >= 5 {
        out.push(DesignFinding {
            rule: "em-dash-overuse",
            severity: DesignSeverity::Soft,
            note: format!(
                "{em_dashes} em-dashes — overuse reads as machine-written; prefer periods/commas."
            ),
        });
    }

    // 9. Placeholder identities in shipped copy.
    if let Some(name) = PLACEHOLDER_NAMES.iter().find(|n| lower.contains(**n)) {
        out.push(DesignFinding {
            rule: "placeholder-name",
            severity: DesignSeverity::Soft,
            note: format!(
                "Placeholder identity \"{name}\" — use realistic, product-specific names."
            ),
        });
    }

    out
}

/// Find a 6-digit hex that lands in the AI "cream/beige" band: very light,
/// warm, and red≥green≥blue (`min(r,g,b)≥209`, warmth `(r-b)∈[6,48]`).
fn cream_band_hex(lower: &str) -> Option<String> {
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' && i + 7 <= bytes.len() {
            let hex = &lower[i + 1..i + 7];
            if hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
                let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
                let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
                let warmth = i32::from(r) - i32::from(b);
                if r.min(g).min(b) >= 209 && r >= g && g >= b && (6..=48).contains(&warmth) {
                    return Some(format!("#{hex}"));
                }
            }
            i += 7;
        } else {
            i += 1;
        }
    }
    None
}

/// Generic/system font families that are universal fallbacks — they may appear
/// in any stack and never need to be declared in a design contract.
const GENERIC_FONTS: &[&str] = &[
    "sans-serif",
    "serif",
    "monospace",
    "system-ui",
    "ui-sans-serif",
    "ui-serif",
    "ui-monospace",
    "ui-rounded",
    "-apple-system",
    "blinkmacsystemfont",
    "segoe ui",
    "arial",
    "helvetica",
    "helvetica neue",
    "roboto",
    "cursive",
    "fantasy",
    "emoji",
    "math",
    "inherit",
    "initial",
    "unset",
    "noto sans",
    "apple color emoji",
    "segoe ui emoji",
];

/// Whether `name` (lowercased, unquoted) is a universal/system fallback font.
#[must_use]
pub fn is_generic_font(name: &str) -> bool {
    GENERIC_FONTS.contains(&name)
}

/// Extract every font-family name referenced in source — the lead and fallback
/// families of each `font-family:` / `--font-*:` declaration, lowercased and
/// unquoted. Used to cross-check generated code against the locked UIUX
/// typography contract (a code font absent from the contract = drift).
#[must_use]
pub fn extract_fonts(content: &str) -> Vec<String> {
    const MARKERS: &[&str] = &[
        "font-family:",
        "--font-display:",
        "--font-sans:",
        "--font-heading:",
        "--font-body:",
        "--font-mono:",
        "--font-serif:",
    ];
    let lower = content.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    for marker in MARKERS {
        let mut from = 0;
        while let Some(idx) = lower[from..].find(marker) {
            let start = from + idx + marker.len();
            let decl = &lower[start..];
            // char-safe cap: byte 160 may land mid-char on a long CJK stack.
            let end = decl
                .find([';', '\n', '}', '{'])
                .unwrap_or_else(|| floor_boundary(decl, 160));
            for fam in decl[..end].split(',') {
                let f = fam.trim().trim_matches(['"', '\'', ' ', '`']);
                if !f.is_empty() && f.len() < 40 && !f.contains("var(") && !f.contains('$') {
                    let f = f.to_string();
                    if !out.contains(&f) {
                        out.push(f);
                    }
                }
            }
            from = start;
        }
    }
    out
}

/// Find a generic default font used as the leading family in a `font-family`
/// or `--font-*` declaration (the primary face), ignoring fallback position.
fn overused_primary_font(lower: &str) -> Option<&'static str> {
    for marker in [
        "font-family:",
        "--font-display:",
        "--font-sans:",
        "--font-heading:",
    ] {
        let mut from = 0;
        while let Some(idx) = lower[from..].find(marker) {
            let start = from + idx + marker.len();
            let decl = &lower[start..];
            // The value up to the line/statement end (char-safe cap).
            let end = decl
                .find([';', '\n', '}'])
                .unwrap_or_else(|| floor_boundary(decl, 120));
            let value = decl[..end].trim().trim_start_matches(['"', '\'', ' ']);
            // The FIRST family is everything up to the first comma.
            let first = value
                .split(',')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(['"', '\'']);
            if let Some(f) = OVERUSED_FONTS.iter().find(|f| first == **f) {
                return Some(f);
            }
            from = start;
        }
    }
    None
}

/// Detect an overshooting `cubic-bezier(...)` (bounce/elastic): the y1 or y2
/// control points fall outside `[-0.1, 1.1]`.
fn has_overshoot_easing(lower: &str) -> bool {
    let mut from = 0;
    while let Some(idx) = lower[from..].find("cubic-bezier(") {
        let start = from + idx + "cubic-bezier(".len();
        let Some(close_rel) = lower[start..].find(')') else {
            return false;
        };
        let inner = &lower[start..start + close_rel];
        let nums: Vec<f64> = inner
            .split(',')
            .filter_map(|p| p.trim().parse::<f64>().ok())
            .collect();
        if nums.len() == 4 && (nums[1] > 1.1 || nums[1] < -0.1 || nums[3] > 1.1 || nums[3] < -0.1) {
            return true;
        }
        from = start + close_rel;
    }
    false
}

/// Largest UTF-8 char boundary at or below `idx` (clamped to `s.len()`), so
/// `&s[..floor_boundary(s, idx)]` never panics on a multibyte char.
fn floor_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Detect an invented marketing metric (`trusted by 50,000+`, `99.9% uptime`,
/// `10x faster`, `+47%`). Returns the matched fragment for the message.
fn invented_metric(lower: &str) -> Option<String> {
    // "trusted by <digits>"
    if let Some(idx) = lower.find("trusted by ") {
        let tail = &lower[idx + "trusted by ".len()..];
        if tail.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            let frag: String = lower[idx..].chars().take(24).collect();
            return Some(frag.trim().to_string());
        }
    }
    // "<digits>x faster" / "<digits>% uptime/faster"
    for unit in ["x faster", "% uptime", "% faster", "x more"] {
        if let Some(idx) = lower.find(unit) {
            // Require a digit immediately before the unit.
            let before = lower[..idx].trim_end();
            if before.chars().last().is_some_and(|c| c.is_ascii_digit()) {
                // char-safe back-step: `idx - 8` may land mid-char on CJK copy.
                let from = floor_boundary(lower, idx.saturating_sub(8));
                let frag: String = lower[from..].chars().take(20).collect();
                return Some(frag.trim().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(file: &str, content: &str) -> Vec<&'static str> {
        scan_design_quality(file, content)
            .into_iter()
            .map(|f| f.rule)
            .collect()
    }

    #[test]
    fn flags_ai_purple() {
        let r = rules("src/Hero.tsx", "const c = '#6366f1';");
        assert!(r.contains(&"ai-purple"));
    }

    #[test]
    fn flags_gradient_text() {
        let css =
            "h1 { background: linear-gradient(90deg, #f00, #0f0); -webkit-background-clip: text; }";
        assert!(rules("src/a.css", css).contains(&"gradient-text"));
    }

    #[test]
    fn flags_overused_primary_font_but_not_fallback() {
        // Inter as the lead font → flagged.
        assert!(
            rules("src/a.css", "body { font-family: Inter, system-ui; }")
                .contains(&"overused-font")
        );
        // Inter only as a fallback after a distinctive display font → clean.
        assert!(!rules(
            "src/a.css",
            "h1 { font-family: \"Clash Display\", Inter, sans-serif; }"
        )
        .contains(&"overused-font"));
    }

    #[test]
    fn flags_bounce_easing_not_crafted_ease() {
        assert!(rules(
            "src/a.css",
            "a { transition: 200ms cubic-bezier(0.34, 1.56, 0.64, 1); }"
        )
        .contains(&"bounce-easing"));
        assert!(!rules(
            "src/a.css",
            "a { transition: 200ms cubic-bezier(0.16, 1, 0.3, 1); }"
        )
        .contains(&"bounce-easing"));
    }

    #[test]
    fn flags_buzzwords_when_multiple() {
        let copy = "<p>Supercharge your workflow with our industry-leading platform</p>";
        assert!(rules("src/a.tsx", copy).contains(&"buzzwords"));
        // A single buzzword alone is below threshold.
        assert!(!rules("src/a.tsx", "<p>supercharge</p>").contains(&"buzzwords"));
    }

    #[test]
    fn flags_invented_metrics() {
        assert!(rules("src/a.tsx", "Trusted by 50,000+ teams").contains(&"invented-metrics"));
        assert!(rules("src/a.tsx", "<span>10x faster</span>").contains(&"invented-metrics"));
    }

    #[test]
    fn flags_cream_band_surface() {
        // #faf3e6 is a warm, very-light cream (r≥g≥b, warmth 20) → flagged.
        assert!(rules("src/a.css", "body{background:#faf3e6}").contains(&"cream-band"));
        // A pure/cool near-white is NOT cream.
        assert!(!rules("src/a.css", "body{background:#fafafa}").contains(&"cream-band"));
        assert!(!rules("src/a.css", "body{background:#f8fafc}").contains(&"cream-band"));
    }

    #[test]
    fn flags_em_dash_overuse_only_at_scale() {
        let many = "a — b — c — d — e — f".to_string();
        assert!(rules("src/a.tsx", &many).contains(&"em-dash-overuse"));
        assert!(!rules("src/a.tsx", "a — b").contains(&"em-dash-overuse"));
    }

    #[test]
    fn flags_placeholder_names() {
        assert!(rules("src/a.tsx", "<p>Jane Doe, CEO</p>").contains(&"placeholder-name"));
    }

    #[test]
    fn clean_premium_code_passes() {
        let css = "h1 { font-family: \"Clash Display\", system-ui; color: var(--color-text); \
                   transition: 200ms cubic-bezier(0.16,1,0.3,1); }";
        assert!(scan_design_quality("src/a.css", css).is_empty());
    }

    #[test]
    fn extract_fonts_collects_declared_families() {
        let css = "h1{font-family:\"Clash Display\", Inter, sans-serif} body{--font-mono: 'Geist Mono', monospace}";
        let fonts = extract_fonts(css);
        assert!(fonts.contains(&"clash display".to_string()));
        assert!(fonts.contains(&"inter".to_string()));
        assert!(fonts.contains(&"geist mono".to_string()));
        // Generic families are recognized as universal fallbacks.
        assert!(is_generic_font("sans-serif") && is_generic_font("monospace"));
        assert!(!is_generic_font("clash display"));
    }

    #[test]
    fn ignores_non_ui_files() {
        assert!(scan_design_quality("README.md", "#6366f1 supercharge 10x faster").is_empty());
    }

    #[test]
    fn fail_open_on_empty() {
        assert!(scan_design_quality("src/a.tsx", "").is_empty());
    }

    #[test]
    fn never_panics_on_multibyte_input() {
        // Regression: byte-index slicing in invented_metric / extract_fonts /
        // overused_primary_font used to panic on CJK straddling the cut.
        let cases = [
            "比对手快10x more 的体验",                    // invented_metric back-step
            &format!("font-family: {}", "标".repeat(60)), // extract_fonts cap @160
            &format!("font-family:{} sans", "黑体".repeat(40)), // overused cap @120
            "标标标9x more",
            &"图".repeat(500),
        ];
        for c in cases {
            // Must not panic; result is irrelevant.
            let _ = scan_design_quality("src/a.css", c);
            let _ = extract_fonts(c);
        }
    }
}
