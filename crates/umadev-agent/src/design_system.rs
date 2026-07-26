//! **Design-system conformance floor** (`UD-CODE-007`, spec §3.7) — the
//! deterministic half of the design moat.
//!
//! The old check was `verify_design_tokens`: it passed if a `design-tokens.*`
//! file merely EXISTED. `:root{--color-bg:#000}` passed. That is theatre — it
//! proves a file was written, not that a design SYSTEM exists, and not that the
//! UI actually uses it. This module upgrades existence into a contract, in four
//! sub-rules, all deterministic, all fail-open:
//!
//! 1. **Schema floor** (`UD-CODE-007a`, blocking) — the token file must declare
//!    a real system: `>= 6` color roles each with a PAIRED `on-` foreground, a
//!    type scale of `>= 4` steps whose adjacent ratio is `>= 1.125`, a 4pt-based
//!    spacing scale, a radius scale, and `>= 2` motion durations + `>= 1` easing.
//! 2. **Contrast** (`UD-CODE-007b`, blocking) — every DECLARED
//!    `(surface, on-surface)` pair is MEASURED with the WCAG formula (pure Rust,
//!    no browser, no deps — see `umadev_governance::color`): `>= 4.5:1` body,
//!    `>= 3:1` large/UI. A failing pair names BOTH tokens and the measured ratio.
//! 3. **Drift** (`UD-CODE-007c`, blocking) — UI source is scanned for literal
//!    colors / font-families / radii / font-sizes that are NOT drawn from the
//!    token set, with tolerances (color ±6 per channel, radius ±0.5px, size
//!    ±0.5px), because a design system nobody imports is not a design system.
//! 4. **Banned brand hue** (`UD-CODE-007d`, blocking) — a declared
//!    primary/accent inside the AI indigo/violet band is rejected UNLESS the
//!    requirement text explicitly asks for purple.
//!
//! Plus the register-scoped design-lint registry
//! (`umadev_governance::scan_design_rules`), whose small P0 tier folds in as
//! blocking and whose advisory tier surfaces as notes.
//!
//! **Fail-open at every edge.** No token file → the report is `unavailable` and
//! the caller keeps today's `DesignTokensPresent` behaviour. An unparseable
//! token file, an unreadable tree, a color syntax we do not know → that
//! contributor simply says nothing. The floor never fabricates a failure and
//! never blocks the host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use umadev_governance::color::{contrast_ratio, is_ai_purple, parse_color, Srgb};
use umadev_governance::design::Register;

/// Sub-rule ids (spec §3.7). Carried in each finding so a rework directive can
/// cite the clause it violates.
pub const RULE_SCHEMA: &str = "UD-CODE-007a";
/// Contrast sub-rule id.
pub const RULE_CONTRAST: &str = "UD-CODE-007b";
/// Token-drift sub-rule id.
pub const RULE_DRIFT: &str = "UD-CODE-007c";
/// Banned-brand-hue sub-rule id.
pub const RULE_BANNED_HUE: &str = "UD-CODE-007d";
/// Design-lint-registry sub-rule id (the register-scoped source lints).
pub const RULE_LINT: &str = "UD-CODE-007e";

/// WCAG AA body-text contrast floor.
const CONTRAST_BODY: f64 = 4.5;
/// WCAG AA large-text / UI contrast floor — used for the `border` / `muted`
/// roles, which are chrome rather than running copy.
const CONTRAST_UI: f64 = 3.0;
/// Minimum distinct color roles a real design system declares.
const MIN_COLOR_ROLES: usize = 6;
/// Minimum type-scale steps.
const MIN_TYPE_STEPS: usize = 4;
/// Minimum adjacent type-step ratio (below this the "scale" is noise).
const MIN_TYPE_RATIO: f64 = 1.125;
/// Minimum motion durations.
const MIN_DURATIONS: usize = 2;
/// Per-channel tolerance (0–255) for calling a source literal "drawn from" a token.
const COLOR_TOL: u8 = 6;
/// Tolerance (px) for radius / font-size literals.
const LEN_TOL: f64 = 0.5;
/// Max UI files scanned for drift (bounded — the floor never grinds).
const MAX_DRIFT_FILES: usize = 300;
/// Max drift findings reported (one directive, not a wall).
const MAX_DRIFT_FINDINGS: usize = 6;
/// Hard limits for project-controlled design inputs. A file is either read in
/// full within these bounds or ignored; a truncated prefix is never parsed as
/// a complete design contract.
const MAX_DESIGN_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOKEN_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCE_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// One design-system finding.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Whether this blocks the step (true) or is advisory (false).
    pub blocking: bool,
    /// Evidence-bearing message (self-prefixed `design-system:`) suitable for
    /// folding into a rework directive — it names the tokens, the measured
    /// number, and what to do instead.
    pub message: String,
    /// The violated sub-rule id (`UD-CODE-007a`..`e`).
    pub rule: &'static str,
}

/// The parsed token set — the ALLOWED values a conformant UI may use.
#[derive(Debug, Clone, Default)]
pub struct TokenSet {
    /// `--color-*` role → color (roles keep their `on-` prefix, e.g. `on-primary`).
    pub colors: BTreeMap<String, Srgb>,
    /// Declared font families (lowercased), from `--font-*` / `font-family`.
    pub fonts: Vec<String>,
    /// Declared radii, in px.
    pub radii: Vec<f64>,
    /// Declared type-scale steps, in px, ascending.
    pub type_steps: Vec<f64>,
    /// Declared spacing steps, in px, ascending.
    pub spacing: Vec<f64>,
    /// Declared motion durations, in ms.
    pub durations: Vec<f64>,
    /// Declared easing curves (raw text).
    pub easings: Vec<String>,
}

impl TokenSet {
    /// Whether the set holds nothing at all (an empty / unparseable file).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.colors.is_empty()
            && self.fonts.is_empty()
            && self.radii.is_empty()
            && self.type_steps.is_empty()
    }

    /// Whether `c` was drawn from this token set (within [`COLOR_TOL`]).
    fn allows_color(&self, c: Srgb) -> bool {
        self.colors.values().any(|t| t.near(c, COLOR_TOL))
    }

    /// Whether `px` matches a declared radius (within [`LEN_TOL`]).
    fn allows_radius(&self, px: f64) -> bool {
        // `9999px` / `50%` pills are a universal idiom, not drift.
        px >= 999.0 || self.radii.iter().any(|r| (r - px).abs() <= LEN_TOL)
    }

    /// Whether `px` matches a declared type step (within [`LEN_TOL`]).
    fn allows_size(&self, px: f64) -> bool {
        self.type_steps.iter().any(|s| (s - px).abs() <= LEN_TOL)
    }

    /// Whether `family` (lowercased) is declared, or is a universal fallback.
    fn allows_font(&self, family: &str) -> bool {
        umadev_governance::is_generic_font(family)
            || self.fonts.iter().any(|f| f == family)
            || family.starts_with("var(")
            || family.is_empty()
    }
}

/// The full report for one project.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// `false` when there is no token file at all — the caller then keeps the
    /// legacy `DesignTokensPresent` behaviour (fail-open).
    pub available: bool,
    /// The parsed token set (empty when `available == false`).
    pub tokens: TokenSet,
    /// Every finding, blocking and advisory.
    pub findings: Vec<Finding>,
    /// Token files that were read (workspace-relative).
    pub files: Vec<String>,
}

impl Report {
    /// The blocking findings only.
    #[must_use]
    pub fn blocking(&self) -> Vec<&Finding> {
        self.findings.iter().filter(|f| f.blocking).collect()
    }

    /// Whether the design system conforms (no blocking finding). An
    /// `unavailable` report conforms vacuously — the floor never fabricates.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.findings.iter().all(|f| !f.blocking)
    }
}

/// Run the full design-system floor for `root`.
///
/// `requirement` is the user's own words — consulted ONLY to decide whether a
/// purple brand hue was explicitly asked for (`UD-CODE-007d`).
/// `register` scopes the design-lint registry (`Unknown` runs every rule).
///
/// Fail-open: no token file → `available: false` and zero findings.
#[must_use]
pub fn verify_design_system(root: &Path, requirement: &str, register: Register) -> Report {
    let files = crate::acceptance::design_tokens_files(root);
    if files.is_empty() {
        return Report::default();
    }
    let mut raw = String::new();
    let mut names = Vec::new();
    let mut token_budget =
        crate::bounded_fs::Utf8ReadBudget::new(MAX_TOKEN_TOTAL_BYTES, MAX_DESIGN_FILE_BYTES);
    for f in &files {
        let Ok(s) = token_budget.read_utf8_beneath(root, f) else {
            // Parsing only the readable subset would invent a token system that
            // the project does not actually declare. Preserve this floor's
            // fail-open contract instead.
            return Report::default();
        };
        raw.push_str(&s);
        raw.push('\n');
        names.push(rel(root, f));
    }
    // THEMES ARE PARSED SEPARATELY. A token file that declares light AND dark is the
    // norm, and both are REAL: `--color-bg: #fff` in `:root` and `--color-bg: #0b0b0c`
    // under `.dark` are two different decisions, each with its own foreground, each of
    // which must clear contrast on its own. Flattening them into one map (last write
    // wins) measures a theme that ships to nobody, and makes every literal of the theme
    // that lost read as drift. So: one token set per theme, each an overlay on the base.
    let themes = parse_theme_tokens(&raw);
    // The UNION across themes — the allowed-value set the drift scan compares against
    // (a dark-theme color IS drawn from the system), and the role set the schema floor
    // reads.
    let tokens = union_tokens(&themes);
    let mut findings = Vec::new();

    // The file exists but holds nothing we can read as a system → say so once,
    // and stop (every downstream check would be a fabrication).
    if tokens.is_empty() {
        findings.push(Finding {
            blocking: true,
            rule: RULE_SCHEMA,
            message: format!(
                "design-system: `{}` declares no readable tokens — write the design system as \
                 real CSS custom properties / JSON keys (color roles with paired foregrounds — \
                 `--color-on-<role>` or `--<role>-foreground` — a type scale, spacing, radii, \
                 motion), not prose",
                names.join(", ")
            ),
        });
        return Report {
            available: true,
            tokens,
            findings,
            files: names,
        };
    }

    // The run's ONE colour decision (the brain's, made at the run door and persisted),
    // read here and honoured by EVERY rule that reads the same band. If the token-level
    // banned-hue rule (`007d`) and the source-level `ai-color-palette` lint (`007e`) do not
    // stand down on the SAME condition, they contradict each other (tokens accepted, the
    // component using them blocked) and the build cannot converge on any color at all.
    let purple_allowed = requirement_asks_for_purple(root, requirement);

    findings.extend(schema_findings(&tokens));
    for (theme, ts) in &themes {
        findings.extend(contrast_findings_in(theme, ts));
        if !purple_allowed {
            findings.extend(banned_hue_findings_in(theme, ts));
        }
    }
    let mut source_budget =
        crate::bounded_fs::Utf8ReadBudget::new(MAX_SOURCE_TOTAL_BYTES, MAX_DESIGN_FILE_BYTES);
    // Blocking registry rules get first claim on the shared source budget;
    // advisory drift must never starve the gate that can actually stop a bad
    // implementation.
    findings.extend(lint_findings(
        root,
        register,
        purple_allowed,
        &mut source_budget,
    ));
    findings.extend(drift_findings(root, &tokens, &mut source_budget));

    Report {
        available: true,
        tokens,
        findings,
        files: names,
    }
}

/// The name of the base (unqualified) theme — everything outside a dark-scheme block.
const BASE_THEME: &str = "base";
/// The name of the dark theme segment.
const DARK_THEME: &str = "dark";

/// Selector fragments that open a DARK-scheme block, in the shapes token files
/// actually use.
const DARK_SELECTORS: &[&str] = &[
    "prefers-color-scheme: dark",
    "prefers-color-scheme:dark",
    ".dark",
    "[data-theme=\"dark\"]",
    "[data-theme='dark']",
    "[data-theme=dark]",
    "[data-mode=\"dark\"]",
    "\"dark\"",
];

/// Split the raw token source into `(theme, css)` segments — the base, and whatever
/// sits inside a dark-scheme block. A brace-depth walk, not a CSS parser: it must never
/// fail, only find less (a shape it cannot see lands entirely in the base, which is
/// exactly today's behaviour).
fn theme_split(raw: &str) -> (String, String) {
    let mut base = String::new();
    let mut dark = String::new();
    let mut depth: i32 = 0;
    let mut dark_from: Option<i32> = None;
    for line in raw.lines().take(4000) {
        let lower = line.to_ascii_lowercase();
        let opens = i32::try_from(line.matches('{').count()).unwrap_or(0);
        let closes = i32::try_from(line.matches('}').count()).unwrap_or(0);
        if dark_from.is_none() && opens > 0 && DARK_SELECTORS.iter().any(|s| lower.contains(s)) {
            dark_from = Some(depth);
        }
        if dark_from.is_some() {
            dark.push_str(line);
            dark.push('\n');
        } else {
            base.push_str(line);
            base.push('\n');
        }
        depth += opens - closes;
        if let Some(d) = dark_from {
            if depth <= d {
                dark_from = None;
            }
        }
    }
    (base, dark)
}

/// One [`TokenSet`] per declared theme, each an OVERLAY on the base (a dark block that
/// only overrides `--color-bg` still inherits the base's foreground, which is precisely
/// how it renders). The base is always first; a file with no dark block yields exactly
/// one theme, so nothing changes for the common case.
#[must_use]
pub fn parse_theme_tokens(raw: &str) -> Vec<(String, TokenSet)> {
    let (base_css, dark_css) = theme_split(raw);
    let base = parse_tokens(&base_css);
    let mut out = vec![(BASE_THEME.to_string(), base.clone())];
    if dark_css.trim().is_empty() {
        return out;
    }
    let overrides = parse_tokens(&dark_css);
    if overrides.colors.is_empty() {
        return out;
    }
    let mut dark = base;
    for (role, c) in overrides.colors {
        dark.colors.insert(role, c);
    }
    out.push((DARK_THEME.to_string(), dark));
    out
}

/// The union of every theme's tokens — the ALLOWED set (a literal drawn from the dark
/// theme is drawn from the system) and the role set the schema floor reads.
///
/// The base theme's roles keep their plain names; a role a later theme OVERRIDES is
/// stored under a `theme/role` key so it widens the allowed VALUES without shadowing the
/// base role or inventing a new one (`is_surface_role` refuses any key with a `/`).
fn union_tokens(themes: &[(String, TokenSet)]) -> TokenSet {
    let mut out = TokenSet::default();
    for (theme, ts) in themes {
        for (role, c) in &ts.colors {
            let key = if out.colors.contains_key(role) {
                format!("{theme}/{role}")
            } else {
                role.clone()
            };
            out.colors.insert(key, *c);
        }
        for f in &ts.fonts {
            if !out.fonts.contains(f) {
                out.fonts.push(f.clone());
            }
        }
        for r in &ts.radii {
            push_uniq(&mut out.radii, *r);
        }
        for s in &ts.type_steps {
            push_uniq(&mut out.type_steps, *s);
        }
        for s in &ts.spacing {
            push_uniq(&mut out.spacing, *s);
        }
        for d in &ts.durations {
            push_uniq(&mut out.durations, *d);
        }
        for e in &ts.easings {
            if !out.easings.contains(e) {
                out.easings.push(e.clone());
            }
        }
    }
    out.radii.sort_by(f64::total_cmp);
    out.type_steps.sort_by(f64::total_cmp);
    out.spacing.sort_by(f64::total_cmp);
    out.durations.sort_by(f64::total_cmp);
    out
}

/// Workspace-relative, `/`-separated path.
fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

// ===================================================================
// UD-CODE-007f — the designer's VISUAL DIRECTION, before any token
// ===================================================================

/// Sub-rule id for the `## Visual direction` contract.
pub const RULE_DIRECTION: &str = "UD-CODE-007f";

/// The UIUX doc for `slug`.
fn uiux_path(root: &Path, slug: &str) -> PathBuf {
    root.join("output").join(format!("{slug}-uiux.md"))
}

/// The body of the UIUX doc's `## Visual direction` section (up to the next
/// `## `). Empty when the doc or the section is absent.
#[must_use]
pub fn visual_direction_section(root: &Path, slug: &str) -> String {
    let Ok(doc) =
        crate::bounded_fs::read_utf8_beneath(root, &uiux_path(root, slug), MAX_DESIGN_FILE_BYTES)
    else {
        return String::new();
    };
    section_body(&doc, "visual direction")
}

/// The body of the `## <heading>` section of a markdown doc (case-insensitive),
/// up to the next same-or-higher heading.
fn section_body(doc: &str, heading: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in doc.lines() {
        let l = line.trim();
        if let Some(h) = l.strip_prefix("##") {
            let h = h.trim_start_matches('#').trim().to_ascii_lowercase();
            if inside {
                break;
            }
            inside = h.contains(heading);
            continue;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The design REGISTER this project declared, read from the UIUX doc's
/// `## Visual direction`. Fail-open: no doc / no section / no register named →
/// [`Register::Unknown`], which runs every lint rule (the pre-register
/// behaviour), so an un-annotated project is never silently under-checked.
#[must_use]
pub fn register_for_project(root: &Path, slug: &str) -> Register {
    register_from_text(&visual_direction_section(root, slug))
}

/// Read a register out of free text.
///
/// An EXPLICIT `register: <x>` declaration always wins — that is the designer's
/// own decision and it outranks any guess. Failing that, infer from the surface
/// the text describes: a landing/marketing page is `brand`, a
/// dashboard/admin/console is `product`. Text that says neither (or both) is
/// [`Register::Unknown`] — and Unknown deliberately keeps the FULL historical
/// rule set, so a bad guess can never under-govern a turn.
#[must_use]
pub fn register_from_text(text: &str) -> Register {
    let lower = text.to_ascii_lowercase();
    // 1. An explicit declaration.
    for line in lower.lines() {
        if let Some(idx) = line.find("register") {
            let after = &line[idx + "register".len()..];
            // Only a real binding (`register: product`), not prose that merely
            // mentions the word.
            if after.trim_start().starts_with([':', '：', '=', '-']) {
                let r = Register::parse(after);
                if r != Register::Unknown {
                    return r;
                }
            }
        }
    }
    // 2. Infer from the surface being described. Count the signals rather than
    //    first-match, so "a landing page for our admin tool" does not flip on a
    //    coin toss — a tie is Unknown, which is the safe (full-rules) reading.
    const BRAND: &[&str] = &[
        "landing page",
        "landing-page",
        "marketing site",
        "marketing page",
        "campaign",
        "portfolio",
        "官网",
        "落地页",
        "营销页",
        "宣传页",
        "作品集",
    ];
    const PRODUCT: &[&str] = &[
        "dashboard",
        "admin panel",
        "admin console",
        "admin ",
        "back office",
        "backoffice",
        "console",
        "settings page",
        "devtool",
        "developer tool",
        "internal tool",
        "data table",
        "后台",
        "管理后台",
        "仪表盘",
        "控制台",
        "设置页",
        "工作台",
    ];
    let brand_hits = BRAND.iter().filter(|k| lower.contains(**k)).count();
    let product_hits = PRODUCT.iter().filter(|k| lower.contains(**k)).count();
    match brand_hits.cmp(&product_hits) {
        std::cmp::Ordering::Greater => Register::Brand,
        std::cmp::Ordering::Less => Register::Product,
        std::cmp::Ordering::Equal => Register::Unknown,
    }
}

/// The register for a turn in `root`, when the caller has no slug: prefer the
/// project's OWN declaration (any `output/*-uiux.md` with a
/// `## Visual direction`), then fall back to inferring from the user's words.
///
/// This is what the L0 firmware consults, so the injected design law is scoped to
/// the surface actually being built. Fail-open: nothing readable → the user's
/// words → [`Register::Unknown`] → the full historical law.
#[must_use]
pub fn register_for_root(root: &Path, requirement: &str) -> Register {
    let output = root.join("output");
    let mut budget =
        crate::bounded_fs::Utf8ReadBudget::new(MAX_TOKEN_TOTAL_BYTES, MAX_DESIGN_FILE_BYTES);
    if crate::bounded_fs::is_real_directory_beneath(root, &output) {
        if let Ok(rd) = std::fs::read_dir(&output) {
            for e in rd.flatten().take(64) {
                let p = e.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.ends_with("-uiux.md") {
                    continue;
                }
                if let Ok(doc) = budget.read_utf8_beneath(root, &p) {
                    let r = register_from_text(&section_body(&doc, "visual direction"));
                    if r != Register::Unknown {
                        return r;
                    }
                }
            }
        }
    }
    register_from_text(requirement)
}

/// Verify the designer's **direction step** (`UD-CODE-007f`) — the step that
/// must land BEFORE the token file.
///
/// A designer that jumps straight to tokens is answering "what hex?" before ever
/// answering "what is this, for whom, and what does it feel like?" — and a token
/// file whose only gate is existence cannot tell the difference. So the plan
/// carries a `design-direction` step whose EVIDENCE is this section, and this
/// section must actually decide things:
///
/// 1. **A one-line design read** — page kind / audience / register / vibe /
///    aesthetic family.
/// 2. **Three forced decisions** — (a) *color commitment level*: one of
///    `restrained` / `committed` / `full-palette` / `drenched` (a level, not
///    "we'll see"); (b) *theme decided by a PHYSICAL SCENE sentence*: who uses
///    this, where, under what ambient light, in what mood — if the sentence does
///    not force light-vs-dark, it is not specific enough yet; (c) *2–3 named
///    anchor references, EACH bound to a specific dimension* (density from one,
///    type from another, whitespace from a third). Adjectives like "modern" /
///    "clean" are REJECTED — they decide nothing.
/// 3. **Anti-goals** — what this product deliberately is NOT.
///
/// ## What blocks, and what merely speaks
/// Only ONE thing blocks: the `## Visual direction` section is **entirely absent** from
/// a UI-bearing run. That is an objective, unambiguous miss — the designer seat was
/// asked for a decision and produced no section at all — and the finding teaches the
/// full recipe.
///
/// Every SUB-CLAUSE (register named, commitment level, scene sentence, bound anchors,
/// anti-goals) is **advisory**. Each is a keyword test over prose, and a keyword test
/// over prose has false negatives: a direction that says "a bright open-plan office; the
/// app is light-first" has decided the theme by a physical scene and names no keyword
/// this scan knows. Blocking on that is blocking a correct answer for being phrased
/// unexpectedly — and the fix the rework directive demands ("add the word `dark`") makes
/// the document worse, not better. They are exactly the notes a designer wants and
/// exactly the wrong thing to stop a build on.
///
/// ## Gating
/// `needs_ui` — the ROUTE's own judgement (`RoutePlan::needs_ui`), not the presence of a
/// file. A file is not a question: a brownfield repo, or a second run in a workspace
/// where an earlier UI build left `output/<slug>-uiux.md` on disk, would otherwise hand
/// a pure backend task a blocking design finding it can neither act on nor escape.
///
/// Fail-open: not a UI run → nothing. No UIUX doc at all → nothing (a project with no
/// design phase is not failed for skipping one). The section is only demanded of a run
/// that is actually building UI and already has a UIUX doc.
#[must_use]
pub fn visual_direction_findings(root: &Path, slug: &str, needs_ui: bool) -> Vec<Finding> {
    if !needs_ui || !crate::bounded_fs::is_real_file_beneath(root, &uiux_path(root, slug)) {
        return Vec::new();
    }
    let body = visual_direction_section(root, slug);
    if body.trim().is_empty() {
        return vec![Finding {
            blocking: true,
            rule: RULE_DIRECTION,
            message: format!(
                "design-system: `output/{slug}-uiux.md` has no `## Visual direction` section — \
                 before any token, decide and WRITE: (1) a one-line design read (page kind / \
                 audience / register `brand`|`product` / vibe / aesthetic family); (2) three \
                 forced decisions — color commitment level \
                 (restrained|committed|full-palette|drenched), the light-vs-dark theme decided by \
                 a PHYSICAL SCENE sentence (who uses this, where, under what ambient light, in \
                 what mood), and 2-3 NAMED anchor references each bound to ONE dimension (density \
                 from one, type from another, whitespace from a third); (3) anti-goals. A tokens \
                 file written before this is a hex guess, not a design"
            ),
        }];
    }
    let lower = body.to_ascii_lowercase();
    let mut missing: Vec<&str> = Vec::new();

    if register_for_project(root, slug) == Register::Unknown {
        missing.push(
            "the design read must name the REGISTER in one word — `brand` (landing/marketing: \
             design IS the product) or `product` (app/dashboard/tool: design SERVES the task). \
             They take opposite rules; guessing makes one of them worse",
        );
    }
    const LEVELS: &[&str] = &[
        "restrained",
        "committed",
        "full-palette",
        "full palette",
        "drenched",
    ];
    if !LEVELS.iter().any(|l| lower.contains(l)) {
        missing.push(
            "a COLOR COMMITMENT LEVEL — pick exactly one: restrained | committed | full-palette \
             | drenched",
        );
    }
    if !has_scene_sentence(&lower) {
        missing.push(
            "the THEME decided by a physical-scene sentence — who uses this, WHERE, under what \
             ambient LIGHT, in what MOOD — and then light or dark falls out of it. If your \
             sentence doesn't force the choice, add detail until it does",
        );
    }
    let anchors = bound_anchor_count(&body);
    if anchors < 2 {
        missing.push(
            "2-3 NAMED anchor references, EACH bound to one dimension (e.g. `density: <named \
             reference>` / `type: <named reference>` / `whitespace: <named reference>`). \
             \"modern\", \"clean\", \"professional\" are adjectives, not anchors — they decide \
             nothing and are rejected",
        );
    }
    if !lower.contains("anti-goal") && !lower.contains("anti goal") && !lower.contains("not:") {
        missing.push(
            "ANTI-GOALS — name what this product deliberately is NOT. A direction with no \
             anti-goal has not ruled anything out, so it has not chosen anything",
        );
    }

    if missing.is_empty() {
        return Vec::new();
    }
    vec![Finding {
        // ADVISORY. The section EXISTS — the designer answered. What is left is a
        // keyword scan's opinion about how the answer is phrased, and a keyword scan is
        // not allowed to stop a build over prose it merely failed to recognise.
        blocking: false,
        rule: RULE_DIRECTION,
        message: format!(
            "design-system (advisory): `## Visual direction` in `output/{slug}-uiux.md` reads as \
             incomplete — worth deciding explicitly: {}",
            missing
                .iter()
                .enumerate()
                .map(|(i, m)| format!("({}) {m}", i + 1))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    }]
}

/// Whether the direction contains a real PHYSICAL SCENE sentence — one that
/// names an ambient-light / setting / mood cue AND lands on light-vs-dark. Both
/// halves are required: "users like dark mode" decides nothing about the scene,
/// and "engineers at night" that never states the theme decided nothing either.
fn has_scene_sentence(lower: &str) -> bool {
    const SCENE: &[&str] = &[
        "light",
        "lit",
        "lighting",
        "daylight",
        "sunlight",
        "glare",
        "dim",
        "night",
        "evening",
        "morning",
        "office",
        "desk",
        "outdoor",
        "indoor",
        "screen",
        "ambient",
        "环境光",
        "白天",
        "夜里",
        "夜间",
        "光线",
        "户外",
        "室内",
        "办公",
    ];
    const THEME: &[&str] = &[
        "dark",
        "light mode",
        "light theme",
        "深色",
        "浅色",
        "暗色",
        "亮色",
    ];
    SCENE.iter().any(|s| lower.contains(s)) && THEME.iter().any(|t| lower.contains(t))
}

/// Count anchors that are BOUND to a dimension: a list line of the shape
/// `<dimension>: <concrete reference>` — the dimension LEADS the line, so prose
/// that merely happens to contain the word "color" or "motion" is not mistaken
/// for an anchor. The reference must be substantive and must not be a bare
/// adjective ("modern", "clean") — an adjective decides nothing, which is
/// exactly why the direction step exists.
fn bound_anchor_count(body: &str) -> usize {
    const DIMENSIONS: &[&str] = &[
        "density",
        "type",
        "typography",
        "whitespace",
        "spacing",
        "motion",
        "color",
        "palette",
        "layout",
        "rhythm",
        "密度",
        "排版",
        "留白",
        "动效",
        "配色",
        "布局",
    ];
    // Words that DECIDE NOTHING — a reference made only of these is rejected.
    const VAGUE: &[&str] = &[
        "modern",
        "clean",
        "professional",
        "sleek",
        "minimal",
        "beautiful",
        "elegant",
        "nice",
        "现代",
        "干净",
        "简洁",
        "专业",
        "优雅",
    ];
    let mut n = 0;
    for line in body.lines() {
        // Strip list markers so `  - density: …` and `2. density: …` both work.
        let l = line
            .trim()
            .trim_start_matches(['-', '*', '+', '·', '•'])
            .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ')')
            .trim()
            .to_ascii_lowercase();
        // The dimension must LEAD, and be followed by a binding separator.
        let Some(dim) = DIMENSIONS.iter().find(|d| l.starts_with(**d)) else {
            continue;
        };
        let rest = l[dim.len()..].trim_start();
        if !rest.starts_with([':', '：', '-', '—', '=']) {
            continue;
        }
        let anchor = rest
            .trim_start_matches([':', '：', '-', '—', '=', ' '])
            .trim();
        if anchor.len() < 6 {
            continue;
        }
        // An anchor that is ONLY an adjective decides nothing.
        let words = anchor.split_whitespace().count();
        let vague_only = VAGUE.iter().any(|v| anchor.contains(v)) && words <= 3;
        if !vague_only {
            n += 1;
        }
    }
    n
}

// ===================================================================
// token parsing — CSS custom properties AND flat JSON keys
// ===================================================================

/// Parse a `design-tokens.css` / `design-tokens.json` body into a [`TokenSet`].
///
/// Handles the two shapes a designer actually writes: CSS custom properties
/// (`--color-primary: oklch(...)`) and JSON (`"color-primary": "#1d4ed8"` or a
/// nested `{"color": {"primary": "#1d4ed8"}}`). The parse is a tolerant
/// key/value scan, not a real CSS/JSON parser — it must never fail, only find
/// less.
#[must_use]
pub fn parse_tokens(raw: &str) -> TokenSet {
    let mut ts = TokenSet::default();
    for (key, value) in key_values(raw) {
        let k = key.trim_start_matches("--").to_ascii_lowercase();
        let v = value.trim().trim_matches(['"', '\'', ',']).trim();
        if v.is_empty() {
            continue;
        }
        if let Some(role) = k.strip_prefix("color-") {
            // Skip aliases that point at another token — they add no new value.
            if v.starts_with("var(") {
                continue;
            }
            if let Some(c) = parse_color(v) {
                ts.colors.insert(role.to_string(), c);
            }
            continue;
        }
        if k.starts_with("font-") || k == "font" {
            for fam in v.split(',') {
                let f = fam.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
                if !f.is_empty() && f.len() < 40 && !ts.fonts.contains(&f) {
                    ts.fonts.push(f);
                }
            }
            continue;
        }
        if k.starts_with("radius") || k.starts_with("rounded") || k.starts_with("border-radius") {
            if let Some(px) = length_px(v) {
                push_uniq(&mut ts.radii, px);
            }
            continue;
        }
        if k.starts_with("text-") || k.starts_with("font-size") || k.starts_with("type-") {
            if let Some(px) = length_px(v) {
                push_uniq(&mut ts.type_steps, px);
            }
            continue;
        }
        if k.starts_with("space") || k.starts_with("spacing") || k.starts_with("gap") {
            if let Some(px) = length_px(v) {
                push_uniq(&mut ts.spacing, px);
            }
            continue;
        }
        if k.starts_with("duration") || k.starts_with("transition") || k.starts_with("motion") {
            if let Some(ms) = duration_ms(v) {
                push_uniq(&mut ts.durations, ms);
            }
            // A `--transition-fast: 150ms cubic-bezier(...)` carries both.
            if v.contains("cubic-bezier") || v.contains("ease") || v.contains("linear") {
                let e = v.to_string();
                if !ts.easings.contains(&e) {
                    ts.easings.push(e);
                }
            }
            continue;
        }
        if k.starts_with("ease") || k.starts_with("easing") || k.starts_with("curve") {
            let e = v.to_string();
            if !ts.easings.contains(&e) {
                ts.easings.push(e);
            }
            continue;
        }
        // THE UNPREFIXED ROLE IDIOM. The dominant React convention names its roles
        // directly — `--primary`, `--primary-foreground`, `--background` — with no
        // `color-` prefix at all. A parser that only knows `--color-*` reads such a file
        // as declaring ZERO colors, which fails the schema floor, fails the pairing
        // check, and hands the base a rework loop it cannot exit without abandoning the
        // convention its entire component library is built on.
        //
        // Recognised by an explicit ROLE VOCABULARY rather than "any key whose value is
        // a color" — otherwise `--chart-1`, `--shadow-md`, or a one-off brand swatch
        // would each be read as a surface owing a paired foreground, and the schema
        // floor would start blocking on tokens that owe nothing.
        if is_bare_color_role(&k) && !v.starts_with("var(") {
            if let Some(c) = parse_color(v) {
                ts.colors.insert(k.clone(), c);
            }
        }
    }
    ts.radii.sort_by(f64::total_cmp);
    ts.type_steps.sort_by(f64::total_cmp);
    ts.spacing.sort_by(f64::total_cmp);
    ts.durations.sort_by(f64::total_cmp);
    ts
}

/// The role vocabulary an UNPREFIXED token file (`--primary`, `--background`) draws
/// from. Closed on purpose: a key outside this set is not assumed to be a color role
/// just because its value happens to parse as a color.
const BARE_COLOR_ROLES: &[&str] = &[
    "background",
    "foreground",
    "bg",
    "fg",
    "surface",
    "card",
    "popover",
    "primary",
    "secondary",
    "muted",
    "accent",
    "destructive",
    "danger",
    "success",
    "warning",
    "info",
    "border",
    "input",
    "ring",
];

/// Whether `k` is an unprefixed color role — one of [`BARE_COLOR_ROLES`], or that role's
/// foreground twin in either idiom (`primary-foreground` / `on-primary`).
fn is_bare_color_role(k: &str) -> bool {
    if BARE_COLOR_ROLES.contains(&k) {
        return true;
    }
    for base in BARE_COLOR_ROLES {
        if k == format!("{base}-foreground") || k == format!("on-{base}") {
            return true;
        }
    }
    false
}

/// Push `v` if no near-equal value is present.
fn push_uniq(v: &mut Vec<f64>, x: f64) {
    if !v.iter().any(|e| (e - x).abs() < 0.01) {
        v.push(x);
    }
}

/// Every `key: value` / `"key": "value"` pair in the source, as a flat list.
/// Deliberately shape-agnostic (CSS or JSON), because both files are the same
/// thing to us: a bag of named design decisions.
fn key_values(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in raw.lines().take(4000) {
        let l = line.trim();
        if l.is_empty() || l.starts_with("//") || l.starts_with("/*") || l.starts_with('*') {
            continue;
        }
        // JSON: a line may hold several `"key": <value>` pairs separated by
        // commas that a naive `;` split would never see. Scan for QUOTED keys
        // first — a quoted key is unambiguous, so this can't collide with CSS.
        let json_pairs = json_key_values(l);
        let found_json = !json_pairs.is_empty();
        out.extend(json_pairs);
        if found_json {
            continue;
        }
        // CSS: a line may hold several `a: b; c: d;` declarations.
        for decl in l.split(';') {
            let Some(colon) = decl.find(':') else {
                continue;
            };
            let key = decl[..colon]
                .trim()
                .trim_matches(['"', '\'', ',', '{'])
                .trim();
            let val = decl[colon + 1..].trim();
            if key.is_empty() || val.is_empty() || val == "{" {
                continue;
            }
            // A CSS var name. Reject selectors / at-rules.
            if key.contains('{') || key.starts_with('@') || key.contains(' ') && !key.contains('-')
            {
                continue;
            }
            out.push((key.to_string(), val.to_string()));
        }
        if out.len() > 2000 {
            break;
        }
    }
    out
}

/// Every `"key": <value>` pair on one JSON line. `<value>` is a quoted string or
/// a bare number; anything else (an object / array opener) is skipped, which is
/// how a nested token file degrades gracefully to its leaf keys.
fn json_key_values(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        // "key"
        let Some(kend) = line[i + 1..].find('"') else {
            break;
        };
        let key = &line[i + 1..i + 1 + kend];
        let mut j = i + 1 + kend + 1;
        // : value
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            i += 1 + kend + 1;
            continue;
        }
        j += 1;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        if bytes[j] == b'"' {
            let Some(vend) = line[j + 1..].find('"') else {
                break;
            };
            out.push((key.to_string(), line[j + 1..j + 1 + vend].to_string()));
            i = j + 1 + vend + 1;
        } else {
            let val: String = line[j..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                .collect();
            if !val.is_empty() && !key.is_empty() {
                out.push((key.to_string(), val.clone()));
            }
            i = j + val.len().max(1);
        }
        if out.len() > 500 {
            break;
        }
    }
    out
}

/// A CSS length in px (`16px`, `1rem`, `0.75em`) → px. `None` otherwise.
fn length_px(value: &str) -> Option<f64> {
    let v = value.trim();
    let num: String = v
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    let n: f64 = num.parse().ok()?;
    match v[num.len()..].trim() {
        "px" => Some(n),
        "rem" | "em" => Some(n * 16.0),
        "" => Some(n), // a bare JSON number is px by convention
        _ => None,
    }
}

/// A CSS duration (`150ms`, `0.2s`) → ms.
fn duration_ms(value: &str) -> Option<f64> {
    let v = value.trim();
    let num: String = v
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let n: f64 = num.parse().ok()?;
    let rest = v[num.len()..].trim();
    if rest.starts_with("ms") {
        Some(n)
    } else if rest.starts_with('s') {
        Some(n * 1000.0)
    } else {
        None
    }
}

// ===================================================================
// UD-CODE-007a — schema floor
// ===================================================================

/// Color roles that are SURFACES (they carry content, so they need a paired
/// foreground). Everything else (`border`, `ring`, a chart series, `*-hover`,
/// `*-muted`) is chrome or data and is exempt from the pairing requirement.
fn is_surface_role(role: &str) -> bool {
    // A `theme/role` key is another theme's OVERRIDE of a role that already exists —
    // it widens the allowed value set, it is not a second role, and it must never
    // demand a foreground of its own.
    if role.contains('/') {
        return false;
    }
    // A FOREGROUND, in any of its idioms — `--color-on-primary` (the pack idiom),
    // `--primary-foreground` (the dominant React idiom), and the bare `--foreground`
    // that pairs with the bare `--background`.
    if is_foreground_role(role) {
        return false;
    }
    const CHROME: &[&str] = &[
        "border",
        "ring",
        "shadow",
        "glass",
        "overlay",
        "aurora",
        "input",
        "outline",
        "divider",
        "chart",
        "series",
        "sparkline",
        "gradient",
        "scrim",
        "skeleton",
    ];
    if CHROME.iter().any(|c| role.starts_with(c)) {
        return false;
    }
    // Derived variants ride on their base role's pairing.
    !(role.ends_with("-hover")
        || role.ends_with("-active")
        || role.ends_with("-muted")
        || role.ends_with("-strong")
        || role.ends_with("-subtle"))
}

/// Whether the role names a FOREGROUND rather than a surface, in ANY of the idioms a
/// real token file uses.
fn is_foreground_role(role: &str) -> bool {
    role.starts_with("on-")
        || role.ends_with("-foreground")
        || role.ends_with("-fg")
        || role == "foreground"
        || role == "fg"
}

/// The declared foreground for `role`, whichever idiom the file writes it in.
///
/// `--color-on-primary` and `--primary-foreground` are THE SAME DECISION — "what is
/// legible on this surface" — and the second is the dominant convention in the React
/// ecosystem. A schema that only knows the first fails every project written in the
/// second: every surface reads as unpaired, every unpaired surface is a blocking
/// finding, and the base cannot fix it without abandoning the idiom its whole component
/// library is built on. One decision, several spellings; recognise all of them.
fn foreground_of(ts: &TokenSet, role: &str) -> Option<(String, Srgb)> {
    let mut keys = vec![
        format!("on-{role}"),
        format!("{role}-foreground"),
        format!("{role}-fg"),
    ];
    // The bare top-level pair (`--background` / `--foreground`).
    if role == "background" || role == "bg" {
        keys.push("foreground".to_string());
        keys.push("fg".to_string());
    }
    keys.into_iter()
        .find_map(|k| ts.colors.get(&k).map(|c| (k, *c)))
}

fn schema_findings(ts: &TokenSet) -> Vec<Finding> {
    let mut out = Vec::new();

    // ≥6 color roles, each surface role with a paired foreground (in EITHER idiom).
    let surfaces: Vec<&String> = ts.colors.keys().filter(|r| is_surface_role(r)).collect();
    if surfaces.len() < MIN_COLOR_ROLES {
        out.push(Finding {
            blocking: true,
            rule: RULE_SCHEMA,
            message: format!(
                "design-system: only {} color role(s) declared (need >= {MIN_COLOR_ROLES}) — a real \
                 system names bg / surface / card / muted / primary / accent (+ status), not a \
                 couple of one-off hexes",
                surfaces.len()
            ),
        });
    }
    let unpaired: Vec<String> = surfaces
        .iter()
        .filter(|r| foreground_of(ts, r).is_none())
        .map(|r| format!("--color-{r}"))
        .take(6)
        .collect();
    if !unpaired.is_empty() {
        out.push(Finding {
            blocking: true,
            rule: RULE_SCHEMA,
            message: format!(
                "design-system: surface token(s) with no paired foreground: {} — every surface \
                 ships the foreground that is legible on it, in whichever idiom this project \
                 uses (`--color-on-<role>` or `--<role>-foreground`), so a component never has \
                 to guess what text color to put on it",
                unpaired.join(", ")
            ),
        });
    }

    // Type scale: ≥4 steps, adjacent ratio ≥1.125.
    if ts.type_steps.len() < MIN_TYPE_STEPS {
        out.push(Finding {
            blocking: true,
            rule: RULE_SCHEMA,
            message: format!(
                "design-system: type scale has {} step(s) (need >= {MIN_TYPE_STEPS}) — declare \
                 `--text-xs..--text-3xl` as a real scale so the UI never invents a size",
                ts.type_steps.len()
            ),
        });
    } else if let Some((a, b)) = flat_type_pair(&ts.type_steps) {
        out.push(Finding {
            blocking: true,
            rule: RULE_SCHEMA,
            message: format!(
                "design-system: type steps {a:.0}px and {b:.0}px sit at a ratio of {:.3} (need \
                 >= {MIN_TYPE_RATIO}) — two steps that close together are one step with extra \
                 names; widen the scale or drop a step",
                b / a
            ),
        });
    }

    // 4pt-based spacing scale.
    if ts.spacing.len() < 4 {
        out.push(Finding {
            blocking: true,
            rule: RULE_SCHEMA,
            message: format!(
                "design-system: spacing scale has {} step(s) (need >= 4 on a 4pt grid) — declare \
                 `--space-1: 4px` .. `--space-16: 64px` so layout never uses an ad-hoc number",
                ts.spacing.len()
            ),
        });
    } else if let Some(bad) = ts.spacing.iter().copied().find(|s| (s % 4.0).abs() > 0.01) {
        out.push(Finding {
            blocking: true,
            rule: RULE_SCHEMA,
            message: format!(
                "design-system: spacing step {bad}px is off the 4pt grid — every step is a \
                 multiple of 4 (4 / 8 / 12 / 16 / 24 / 32 / 48 / 64), so rhythm is composable"
            ),
        });
    }

    // Radius scale.
    if ts.radii.is_empty() {
        out.push(Finding {
            blocking: true,
            rule: RULE_SCHEMA,
            message: "design-system: no radius scale declared — name `--radius-sm/md/lg` (a \
                      brutalist system declares 0px; that is still a declaration)"
                .to_string(),
        });
    }

    // Motion: ≥2 durations + ≥1 easing.
    if ts.durations.len() < MIN_DURATIONS || ts.easings.is_empty() {
        out.push(Finding {
            blocking: true,
            rule: RULE_SCHEMA,
            message: format!(
                "design-system: motion is under-declared ({} duration(s), {} easing(s); need \
                 >= {MIN_DURATIONS} durations + >= 1 easing) — name `--duration-fast/normal` and \
                 one `--ease-*` curve so timing is a decision, not a default",
                ts.durations.len(),
                ts.easings.len()
            ),
        });
    }
    out
}

/// The first adjacent type-step pair whose ratio is below [`MIN_TYPE_RATIO`].
fn flat_type_pair(steps: &[f64]) -> Option<(f64, f64)> {
    steps
        .windows(2)
        .find(|w| w[0] > 0.0 && w[1] / w[0] < MIN_TYPE_RATIO)
        .map(|w| (w[0], w[1]))
}

// ===================================================================
// UD-CODE-007b — contrast, measured (no browser, no deps)
// ===================================================================

/// Roles whose foreground is CHROME rather than running copy — the 3:1 large/UI
/// floor applies instead of the 4.5:1 body floor.
fn is_ui_scale_role(role: &str) -> bool {
    role.starts_with("muted") || role.starts_with("border") || role.starts_with("disabled")
}

/// Contrast for the BASE theme's token set (see [`contrast_findings_in`]). Kept as the
/// single-theme entry point the unit tests measure against.
#[cfg(test)]
fn contrast_findings(ts: &TokenSet) -> Vec<Finding> {
    contrast_findings_in(BASE_THEME, ts)
}

/// Measure every declared `(surface, foreground)` pair in ONE theme.
///
/// Per-theme, because contrast is a property of a theme, not of a file: a dark theme
/// that overrides the surface but not the foreground is a real, shipping, unreadable
/// screen — and a flattened, last-write-wins map would measure a theme nobody sees.
fn contrast_findings_in(theme: &str, ts: &TokenSet) -> Vec<Finding> {
    let mut out = Vec::new();
    for (role, surface) in &ts.colors {
        if !is_surface_role(role) {
            continue;
        }
        let Some((fg_key, fg)) = foreground_of(ts, role) else {
            continue; // the missing pair is a SCHEMA finding, not a contrast one
        };
        // A translucent surface (a glass layer) has no measurable contrast of
        // its own — it composites over whatever is beneath. Skip it honestly
        // rather than measure a fiction.
        if surface.a < 0.95 {
            continue;
        }
        let ratio = contrast_ratio(*surface, fg);
        let floor = if is_ui_scale_role(role) {
            CONTRAST_UI
        } else {
            CONTRAST_BODY
        };
        if ratio < floor {
            let scope = if theme == BASE_THEME {
                String::new()
            } else {
                format!(" (in the `{theme}` theme)")
            };
            out.push(Finding {
                blocking: true,
                rule: RULE_CONTRAST,
                message: format!(
                    "design-system: `--color-{fg_key}` on `--color-{role}`{scope} measures \
                     {ratio:.2}:1, below the {floor:.1}:1 floor — darken the foreground or \
                     lighten the surface until it clears; contrast is measured, not eyeballed"
                ),
            });
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
}

// ===================================================================
// UD-CODE-007d — the banned brand hue
// ===================================================================

/// Whether the user's own words AUTHORIZED a purple/violet brand. The band is a
/// DEFAULT-REJECT, not a censor: a user who chose the hue gets the hue.
///
/// **Reads the run's STORED decision — it does not derive one.** "Did the user authorize
/// this hue?" is an intent question, answered ONCE per run by the brain at the run door
/// (`crate::color_permission`) and persisted into `.umadev/governance-context.json`. The
/// design floor is one of the three readers of that decision (with the PreToolUse hook and
/// `umadev ci`), and all three must read the SAME stored answer — a floor that re-derived
/// its own would be a fourth rule book, and the build would get a fix for one check that is
/// a violation of another.
///
/// STRICT when nothing is stored (no run door has asked, the context belongs to a different
/// requirement, the file is unreadable): permission withheld, band armed.
fn requirement_asks_for_purple(root: &Path, requirement: &str) -> bool {
    crate::planner::stored_color_permission(root, requirement)
}

/// The banned-hue check with the permission supplied directly — the unit-test seam. The
/// PERMISSION itself is the brain's to give (see `crate::color_permission`); what this
/// exercises is what the check does once it has one.
#[cfg(test)]
fn banned_hue_findings(ts: &TokenSet, purple_allowed: bool) -> Vec<Finding> {
    if purple_allowed {
        return Vec::new();
    }
    banned_hue_findings_in(BASE_THEME, ts)
}

/// The banned-hue check for ONE theme's token set. The caller has already decided
/// whether the user asked for purple (see `requirement_asks_for_purple`) — this is a
/// DEFAULT-reject of an unchosen hue, never a censor, and the SAME condition stands the
/// source-level `ai-color-palette` lint down.
fn banned_hue_findings_in(theme: &str, ts: &TokenSet) -> Vec<Finding> {
    let mut out = Vec::new();
    for role in ["primary", "accent", "brand", "cta"] {
        let Some(c) = ts.colors.get(role) else {
            continue;
        };
        if is_ai_purple(*c) {
            let o = c.oklch();
            let scope = if theme == BASE_THEME {
                String::new()
            } else {
                format!(" (in the `{theme}` theme)")
            };
            out.push(Finding {
                blocking: true,
                rule: RULE_BANNED_HUE,
                message: format!(
                    "design-system: `--color-{role}`{scope} sits in the AI indigo/violet band \
                     (OKLCH hue {:.0}, chroma {:.2}) — the single most recognizable AI tell, and \
                     nothing in the requirement asked for purple. Pick a hue this product OWNS \
                     (the design pack's primary) and re-measure its paired foreground",
                    o.h, o.c
                ),
            });
        }
    }
    out
}

// ===================================================================
// UD-CODE-007c — drift: literals in source not drawn from the token set
// ===================================================================

/// UI source extensions the drift scan reads.
const UI_EXTS: &[&str] = &[
    "tsx", "jsx", "vue", "svelte", "astro", "css", "scss", "sass", "less",
];

/// Whether this path IS a token file (it declares the tokens; it cannot drift
/// from itself) or a generated/vendored artifact — code nobody on the team wrote, whose
/// literals are not a design decision anyone made and cannot be "fixed" by reaching for
/// a token.
fn is_token_or_vendor(p: &Path) -> bool {
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let full = p.to_string_lossy().to_ascii_lowercase().replace('\\', "/");
    // SEGMENT-matched at any depth: the monorepo layout puts the generated tree at
    // `apps/web/.next/…`, and a vendored one at `packages/api/vendor/…`.
    const VENDOR_DIRS: &[&str] = &[
        "node_modules",
        "vendor",
        "dist",
        "build",
        ".next",
        ".nuxt",
        ".output",
        ".turbo",
        "coverage",
    ];
    name.starts_with("design-tokens")
        || name.contains("tokens.")
        || full.split('/').any(|seg| VENDOR_DIRS.contains(&seg))
}

fn drift_findings(
    root: &Path,
    ts: &TokenSet,
    budget: &mut crate::bounded_fs::Utf8ReadBudget,
) -> Vec<Finding> {
    // With no colors AND no type steps declared, we have no allowed-set to
    // compare against — every literal would "drift". Stay silent (fail-open);
    // the schema floor already said the real thing.
    if ts.colors.is_empty() && ts.type_steps.is_empty() && ts.radii.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Finding> = Vec::new();
    let mut scanned = 0usize;
    for f in crate::acceptance::source_files(root) {
        if scanned >= MAX_DRIFT_FILES || out.len() >= MAX_DRIFT_FINDINGS {
            break;
        }
        let ext = f
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !UI_EXTS.contains(&ext.as_str()) || is_token_or_vendor(&f) {
            continue;
        }
        let Ok(content) = budget.read_utf8_beneath(root, &f) else {
            continue;
        };
        scanned += 1;
        out.extend(drift_in_file(&rel(root, &f), &content, ts));
    }
    out.truncate(MAX_DRIFT_FINDINGS);
    out
}

/// Whether a drift finding may BLOCK the step.
///
/// It may NOT — drift is **advisory**, and that is a deliberate, conservative call.
/// The rule's premise ("a literal not in the token set is a mistake") is right about a
/// component's chrome and WRONG about several things that legitimately carry literal
/// color and are not design decisions at all: the fills inside an inline SVG logo, a
/// chart's categorical series palette, a third-party embed's required brand color. The
/// scan cannot yet tell those apart from a genuinely hardcoded button background, and a
/// rule that cannot tell them apart must not be the thing that stops a build: it would
/// demand the base "fix" a brand mark by pointing it at `--color-primary`, and there is
/// no edit that satisfies both the rule and the design.
///
/// So drift SPEAKS (it is exactly the signal a reviewer wants, and it is the truth about
/// most files) and does not BLOCK. The hard tell it exists to stop — an AI-purple
/// literal in the source — is caught and BLOCKED by the `ai-color-palette` lint on its
/// own terms. When the scan can name the exception precisely, this can be raised again.
const DRIFT_BLOCKS: bool = false;

/// Drift findings for ONE file. Split out so it is unit-testable without a tree.
fn drift_in_file(path: &str, content: &str, ts: &TokenSet) -> Vec<Finding> {
    let lower = umadev_governance::tokenizer::Tokenized::new(content)
        .without_comments(content)
        .to_ascii_lowercase();
    let mut out = Vec::new();

    // Colors: any hex / rgb() / oklch() literal not near a declared token.
    if !ts.colors.is_empty() {
        if let Some(lit) = first_color_literal(&lower, |c| !ts.allows_color(c)) {
            out.push(Finding {
                blocking: DRIFT_BLOCKS,
                rule: RULE_DRIFT,
                message: format!(
                    "design-system: `{path}` uses the literal color `{lit}`, which is not in the \
                     token set — reference the token (`var(--color-…)`), or add the color to \
                     `design-tokens` with its paired `on-` foreground if it is genuinely new"
                ),
            });
        }
    }

    // Fonts: a lead family neither declared nor a universal fallback.
    if !ts.fonts.is_empty() {
        if let Some(f) = umadev_governance::extract_fonts(&lower)
            .into_iter()
            .find(|f| !ts.allows_font(f))
        {
            out.push(Finding {
                blocking: DRIFT_BLOCKS,
                rule: RULE_DRIFT,
                message: format!(
                    "design-system: `{path}` uses the font family `{f}`, which the design system \
                     never declared — use `var(--font-…)`; two type systems in one product is a \
                     defect, not a variation"
                ),
            });
        }
    }

    // Radii + sizes: a literal px/rem not on the declared scale.
    if !ts.radii.is_empty() {
        if let Some(px) = first_len(&lower, "border-radius", |px| !ts.allows_radius(px)) {
            out.push(Finding {
                blocking: DRIFT_BLOCKS,
                rule: RULE_DRIFT,
                message: format!(
                    "design-system: `{path}` sets `border-radius: {px:.0}px`, which is not on the \
                     declared radius scale — use `var(--radius-…)` so shape stays one decision"
                ),
            });
        }
    }
    if !ts.type_steps.is_empty() {
        if let Some(px) = first_len(&lower, "font-size", |px| !ts.allows_size(px)) {
            out.push(Finding {
                blocking: DRIFT_BLOCKS,
                rule: RULE_DRIFT,
                message: format!(
                    "design-system: `{path}` sets `font-size: {px:.0}px`, which is not on the \
                     declared type scale — use `var(--text-…)`; an off-scale size is how a type \
                     hierarchy quietly dissolves"
                ),
            });
        }
    }
    out
}

/// The first color literal in `lower` for which `reject` is true.
fn rejected_hex_literal(
    lower: &str,
    marker: usize,
    digits: usize,
    reject: &impl Fn(Srgb) -> bool,
) -> Option<String> {
    let hex = lower.get(marker + 1..marker + 1 + digits)?;
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let next = lower[marker + 1 + digits..].chars().next();
    if next.is_some_and(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let color = parse_color(&format!("#{hex}"))?;
    reject(color).then(|| format!("#{hex}"))
}

fn first_color_literal(lower: &str, reject: impl Fn(Srgb) -> bool) -> Option<String> {
    // Hex literals.
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            for len in [6usize, 3] {
                if let Some(literal) = rejected_hex_literal(lower, i, len, &reject) {
                    return Some(literal);
                }
            }
        }
        i += 1;
    }
    // rgb()/oklch() literals.
    for marker in ["rgb(", "rgba(", "oklch("] {
        let mut from = 0;
        while let Some(idx) = lower[from..].find(marker) {
            let start = from + idx;
            let Some(close) = lower[start..].find(')') else {
                break;
            };
            let frag = &lower[start..=start + close];
            if let Some(c) = parse_color(frag) {
                // A fully-transparent / near-transparent scrim is not a brand
                // color decision — it composites. Never call it drift.
                if c.a >= 0.95 && reject(c) {
                    return Some(frag.to_string());
                }
            }
            from = start + close;
        }
    }
    None
}

/// The first `prop: <len>` value in `lower` for which `reject` is true.
fn first_len(lower: &str, prop: &str, reject: impl Fn(f64) -> bool) -> Option<f64> {
    let mut from = 0;
    while let Some(idx) = lower[from..].find(prop) {
        let at = from + idx;
        let after = at + prop.len();
        let rest = lower.get(after..).unwrap_or("");
        let trimmed = rest.trim_start();
        if trimmed.starts_with(':') {
            let vs = after + (rest.len() - trimmed.len()) + 1;
            let decl = lower.get(vs..).unwrap_or("");
            let end = decl
                .find([';', '\n', '}', '{'])
                .unwrap_or(decl.len().min(40));
            let value = decl[..end].trim();
            if !value.contains("var(") && !value.contains("calc(") {
                if let Some(px) = value.split_whitespace().find_map(length_px) {
                    if px > 0.0 && reject(px) {
                        return Some(px);
                    }
                }
            }
        }
        from = after.max(at + 1);
    }
    None
}

// ===================================================================
// UD-CODE-007e — the register-scoped design-lint registry
// ===================================================================

/// Run the register-scoped design-lint registry over the UI source.
///
/// `purple_allowed` is the SAME decision the token-level banned-hue rule makes, carried
/// here so the two cannot contradict each other. Without it, a user who says "our brand
/// color is violet `#7c3aed`" gets an unconvergeable build: the token rule accepts the
/// hue (the user asked for it) while this one blocks every component that uses it, and
/// no edit satisfies both.
fn lint_findings(
    root: &Path,
    register: Register,
    purple_allowed: bool,
    budget: &mut crate::bounded_fs::Utf8ReadBudget,
) -> Vec<Finding> {
    let intent = umadev_governance::DesignIntent { purple_allowed };
    let mut out = Vec::new();
    let mut scanned = 0usize;
    for f in crate::acceptance::source_files(root) {
        if scanned >= MAX_DRIFT_FILES || out.len() >= 10 {
            break;
        }
        let ext = f
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !UI_EXTS.contains(&ext.as_str()) || is_token_or_vendor(&f) {
            continue;
        }
        let Ok(content) = budget.read_utf8_beneath(root, &f) else {
            continue;
        };
        scanned += 1;
        let path = rel(root, &f);
        for d in umadev_governance::scan_design_rules_with(&path, &content, register, intent) {
            out.push(Finding {
                blocking: d.blocking(),
                rule: RULE_LINT,
                message: format!("design-system: `{path}` [{}] {}", d.rule, d.note),
            });
        }
    }
    out.truncate(10);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete, conformant token file — the shape the packs now ship.
    const GOOD: &str = r"
:root {
  --color-bg: #fafafa;        --color-on-bg: #18181b;
  --color-surface: #ffffff;   --color-on-surface: #18181b;
  --color-card: #ffffff;      --color-on-card: #3f3f46;
  --color-muted: #f4f4f5;     --color-on-muted: #52525b;
  --color-primary: #1d4ed8;   --color-on-primary: #ffffff;
  --color-accent: #0f766e;    --color-on-accent: #ffffff;
  --color-border: #e4e4e7;
  --text-xs: 0.75rem; --text-sm: 0.875rem; --text-base: 1rem;
  --text-lg: 1.125rem; --text-xl: 1.375rem; --text-2xl: 1.625rem;
  --space-1: 4px; --space-2: 8px; --space-4: 16px; --space-6: 24px; --space-8: 32px;
  --radius-sm: 6px; --radius-md: 8px; --radius-lg: 12px;
  --duration-fast: 120ms; --duration-normal: 180ms;
  --ease-standard: cubic-bezier(0.2, 0, 0.2, 1);
}
";

    fn good() -> TokenSet {
        parse_tokens(GOOD)
    }

    #[test]
    fn parses_css_custom_properties_into_a_token_set() {
        let ts = good();
        assert!(ts.colors.contains_key("primary"));
        assert!(ts.colors.contains_key("on-primary"));
        assert_eq!(ts.type_steps.len(), 6);
        assert_eq!(ts.radii.len(), 3);
        assert_eq!(ts.durations.len(), 2);
        assert_eq!(ts.easings.len(), 1);
        assert!(ts.spacing.len() >= 4);
    }

    #[test]
    fn parses_a_flat_json_token_file_too() {
        let json = r##"{
          "color-bg": "#ffffff", "color-on-bg": "#111111",
          "text-base": 16, "radius-md": 8, "duration-fast": "120ms"
        }"##;
        let ts = parse_tokens(json);
        assert!(ts.colors.contains_key("bg") && ts.colors.contains_key("on-bg"));
        assert_eq!(ts.type_steps, vec![16.0]);
        assert_eq!(ts.radii, vec![8.0]);
    }

    #[test]
    fn a_conformant_token_file_clears_the_schema_and_contrast_floors() {
        let ts = good();
        let s = schema_findings(&ts);
        assert!(
            s.is_empty(),
            "schema: {:?}",
            s.iter().map(|f| &f.message).collect::<Vec<_>>()
        );
        let c = contrast_findings(&ts);
        assert!(
            c.is_empty(),
            "contrast: {:?}",
            c.iter().map(|f| &f.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_old_theatre_token_file_is_now_rejected() {
        // `:root{--color-bg:#000}` used to PASS (the file merely existed).
        let ts = parse_tokens(":root{--color-bg:#000}");
        let f = schema_findings(&ts);
        assert!(
            !f.is_empty(),
            "a one-token file must not pass the schema floor"
        );
        assert!(f.iter().all(|x| x.blocking));
        assert!(f.iter().any(|x| x.message.contains("color role")));
    }

    #[test]
    fn an_unpaired_surface_token_is_a_blocking_schema_finding() {
        let mut raw = GOOD.replace("--color-on-accent: #ffffff;", "");
        raw.push('\n');
        let f = schema_findings(&parse_tokens(&raw));
        assert!(f.iter().any(|x| x.message.contains("--color-accent")));
    }

    #[test]
    fn a_failing_contrast_pair_names_both_tokens_and_the_measured_ratio() {
        let raw = GOOD.replace(
            "--color-on-primary: #ffffff;",
            "--color-on-primary: #8ab4f8;",
        );
        let f = contrast_findings(&parse_tokens(&raw));
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].blocking);
        assert!(f[0].message.contains("--color-on-primary"));
        assert!(f[0].message.contains("--color-primary"));
        assert!(f[0].message.contains(":1"), "must state the measured ratio");
        assert_eq!(f[0].rule, RULE_CONTRAST);
    }

    #[test]
    fn a_flat_type_scale_is_a_blocking_schema_finding() {
        let raw = "
:root{
  --color-bg:#fff; --color-on-bg:#111;
  --text-a: 16px; --text-b: 17px; --text-c: 18px; --text-d: 19px;
  --space-1: 4px; --space-2: 8px; --space-4: 16px; --space-6: 24px;
  --radius-md: 8px; --duration-fast: 100ms; --duration-slow: 300ms;
  --ease-standard: cubic-bezier(0.2,0,0.2,1);
}";
        let f = schema_findings(&parse_tokens(raw));
        assert!(f.iter().any(|x| x.message.contains("ratio")), "{f:?}");
    }

    #[test]
    fn an_off_grid_spacing_step_is_a_blocking_schema_finding() {
        let raw = GOOD.replace("--space-6: 24px;", "--space-6: 25px;");
        let f = schema_findings(&parse_tokens(&raw));
        assert!(f.iter().any(|x| x.message.contains("4pt grid")), "{f:?}");
    }

    #[test]
    fn a_declared_ai_purple_primary_is_rejected_unless_the_user_authorized_purple() {
        // The DEFAULT-REJECT: an AI-purple primary nobody authorized is a blocking finding.
        let raw = GOOD.replace("--color-primary: #1d4ed8;", "--color-primary: #6366f1;");
        let ts = parse_tokens(&raw);
        let f = banned_hue_findings(&ts, false);
        assert_eq!(f.len(), 1);
        assert!(f[0].blocking && f[0].rule == RULE_BANNED_HUE);
        assert!(f[0].message.contains("--color-primary"));
        // …and the ONE thing that stands it down is the run's recorded permission — the
        // brain's verdict on the requirement, not a reading of the requirement's words
        // taken here (see `crate::color_permission`; the token rule and the source-level
        // `ai-color-palette` lint must stand down on the SAME condition or no edit
        // satisfies both).
        assert!(banned_hue_findings(&ts, true).is_empty());
        // A real blue primary is never flagged, permission or not.
        assert!(banned_hue_findings(&good(), false).is_empty());
        assert!(banned_hue_findings(&good(), true).is_empty());
    }

    #[test]
    fn drift_speaks_but_does_not_block() {
        // Drift is ADVISORY. The rule cannot yet tell a hardcoded button background from
        // the fills inside an inline SVG logo or a chart's series palette, and a rule
        // that cannot tell them apart must not be the thing that stops a build — there
        // would be no edit that satisfies both the rule and the design. It still SPEAKS
        // (it is the right signal for a reviewer); it just never blocks.
        let ts = good();
        let f = drift_in_file("src/Card.tsx", ".card{color:#ff00ff}", &ts);
        assert!(f.iter().any(|x| x.message.contains("#ff00ff")), "{f:?}");
        assert!(f.iter().all(|x| x.rule == RULE_DRIFT));
        assert!(
            f.iter().all(|x| !x.blocking),
            "drift is advisory, never blocking: {f:?}"
        );

        // A token-drawn color (within the ±6/channel tolerance) is NOT drift.
        let ok = drift_in_file("src/Card.tsx", ".card{color:#1d4ed8}", &ts);
        assert!(ok.is_empty(), "{ok:?}");
        let near = drift_in_file("src/Card.tsx", ".card{color:#1e4fda}", &ts);
        assert!(near.is_empty(), "within tolerance: {near:?}");

        // `var()` references never drift.
        assert!(drift_in_file("src/C.tsx", ".c{color:var(--color-primary)}", &ts).is_empty());
    }

    #[test]
    fn drift_flags_off_scale_radius_size_and_undeclared_fonts() {
        let ts = good();
        let r = drift_in_file("src/C.css", ".card{border-radius:19px}", &ts);
        assert!(
            r.iter().any(|x| x.message.contains("border-radius")),
            "{r:?}"
        );
        // A declared radius passes; so does a pill.
        assert!(drift_in_file("src/C.css", ".card{border-radius:8px}", &ts).is_empty());
        assert!(drift_in_file("src/C.css", ".p{border-radius:9999px}", &ts).is_empty());

        let s = drift_in_file("src/C.css", ".x{font-size:13px}", &ts);
        assert!(s.iter().any(|x| x.message.contains("font-size")), "{s:?}");
        assert!(drift_in_file("src/C.css", ".x{font-size:16px}", &ts).is_empty());

        // Fonts: nothing declared in GOOD → the font check stays silent.
        assert!(drift_in_file("src/C.css", "h1{font-family:Papyrus}", &ts).is_empty());
        let mut with_font = parse_tokens(GOOD);
        with_font.fonts.push("inter".into());
        let ff = drift_in_file("src/C.css", "h1{font-family:Papyrus}", &with_font);
        assert!(ff.iter().any(|x| x.message.contains("papyrus")), "{ff:?}");
        // A universal fallback is always allowed.
        assert!(drift_in_file("src/C.css", "h1{font-family:system-ui}", &with_font).is_empty());
    }

    #[test]
    fn fail_open_no_token_file_no_findings() {
        let tmp = tempfile::tempdir().expect("tmp");
        let r = verify_design_system(tmp.path(), "anything", Register::Unknown);
        assert!(!r.available, "no token file → unavailable, not a failure");
        assert!(r.findings.is_empty());
        assert!(r.passed(), "an unavailable report conforms vacuously");
    }

    #[test]
    fn oversized_or_aggregate_exhausted_tokens_are_not_parsed_as_partial_contracts() {
        let oversized = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(oversized.path().join("src")).expect("mkdir");
        std::fs::write(
            oversized.path().join("src/design-tokens.css"),
            vec![b' '; MAX_DESIGN_FILE_BYTES + 1],
        )
        .expect("write");
        assert!(!verify_design_system(oversized.path(), "ui", Register::Product).available);

        let aggregate = tempfile::tempdir().expect("tmp");
        let file_len = MAX_TOKEN_TOTAL_BYTES / 5 + 1024;
        for index in 0..5 {
            let dir = aggregate.path().join(format!("area-{index}"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let mut body = GOOD.to_string();
            body.push_str(&" ".repeat(file_len - body.len()));
            std::fs::write(dir.join("design-tokens.css"), body).expect("write");
        }
        assert!(file_len <= MAX_DESIGN_FILE_BYTES);
        assert!(5 * file_len > MAX_TOKEN_TOTAL_BYTES);
        assert!(!verify_design_system(aggregate.path(), "ui", Register::Product).available);
    }

    #[cfg(unix)]
    #[test]
    fn visual_direction_rejects_a_symlinked_workspace_document() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::create_dir_all(workspace.path().join("output")).expect("mkdir");
        let secret = outside.path().join("secret.md");
        std::fs::write(&secret, "## Visual direction\nregister: brand\n").expect("write");
        symlink(&secret, workspace.path().join("output/demo-uiux.md")).expect("symlink");

        assert!(visual_direction_section(workspace.path(), "demo").is_empty());
    }

    #[test]
    fn end_to_end_a_good_token_file_and_a_conformant_ui_pass() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        std::fs::write(tmp.path().join("src/design-tokens.css"), GOOD).expect("write");
        std::fs::write(
            tmp.path().join("src/Card.tsx"),
            "export const Card = () => <div style={{color:'var(--color-on-card)'}}/>;",
        )
        .expect("write");
        let r = verify_design_system(tmp.path(), "a task tracker", Register::Product);
        assert!(r.available);
        assert!(
            r.passed(),
            "conformant project must pass: {:?}",
            r.blocking().iter().map(|f| &f.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn end_to_end_a_purple_hardcoding_ui_is_blocked_with_actionable_evidence() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        std::fs::write(tmp.path().join("src/design-tokens.css"), GOOD).expect("write");
        std::fs::write(
            tmp.path().join("src/Hero.tsx"),
            "export const Hero = () => <h1 style={{color:'#8b5cf6'}}>hi</h1>;",
        )
        .expect("write");
        let r = verify_design_system(tmp.path(), "a task tracker", Register::Product);
        assert!(!r.passed());
        let msgs: Vec<&str> = r.blocking().iter().map(|f| f.message.as_str()).collect();
        assert!(
            msgs.iter().any(|m| m.contains("#8b5cf6")),
            "must name the literal: {msgs:?}"
        );
        // Both the drift rule and the lint registry have something to say, and
        // BOTH tell the base what to do instead.
        assert!(msgs
            .iter()
            .any(|m| m.contains("token") || m.contains("Do this instead")));
    }

    #[test]
    fn never_panics_on_junk_token_files() {
        for junk in [
            "",
            "{{{{",
            ":root{--color-bg:}",
            "--color-: #",
            &"色".repeat(500),
            ":root{--color-primary: var(--color-brand)}",
        ] {
            let ts = parse_tokens(junk);
            let _ = schema_findings(&ts);
            let _ = contrast_findings(&ts);
            let _ = banned_hue_findings(&ts, false);
            let _ = drift_in_file("a.css", junk, &ts);
        }
    }

    // ── UD-CODE-007f: the designer's direction step ───────────────────────

    /// A complete `## Visual direction` — the shape the designer seat must land.
    const DIRECTION: &str = "\
# UIUX

## Visual direction

Design read: an internal ops console for warehouse supervisors — register: product —
calm, dense, boringly reliable — aesthetic family: tech-utility.

- Color commitment: restrained. Color is a status signal, never decoration.
- Theme: supervisors work a 12-hour shift on a wall-mounted screen in a loading bay
  lit by overhead sodium lamps and open shutter doors at noon; glare is constant and
  they glance from 3 metres away. That forces a light theme with heavy contrast.
- Anchors:
  - density: from a flight-status board — many true rows, one line each, no cards.
  - type: from a transit signage system — one neutral face, tabular figures.
  - whitespace: from a spreadsheet — tight rows, generous gutters between columns.

Anti-goals: not a consumer dashboard, not a marketing surface, no illustration, no
onboarding delight, no motion beyond a 120ms press confirm.

## Tokens
";

    fn write_uiux(dir: &Path, body: &str) {
        let out = dir.join("output");
        std::fs::create_dir_all(&out).expect("mkdir");
        std::fs::write(out.join("demo-uiux.md"), body).expect("write");
    }

    #[test]
    fn a_complete_visual_direction_passes_and_yields_its_register() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_uiux(tmp.path(), DIRECTION);
        let f = visual_direction_findings(tmp.path(), "demo", true);
        assert!(
            f.is_empty(),
            "{:?}",
            f.iter().map(|x| &x.message).collect::<Vec<_>>()
        );
        assert_eq!(register_for_project(tmp.path(), "demo"), Register::Product);
    }

    #[test]
    fn a_missing_direction_section_blocks_with_the_full_recipe() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_uiux(tmp.path(), "# UIUX\n\n## Tokens\n\n:root{}\n");
        let f = visual_direction_findings(tmp.path(), "demo", true);
        assert_eq!(f.len(), 1);
        assert!(f[0].blocking && f[0].rule == RULE_DIRECTION);
        // The finding must TEACH the slots, not just refuse.
        for slot in [
            "design read",
            "register",
            "restrained",
            "PHYSICAL SCENE",
            "anti-goals",
        ] {
            assert!(
                f[0].message.contains(slot),
                "missing `{slot}` in: {}",
                f[0].message
            );
        }
    }

    #[test]
    fn each_forced_decision_is_individually_required() {
        let tmp = tempfile::tempdir().expect("tmp");

        // (a) no color commitment level.
        write_uiux(
            tmp.path(),
            &DIRECTION.replace("Color commitment: restrained.", "Color: nice."),
        );
        assert!(visual_direction_findings(tmp.path(), "demo", true)[0]
            .message
            .contains("COLOR COMMITMENT LEVEL"));

        // (b) a theme with no physical scene behind it.
        let no_scene = DIRECTION
            .lines()
            .filter(|l| {
                !l.contains("supervisors work")
                    && !l.contains("lit by")
                    && !l.contains("they glance")
            })
            .collect::<Vec<_>>()
            .join("\n");
        write_uiux(tmp.path(), &no_scene);
        assert!(visual_direction_findings(tmp.path(), "demo", true)[0]
            .message
            .contains("physical-scene"));

        // (c) adjectives instead of bound anchors.
        let vague = DIRECTION.replace(
            "  - density: from a flight-status board — many true rows, one line each, no cards.\n  \
             - type: from a transit signage system — one neutral face, tabular figures.\n  \
             - whitespace: from a spreadsheet — tight rows, generous gutters between columns.",
            "  - modern\n  - clean",
        );
        write_uiux(tmp.path(), &vague);
        assert!(visual_direction_findings(tmp.path(), "demo", true)[0]
            .message
            .contains("NAMED anchor"));

        // (d) no anti-goals.
        let no_anti = DIRECTION
            .lines()
            .filter(|l| !l.contains("Anti-goals"))
            .collect::<Vec<_>>()
            .join("\n");
        write_uiux(tmp.path(), &no_anti);
        assert!(visual_direction_findings(tmp.path(), "demo", true)[0]
            .message
            .contains("ANTI-GOALS"));
    }

    // ── BLOCKER: "our brand color is purple" must not be honored by one rule
    //    and blocked by another (an unconvergeable build) ────────────────────

    #[test]
    fn a_requested_purple_brand_is_honored_by_every_rule_that_reads_the_band() {
        // Two checks read the same indigo/violet band: the token-level banned-hue rule
        // (`007d`) and the source-level `ai-color-palette` lint (`007e`). If only ONE of
        // them honours "our brand color is violet #7c3aed", the two contradict each
        // other — the tokens are accepted, every component that uses them is blocked,
        // and the fix for one is the violation of the other. There is no edit that
        // converges. So: BOTH stand down on the SAME condition, or neither does.
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        let purple_tokens = GOOD
            .replace("--color-primary: #1d4ed8;", "--color-primary: #7c3aed;")
            .replace("--color-accent: #0f766e;", "--color-accent: #7c3aed;");
        std::fs::write(tmp.path().join("src/design-tokens.css"), &purple_tokens).expect("write");
        std::fs::write(
            tmp.path().join("src/Hero.tsx"),
            "export const Hero = () => <h1 style={{color:'#7c3aed'}}>hi</h1>;",
        )
        .expect("write");

        // THE RUN asked the brain and recorded its verdict — the ONE thing that stands the
        // band down (`crate::color_permission`). The floor READS that stored decision; it
        // does not re-derive one from the words, because a word list cannot answer "did the
        // user authorize this hue?" (it was tried, and it leaked on every review round).
        let requirement = "our brand color is violet #7c3aed — use it for the primary";
        crate::planner::persist_project_context_with_color(requirement, tmp.path(), "brand", true);
        let asked = verify_design_system(tmp.path(), requirement, Register::Product);
        assert!(
            asked.passed(),
            "a requested purple brand must converge, not block: {:?}",
            asked
                .blocking()
                .iter()
                .map(|f| &f.message)
                .collect::<Vec<_>>()
        );
        assert!(
            !asked
                .blocking()
                .iter()
                .any(|f| f.rule == RULE_BANNED_HUE || f.rule == RULE_LINT),
            "neither the token rule nor the source lint may fire once the user asked"
        );

        // Nobody asked → the default-reject still holds, on BOTH surfaces. Note the stored
        // permission from above is STILL on disk: it belongs to the violet requirement, and a
        // permission derived from one requirement is not a permission for another. Provenance
        // (`ProjectContext::if_current`) is what makes a stale grant harmless.
        let unasked =
            verify_design_system(tmp.path(), "build me a task tracker", Register::Product);
        assert!(!unasked.passed(), "an UNCHOSEN purple is still rejected");
        let rules: Vec<&str> = unasked.blocking().iter().map(|f| f.rule).collect();
        assert!(
            rules.contains(&RULE_BANNED_HUE),
            "the token-level rule still fires: {rules:?}"
        );
        assert!(
            rules.contains(&RULE_LINT),
            "the source-level lint still fires: {rules:?}"
        );
    }

    // ── HIGH: the token schema must accept the dominant React idiom ──────────

    /// The unprefixed convention: `--primary` / `--primary-foreground`, no `color-`
    /// prefix, `-foreground` rather than `on-`.
    const REACT_IDIOM: &str = r"
:root {
  --background: #ffffff;      --foreground: #09090b;
  --card: #ffffff;            --card-foreground: #09090b;
  --popover: #ffffff;         --popover-foreground: #09090b;
  --primary: #1d4ed8;         --primary-foreground: #ffffff;
  --secondary: #f4f4f5;       --secondary-foreground: #27272a;
  --muted: #f4f4f5;           --muted-foreground: #52525b;
  --accent: #0f766e;          --accent-foreground: #ffffff;
  --destructive: #b91c1c;     --destructive-foreground: #ffffff;
  --border: #e4e4e7;          --input: #e4e4e7;  --ring: #1d4ed8;
  --chart-1: #ef4444;         --chart-2: #22c55e;
  --text-xs: 0.75rem; --text-sm: 0.875rem; --text-base: 1rem;
  --text-lg: 1.125rem; --text-xl: 1.375rem;
  --space-1: 4px; --space-2: 8px; --space-4: 16px; --space-6: 24px;
  --radius-sm: 6px; --radius-md: 8px;
  --duration-fast: 120ms; --duration-normal: 180ms;
  --ease-standard: cubic-bezier(0.2, 0, 0.2, 1);
}
";

    #[test]
    fn the_unprefixed_react_token_idiom_clears_the_schema_floor() {
        // `--primary` / `--primary-foreground` is the dominant React convention. A schema
        // that only knows `--color-on-<role>` reads EVERY surface as unpaired → a
        // blocking finding per role → a rework loop the base can only exit by abandoning
        // the idiom its whole component library is built on.
        let ts = parse_tokens(REACT_IDIOM);
        assert!(ts.colors.contains_key("primary"), "{:?}", ts.colors.keys());
        assert!(ts.colors.contains_key("primary-foreground"));
        assert!(ts.colors.contains_key("background") && ts.colors.contains_key("foreground"));

        let f = schema_findings(&ts);
        assert!(
            f.is_empty(),
            "the React idiom is a real design system: {:?}",
            f.iter().map(|x| &x.message).collect::<Vec<_>>()
        );
        // …and its pairs are MEASURED, exactly like the `on-` idiom's.
        assert!(contrast_findings(&ts).is_empty());
        let bad = parse_tokens(&REACT_IDIOM.replace(
            "--primary-foreground: #ffffff;",
            "--primary-foreground: #8ab4f8;",
        ));
        let c = contrast_findings(&bad);
        assert_eq!(c.len(), 1, "{c:?}");
        assert!(c[0].message.contains("--color-primary-foreground"), "{c:?}");

        // A chart series is DATA, not a surface — it owes no foreground.
        assert!(!is_surface_role("chart-1"));
        // A genuinely unpaired surface is still caught, in either idiom.
        let unpaired = parse_tokens(&REACT_IDIOM.replace("--card-foreground: #09090b;", ""));
        assert!(schema_findings(&unpaired)
            .iter()
            .any(|x| x.message.contains("--color-card")));
    }

    #[test]
    fn light_and_dark_themes_are_measured_separately() {
        // A file that declares BOTH themes used to keep only the LAST value per role: the
        // dark theme overwrote the light one, so contrast was measured for a theme that
        // ships to nobody, an unreadable dark pair went unmeasured, and every literal of
        // the theme that lost read as drift.
        let both = format!(
            "{GOOD}\n@media (prefers-color-scheme: dark) {{\n  :root {{\n    \
             --color-bg: #0b0b0c; --color-on-bg: #101014;\n  }}\n}}\n"
        );
        let themes = parse_theme_tokens(&both);
        assert_eq!(themes.len(), 2, "both themes are parsed: {themes:?}");
        // The BASE still holds its own light values (they were not overwritten).
        let base = &themes[0].1;
        assert_eq!(base.colors["bg"], parse_color("#fafafa").unwrap());
        assert!(contrast_findings_in("base", base).is_empty());
        // The DARK theme is measured on its own — and its unreadable pair is caught.
        let dark = &themes[1].1;
        assert_eq!(dark.colors["bg"], parse_color("#0b0b0c").unwrap());
        let d = contrast_findings_in("dark", dark);
        assert_eq!(d.len(), 1, "the dark pair is measured: {d:?}");
        assert!(d[0].message.contains("dark"), "{d:?}");

        // The union is the ALLOWED set: a dark-theme color used in source is NOT drift.
        let union = union_tokens(&themes);
        assert!(union.allows_color(parse_color("#0b0b0c").unwrap()));
        assert!(union.allows_color(parse_color("#fafafa").unwrap()));
        // …and the union invents no unpaired role out of the override.
        assert!(
            schema_findings(&union).is_empty(),
            "{:?}",
            schema_findings(&union)
        );
    }

    #[test]
    fn vendored_and_generated_files_never_drift() {
        for p in [
            "node_modules/x/a.css",
            "vendor/bootstrap/b.css",
            "apps/web/.next/static/c.css",
            "packages/ui/dist/d.css",
            "coverage/report.css",
        ] {
            assert!(
                is_token_or_vendor(Path::new(p)),
                "`{p}` is not the team's code — its literals are not a design decision"
            );
        }
        assert!(!is_token_or_vendor(Path::new("src/components/Card.tsx")));
    }

    // ── HIGH: the visual-direction check must not block a BACKEND-only run ───

    #[test]
    fn the_visual_direction_check_is_gated_on_the_route_not_on_a_file() {
        // A brownfield repo — or simply a SECOND run in a workspace where an earlier UI
        // build left `output/<slug>-uiux.md` behind — still has the doc on disk. Gating
        // on the file hands a pure backend task a blocking design finding it can neither
        // act on nor escape.
        let tmp = tempfile::tempdir().expect("tmp");
        write_uiux(tmp.path(), "# UIUX\n\n## Tokens\n\n:root{}\n"); // no direction section
        assert!(
            visual_direction_findings(tmp.path(), "demo", false).is_empty(),
            "a backend-only run is not held to a design contract it never entered"
        );
        // The same tree, on a UI-bearing run → the one blocking finding still fires.
        let f = visual_direction_findings(tmp.path(), "demo", true);
        assert_eq!(f.len(), 1);
        assert!(f[0].blocking);
    }

    #[test]
    fn only_a_wholly_absent_direction_section_blocks_the_rest_is_advisory() {
        // The sub-clauses are keyword tests over PROSE, and a keyword test over prose has
        // false negatives — a direction that says "a bright open-plan office; the app is
        // light-first" decided the theme by a physical scene and names no keyword this
        // scan knows. Blocking on that blocks a correct answer for being phrased
        // unexpectedly, and the rework it demands ("add the word `dark`") makes the
        // document worse. It speaks; it does not block.
        let tmp = tempfile::tempdir().expect("tmp");
        write_uiux(
            tmp.path(),
            "# UIUX\n\n## Visual direction\n\nA bright open-plan office; the app is \
             light-first and stays out of the way.\n\n## Tokens\n",
        );
        let f = visual_direction_findings(tmp.path(), "demo", true);
        assert!(!f.is_empty(), "it still SAYS what is missing");
        assert!(
            f.iter().all(|x| !x.blocking),
            "an incomplete-but-present direction is advisory: {:?}",
            f.iter().map(|x| &x.message).collect::<Vec<_>>()
        );

        // The ONE objective miss — no section at all — still blocks.
        write_uiux(tmp.path(), "# UIUX\n\n## Tokens\n\n:root{}\n");
        let gone = visual_direction_findings(tmp.path(), "demo", true);
        assert_eq!(gone.len(), 1);
        assert!(gone[0].blocking && gone[0].rule == RULE_DIRECTION);
    }

    #[test]
    fn no_uiux_doc_means_no_direction_finding() {
        let tmp = tempfile::tempdir().expect("tmp");
        assert!(
            visual_direction_findings(tmp.path(), "demo", true).is_empty(),
            "a project with no design phase is not failed for skipping one"
        );
        assert_eq!(register_for_project(tmp.path(), "demo"), Register::Unknown);
    }

    // ── the knowledge corpus must satisfy its own contrast law ────────────

    #[test]
    fn every_palette_row_in_the_product_type_map_passes_wcag() {
        // The recommendation table is the FIRST thing a designer copies. If a
        // row shipped a failing pair, we would be seeding the defect we then
        // block. Parse every row and measure it.
        const MAP: &str =
            include_str!("../../../knowledge/design-systems/product-type-design-map.md");
        let mut rows = 0usize;
        for line in MAP.lines() {
            let l = line.trim();
            if !l.starts_with('|') || l.contains("---") || l.contains("Primary") {
                continue;
            }
            let cells: Vec<&str> = l.trim_matches('|').split('|').map(str::trim).collect();
            // | type | register | style | primary | accent | bg | fg | ... |
            if cells.len() < 7 {
                continue;
            }
            let hex = |c: &str| parse_color(c.trim().trim_matches('`'));
            let (Some(primary), Some(accent), Some(bg), Some(fg)) =
                (hex(cells[3]), hex(cells[4]), hex(cells[5]), hex(cells[6]))
            else {
                continue;
            };
            rows += 1;
            let name = cells[0];
            let body = contrast_ratio(fg, bg);
            assert!(
                body >= CONTRAST_BODY,
                "{name}: foreground on background is {body:.2}:1 (need >= {CONTRAST_BODY})"
            );
            for (label, c) in [("primary", primary), ("accent", accent)] {
                let r = contrast_ratio(c, bg);
                assert!(
                    r >= CONTRAST_UI,
                    "{name}: {label} on background is {r:.2}:1 (need >= {CONTRAST_UI})"
                );
            }
            // And no row may recommend the AI-purple band.
            assert!(
                !is_ai_purple(primary),
                "{name}: primary is in the AI-purple band"
            );
            assert!(
                !is_ai_purple(accent),
                "{name}: accent is in the AI-purple band"
            );
        }
        assert!(
            rows >= 15,
            "expected the full recommendation table, parsed {rows} rows"
        );
    }

    #[test]
    fn every_shipped_design_pack_declares_a_conformant_base_palette() {
        // Our own packs are the reference implementation of the contract. If a
        // pack cannot pass the floor, no project built from it can either.
        const PACKS: &[(&str, &str)] = &[
            (
                "bold-geometric",
                include_str!("../../../knowledge/design-systems/bold-geometric.md"),
            ),
            (
                "brutalist-bold",
                include_str!("../../../knowledge/design-systems/brutalist-bold.md"),
            ),
            (
                "editorial-clean",
                include_str!("../../../knowledge/design-systems/editorial-clean.md"),
            ),
            (
                "glass-aurora",
                include_str!("../../../knowledge/design-systems/glass-aurora.md"),
            ),
            (
                "modern-minimal",
                include_str!("../../../knowledge/design-systems/modern-minimal.md"),
            ),
            (
                "premium-luxury",
                include_str!("../../../knowledge/design-systems/premium-luxury.md"),
            ),
            (
                "soft-warm",
                include_str!("../../../knowledge/design-systems/soft-warm.md"),
            ),
            (
                "tech-utility",
                include_str!("../../../knowledge/design-systems/tech-utility.md"),
            ),
        ];
        for (name, body) in PACKS {
            // The BASE `:root` block (before any `@media` scheme override).
            let start = body.find(":root").unwrap_or(0);
            let end = body[start..]
                .find("@media")
                .map_or(body.len(), |e| start + e);
            let ts = parse_tokens(&body[start..end]);

            let schema = schema_findings(&ts);
            assert!(
                schema.is_empty(),
                "{name}: {:?}",
                schema.iter().map(|f| &f.message).collect::<Vec<_>>()
            );
            let contrast = contrast_findings(&ts);
            assert!(
                contrast.is_empty(),
                "{name}: {:?}",
                contrast.iter().map(|f| &f.message).collect::<Vec<_>>()
            );
            // No pack may ship an AI-purple brand hue.
            let hue = banned_hue_findings(&ts, false);
            assert!(
                hue.is_empty(),
                "{name}: {:?}",
                hue.iter().map(|f| &f.message).collect::<Vec<_>>()
            );
        }
    }
}
