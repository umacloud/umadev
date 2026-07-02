//! Durable per-project **OPEN-DECISIONS** memory — the third durable memory
//! channel, a committed parking-lot register for the items a run leaves
//! undecided, deferred, blocked, or parked pending a future trigger.
//!
//! ## Why this exists (a decision-loss bug)
//!
//! During execution — after the spec + tasks are set — a run constantly produces
//! items that are NOT resolvable right now: a missing external key, a downstream
//! task dependency, an ambiguous design decision that needs re-evaluation, an
//! open question, a deferred validation, a boundary held with reservations. Held
//! only in working memory or mentioned once in chat, these fall out of UmaDev's
//! bounded transcript AND out of the base's own context window and are **lost** —
//! no traceability, no resurfacing. Durable FACT memory ([`crate::project_facts`])
//! persists *resolved* facts; the pitfall ledger ([`crate::lessons`]) persists
//! *pitfalls*. Neither keeps the *open, still-unsettled* items a real dev team
//! writes onto a parking-lot / open-decisions list so nothing is dropped.
//!
//! This module is that place — a SIBLING of the durable fact store, modelled on
//! the same read/recall/bound pattern, but deliberately **project-visible and
//! committed** (it lives at [`REGISTER_REL_PATH`] under `docs/`, NOT under the
//! gitignored `.umadev/`) because open decisions are a thing the team + the user
//! should be able to read, review, and diff.
//!
//! ## The loop
//!
//! - **RECORD** — the base APPENDS an entry to the register with its own file
//!   tools, in the Markdown shape the firmware directive
//!   ([`decisions_directive`]) documents (a `## OPEN — <category> — <title>`
//!   heading + structured fields). The register is **append-only** and resolved
//!   **in place** (an item is closed by flipping its heading to `## RESOLVED`,
//!   never by deleting it — the trail must survive). [`append_decision`] is the
//!   Rust-side sibling of that append for callers/tests that want to record from
//!   code; it locks the on-disk shape the directive documents.
//! - **RECALL** — [`decisions_recall_block`] renders the still-UNRESOLVED items
//!   as a compact, token-budgeted block prefixed with the
//!   `(N unresolved + M resolved)` summary, which [`crate::context::compose_firmware`]
//!   injects on every WORK turn — so a prior open item auto-resurfaces into the
//!   base's context at task/phase start instead of relying on it to remember to
//!   re-read the file.
//!
//! ## Bounded + fail-open by contract
//!
//! Parsing is capped at [`MAX_ENTRIES`] entries with each field truncated, and
//! the recall block is capped at [`DECISIONS_FIRMWARE_BUDGET`] characters AND
//! [`MAX_RECALLED_ITEMS`] items — so the prompt can never bloat under a register
//! that grew to dozens of entries. Every path is fail-open: a missing file, an
//! unreadable file, or malformed / garbage content degrades to "no open
//! decisions" and behaves exactly as before — this module NEVER panics and NEVER
//! returns an error that could block the base.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Repo-relative path of the **project-visible, committed** open-decisions
/// register. Under `docs/` (a normal, diffable project doc) ON PURPOSE — open
/// decisions are meant to be read + reviewed, unlike the gitignored `.umadev/`
/// memory stores. One constant so the parser, the recall block, the append
/// helper, and the firmware directive all name the same file.
pub const REGISTER_REL_PATH: &str = "docs/decisions/OPEN-DECISIONS.md";

/// The three canonical categories an open item is filed under — the exact set a
/// real parking-lot discipline produces:
/// - `waiting-on-external-condition` — blocked on something outside this run (a
///   missing key/credential, a downstream task, an upstream answer).
/// - `design-decision-to-evaluate` — an ambiguous design choice deferred for
///   re-evaluation once more is known.
/// - `existing-design-boundary` — a boundary/limitation accepted with
///   reservations, to revisit if it starts to bite.
pub const CATEGORIES: [&str; 3] = [
    "waiting-on-external-condition",
    "design-decision-to-evaluate",
    "existing-design-boundary",
];

/// Hard cap on entries parsed from the register, so a long-lived project's
/// committed register never bloats parse memory. When exceeded, the OLDEST
/// entries (top of file) are dropped; the recall block is bounded separately.
const MAX_ENTRIES: usize = 256;

/// How many UNRESOLVED items the recall block may list — a small, high-signal
/// digest, never the whole register. The remaining unresolved items still live
/// in the (committed) file; recall surfaces the most-at-risk ones.
const MAX_RECALLED_ITEMS: usize = 12;

/// Per-entry cap on the title length (chars).
const MAX_TITLE_CHARS: usize = 160;

/// Per-field cap on an extracted field value (chars) surfaced in recall.
const MAX_FIELD_CHARS: usize = 200;

/// Per-recall-line cap (chars), so one runaway entry can't dominate the block.
const MAX_RECALL_LINE_CHARS: usize = 280;

/// Character budget for the firmware **recall** block. Tight by design: the
/// recall rides in the always-on work-class head on TOP of identity + craft +
/// the facts recall, so it must stay a small, high-signal overlay.
/// [`decisions_recall_block`] fills the unresolved list up to this budget and
/// never exceeds it.
pub const DECISIONS_FIRMWARE_BUDGET: usize = 1_600;

/// Whether an entry is still open or has been resolved in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionStatus {
    /// Still undecided / deferred / blocked — surfaces in recall.
    Open,
    /// Closed in place (heading flipped to `## RESOLVED`, kept for the trail) —
    /// counted, but not recalled.
    Resolved,
}

/// One parsed entry from the open-decisions register.
///
/// Built from a `## OPEN|RESOLVED — <category> — <title>` heading plus its
/// structured field lines. The extracted fields ([`Self::open_item`] /
/// [`Self::resolves_when`]) are the high-signal ones the recall line renders;
/// the full body stays in the committed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenDecision {
    /// Open vs resolved (defaults to [`DecisionStatus::Open`] when the heading
    /// carries no clear status — safer to surface than to silently hide).
    pub status: DecisionStatus,
    /// Canonical category slug (one of [`CATEGORIES`]) when the heading names a
    /// known category, else the raw category token, else `None`.
    pub category: Option<String>,
    /// Short title of the item (the heading with the status + category stripped).
    pub title: String,
    /// The `Open item` field value, when present — what is undecided/deferred.
    pub open_item: Option<String>,
    /// The `Resolves when` field value, when present — the condition/trigger
    /// that closes it.
    pub resolves_when: Option<String>,
    /// The `Blocked by` field value, when present — what blocks a decision.
    pub blocked_by: Option<String>,
}

/// Fields for a NEW open-decision entry to append to the register from Rust.
///
/// Mirrors the Markdown shape the firmware directive documents, so an
/// [`append_decision`] round-trips through [`load_decisions`] — locking the
/// on-disk contract the base is told to follow.
#[derive(Debug, Clone, Default)]
pub struct NewDecision {
    /// Category slug (ideally one of [`CATEGORIES`]).
    pub category: String,
    /// Short title.
    pub title: String,
    /// ISO date string (`YYYY-MM-DD`); caller-provided so the render stays
    /// deterministic + testable.
    pub date: String,
    /// Originating request / ADR / task id.
    pub source: String,
    /// The undecided / deferred / blocked thing.
    pub open_item: String,
    /// Constraints that bound the item.
    pub related_constraints: String,
    /// Current best guess, if any.
    pub current_leaning: String,
    /// What blocks a decision.
    pub blocked_by: String,
    /// The condition / trigger that resolves it.
    pub resolves_when: String,
}

/// Absolute path of the register for a given project root.
fn register_path(root: &Path) -> PathBuf {
    root.join(REGISTER_REL_PATH)
}

/// Load + parse all entries from the register for `root`, oldest FIRST.
///
/// Fail-open + forgiving: a missing/unreadable file yields an empty vec;
/// malformed content simply yields fewer (or zero) entries — never an error or a
/// panic. Bounded at [`MAX_ENTRIES`] (oldest dropped).
#[must_use]
pub fn load_decisions(root: &Path) -> Vec<OpenDecision> {
    let Ok(text) = std::fs::read_to_string(register_path(root)) else {
        return Vec::new();
    };
    parse_register(&text)
}

/// The still-UNRESOLVED entries for `root`, oldest FIRST (the ones most at risk
/// of being forgotten lead). Fail-open (empty when there's no register).
#[must_use]
pub fn unresolved(root: &Path) -> Vec<OpenDecision> {
    load_decisions(root)
        .into_iter()
        .filter(|d| d.status == DecisionStatus::Open)
        .collect()
}

/// `(unresolved, resolved)` entry counts for `root`. Fail-open → `(0, 0)`.
#[must_use]
pub fn counts(root: &Path) -> (usize, usize) {
    split_counts(&load_decisions(root))
}

/// `(unresolved, resolved)` over an already-loaded slice.
fn split_counts(all: &[OpenDecision]) -> (usize, usize) {
    let resolved = all
        .iter()
        .filter(|d| d.status == DecisionStatus::Resolved)
        .count();
    (all.len() - resolved, resolved)
}

/// The firmware **directive** — the static, byte-stable record-to-register
/// guidance the base sees on every WORK turn. It documents WHEN to record (any
/// undecided / deferred / blocked / pending-trigger item), WHERE
/// ([`REGISTER_REL_PATH`]), the append-only + resolved-in-place discipline, the
/// three [`CATEGORIES`], and the exact entry shape (its structured fields).
///
/// Deliberately a `&'static str` (no per-turn interpolation) so it can live in
/// the KV-cache-stable prefix — it is a fixed policy, like the anti-slop law,
/// not per-turn data. The volatile part (the recalled unresolved items) is
/// [`decisions_recall_block`].
#[must_use]
pub fn decisions_directive() -> &'static str {
    "## OPEN-DECISIONS DISCIPLINE (parking-lot register — never lose a deferred decision)\n\n\
     Whenever something is left undecided, deferred, blocked, or parked pending a future trigger — \
     a missing external key/credential, a downstream task dependency, an ambiguous design decision \
     to re-evaluate, an open question, a deferred validation, or a boundary held with reservations — \
     APPEND it to `docs/decisions/OPEN-DECISIONS.md`. NEVER leave it only in working memory or a chat \
     message, where it is lost with no traceability. The register is APPEND-ONLY and RESOLVED-IN-PLACE: \
     to close an item, change its `## OPEN` heading to `## RESOLVED` and add a `- **Resolution**:` line \
     — do NOT delete it, so the decision trail survives.\n\n\
     CATEGORY is one of: waiting-on-external-condition | design-decision-to-evaluate | \
     existing-design-boundary. Use this exact entry shape:\n\n\
     ## OPEN — <category> — <short title>\n\
     - **Date**: <YYYY-MM-DD>\n\
     - **Source**: <originating request / ADR / task>\n\
     - **Open item**: <the undecided / deferred / blocked thing>\n\
     - **Related constraints**: <constraints that bound it>\n\
     - **Current leaning**: <current best guess, or \"none yet\">\n\
     - **Blocked by**: <what blocks a decision>\n\
     - **Resolves when**: <the condition / trigger that resolves it>"
}

/// The firmware **recall** block: the still-UNRESOLVED entries as a compact,
/// token-budgeted list prefixed with the `(N unresolved + M resolved)` summary,
/// so prior open items auto-resurface at task/phase start.
///
/// Empty string when the register has NO unresolved items (fail-open / a fresh
/// project) — the firmware then relies on the always-on [`decisions_directive`]
/// alone (0 recall tokens). The block is bounded by BOTH `budget_chars`
/// (typically [`DECISIONS_FIRMWARE_BUDGET`]) and [`MAX_RECALLED_ITEMS`]: the
/// unresolved list is filled until either cap is hit, so a register grown to
/// dozens of entries can never bloat the prompt. Deterministic (file order, no
/// timestamps, one store read).
#[must_use]
pub fn decisions_recall_block(root: &Path, budget_chars: usize) -> String {
    let all = load_decisions(root);
    let (n_unresolved, m_resolved) = split_counts(&all);
    let open: Vec<&OpenDecision> = all
        .iter()
        .filter(|d| d.status == DecisionStatus::Open)
        .collect();
    if open.is_empty() {
        return String::new();
    }

    let header = format!(
        "## OPEN DECISIONS — unresolved parking-lot ({n_unresolved} unresolved + {m_resolved} resolved)\n\n\
         These items were deferred / undecided / blocked on THIS project and are still UNRESOLVED. \
         Re-evaluate each one if the current task can settle it; never silently drop one — resolve it \
         in place in `{REGISTER_REL_PATH}` when the trigger fires:\n"
    );

    let mut list = String::new();
    for (i, d) in open.iter().enumerate() {
        if i >= MAX_RECALLED_ITEMS {
            break;
        }
        let line = render_recall_line(d);
        if header.chars().count() + list.chars().count() + line.chars().count() > budget_chars {
            break;
        }
        list.push_str(&line);
    }
    if list.is_empty() {
        // Budget too tight for a whole line — surface the first item, truncated,
        // so the block is never just a header with no recall.
        if let Some(d) = open.first() {
            let line_budget = budget_chars.saturating_sub(header.chars().count()).max(1);
            list = crate::experts::excerpt(&render_recall_line(d), line_budget);
        }
    }

    crate::experts::excerpt(&format!("{header}{list}"), budget_chars)
}

/// Append ONE new entry to the register (append-only), creating the file with a
/// project-doc header on first write. Returns `true` when a (non-empty) entry
/// was written.
///
/// Fail-open: an entry with no title AND no open-item is a no-op (`false`); a
/// write error is swallowed — recording an open decision must never block the
/// team. Append-only by contract: existing content is NEVER rewritten, so the
/// decision trail is preserved.
pub fn append_decision(root: &Path, entry: &NewDecision) -> bool {
    if entry.title.trim().is_empty() && entry.open_item.trim().is_empty() {
        return false;
    }
    // Serialize appends so two concurrent callers can't interleave a half-entry.
    static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let path = register_path(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let existed = path.exists();
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return false;
    };
    let mut body = String::new();
    if !existed {
        body.push_str(REGISTER_HEADER);
    }
    body.push_str(&render_entry(entry));
    file.write_all(body.as_bytes()).is_ok()
}

/// The header written once when the register is first created — a short,
/// human-facing preamble so the committed doc reads as a real project artifact.
const REGISTER_HEADER: &str = "# Open Decisions Register\n\n\
     Append-only, resolved-in-place log of items left undecided / deferred / blocked / parked \
     pending a future trigger. An item is closed by flipping its `## OPEN` heading to `## RESOLVED` \
     and adding a `- **Resolution**:` line — never by deleting it.\n";

/// Render one [`NewDecision`] as a Markdown entry in the canonical shape the
/// directive documents. Leads with a blank line so appends never glue onto the
/// previous entry.
fn render_entry(entry: &NewDecision) -> String {
    let cat = if entry.category.trim().is_empty() {
        "design-decision-to-evaluate"
    } else {
        entry.category.trim()
    };
    let title = if entry.title.trim().is_empty() {
        entry.open_item.trim()
    } else {
        entry.title.trim()
    };
    format!(
        "\n## OPEN — {cat} — {title}\n\
         - **Date**: {}\n\
         - **Source**: {}\n\
         - **Open item**: {}\n\
         - **Related constraints**: {}\n\
         - **Current leaning**: {}\n\
         - **Blocked by**: {}\n\
         - **Resolves when**: {}\n",
        blank_if_empty(&entry.date),
        blank_if_empty(&entry.source),
        blank_if_empty(&entry.open_item),
        blank_if_empty(&entry.related_constraints),
        blank_if_empty(&entry.current_leaning),
        blank_if_empty(&entry.blocked_by),
        blank_if_empty(&entry.resolves_when),
    )
}

/// A trimmed value, or `none yet` for an empty one (keeps the rendered fields
/// non-blank so a re-parse still finds them).
fn blank_if_empty(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        "none yet".to_string()
    } else {
        t.to_string()
    }
}

// ── parsing ──────────────────────────────────────────────────────────────────

/// Parse the register text into entries, oldest FIRST. Forgiving: anything
/// before the first `## ` entry heading (the doc title + preamble) is ignored;
/// a heading with no usable content is dropped. Bounded at [`MAX_ENTRIES`].
fn parse_register(text: &str) -> Vec<OpenDecision> {
    let mut entries: Vec<OpenDecision> = Vec::new();
    let mut heading: Option<String> = None;
    let mut body = String::new();
    for line in text.lines() {
        if is_entry_heading(line) {
            if let Some(h) = heading.take() {
                if let Some(d) = build_decision(&h, &body) {
                    entries.push(d);
                }
            }
            heading = Some(line.to_string());
            body.clear();
        } else if heading.is_some() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(h) = heading.take() {
        if let Some(d) = build_decision(&h, &body) {
            entries.push(d);
        }
    }
    let len = entries.len();
    if len > MAX_ENTRIES {
        entries.drain(0..len - MAX_ENTRIES);
    }
    entries
}

/// Whether `line` is a level-2 (`## `) entry heading — the entry delimiter. A
/// level-1 `# ` (doc title) or level-3+ `### ` (a sub-heading) is NOT an entry.
fn is_entry_heading(line: &str) -> bool {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|&c| c == '#').count();
    hashes == 2 && t.chars().nth(2) == Some(' ')
}

/// Build one [`OpenDecision`] from its heading line + body. Returns `None` when
/// there's no usable title (pure junk), so a malformed section is dropped.
fn build_decision(heading: &str, body: &str) -> Option<OpenDecision> {
    let head_text = heading.trim_start().trim_start_matches('#').trim();
    let status = parse_status(head_text);
    let category = parse_category(head_text);
    let mut title = derive_title(head_text, category);
    if title.is_empty() {
        title = head_text.to_string();
    }
    if title.is_empty() {
        return None;
    }
    Some(OpenDecision {
        status,
        category: category.map(str::to_string),
        title: crate::experts::excerpt(&title, MAX_TITLE_CHARS),
        open_item: field(
            body,
            &["open item", "item", "decision", "question", "topic"],
        ),
        resolves_when: field(
            body,
            &[
                "resolves when",
                "resolved when",
                "resolve when",
                "unblocks when",
                "trigger",
                "condition",
            ],
        ),
        blocked_by: field(
            body,
            &[
                "blocked by",
                "blocker",
                "waiting on",
                "depends on",
                "blocks",
            ],
        ),
    })
}

/// Status from the heading's FIRST token. `RESOLVED`/`DONE`/`CLOSED`/`ANSWERED`
/// → resolved; anything else (incl. explicit `OPEN` and a missing status) →
/// open (default-open is the safe bias: surface, don't hide).
fn parse_status(head_text: &str) -> DecisionStatus {
    let first = head_text
        .split(|c: char| c.is_whitespace() || "—–-·|:".contains(c))
        .find(|s| !s.is_empty())
        .unwrap_or("");
    match first.to_ascii_uppercase().as_str() {
        "RESOLVED" | "DONE" | "CLOSED" | "ANSWERED" => DecisionStatus::Resolved,
        _ => DecisionStatus::Open,
    }
}

/// Canonical category slug when the heading contains a known one (checked in
/// [`CATEGORIES`] order for determinism), else `None`.
fn parse_category(head_text: &str) -> Option<&'static str> {
    let lower = head_text.to_lowercase();
    CATEGORIES.into_iter().find(|slug| lower.contains(slug))
}

/// Derive the title: the heading with a leading status keyword + the category
/// slug removed, trimmed of separator punctuation. ASCII-safe: the byte-offset
/// strips only touch ASCII regions (status keywords + the ASCII category slug).
fn derive_title(head_text: &str, category: Option<&str>) -> String {
    let mut t = head_text.trim().to_string();
    // Strip a leading status keyword (ASCII), if present. `get` is char-boundary
    // safe: a keyword length landing inside a multibyte char (e.g. the em-dash
    // separator) yields `None` rather than panicking.
    for kw in ["RESOLVED", "CLOSED", "DONE", "ANSWERED", "OPEN"] {
        if t.get(..kw.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(kw))
        {
            t.replace_range(0..kw.len(), "");
            break;
        }
    }
    // Strip the category slug (ASCII) wherever it appears — only when the
    // lowercased copy has the same byte length (true for ASCII + CJK), so the
    // found offset is valid in the original.
    if let Some(c) = category {
        let lower = t.to_lowercase();
        if lower.len() == t.len() {
            if let Some(pos) = lower.find(c) {
                t.replace_range(pos..pos + c.len(), "");
            }
        }
    }
    t.trim_matches(|ch: char| ch.is_whitespace() || "—–-·|:*#[]()".contains(ch))
        .to_string()
}

/// Extract the first body field whose (markdown-normalised) line starts with any
/// of `names` followed by `:`. Returns the trimmed, bounded value; `None` when
/// no line matches.
fn field(body: &str, names: &[&str]) -> Option<String> {
    for raw in body.lines() {
        let norm = raw
            .trim()
            .trim_start_matches(['-', '*', ' '])
            .replace("**", "")
            .replace("__", "")
            .replace('`', "");
        let Some((key, value)) = norm.split_once(':') else {
            continue;
        };
        let key_l = key.trim().to_lowercase();
        if names.iter().any(|n| key_l == *n || key_l.starts_with(n)) {
            let v = value.trim();
            if !v.is_empty() {
                return Some(crate::experts::excerpt(v, MAX_FIELD_CHARS));
            }
        }
    }
    None
}

/// Render one unresolved entry as a compact recall bullet, bounded to
/// [`MAX_RECALL_LINE_CHARS`]: `- [category] title — <open item> · resolves when: …`.
fn render_recall_line(d: &OpenDecision) -> String {
    let cat = d.category.as_deref().unwrap_or("uncategorized");
    let mut line = format!("- [{cat}] {}", d.title);
    if let Some(oi) = &d.open_item {
        if !oi.is_empty() {
            line.push_str(" — ");
            line.push_str(oi);
        }
    }
    if let Some(rw) = &d.resolves_when {
        if !rw.is_empty() {
            line.push_str(" · resolves when: ");
            line.push_str(rw);
        }
    }
    let mut out = crate::experts::excerpt(&line, MAX_RECALL_LINE_CHARS);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A register with two open items (both known categories) + one resolved
    /// item — the shape the user's proven manual run produced.
    fn seed_register(root: &Path) {
        let path = register_path(root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "# Open Decisions Register\n\n\
             Some preamble prose that is not an entry.\n\n\
             ## OPEN — waiting-on-external-condition — Stripe live key not provisioned\n\
             - **Date**: 2026-07-01\n\
             - **Source**: checkout task\n\
             - **Open item**: cannot wire live payments without the STRIPE_LIVE_KEY\n\
             - **Blocked by**: ops must provision the key\n\
             - **Resolves when**: the STRIPE_LIVE_KEY env var is available\n\n\
             ## OPEN — design-decision-to-evaluate — Session store: cookie vs Redis\n\
             - **Date**: 2026-07-01\n\
             - **Open item**: pick the session backend\n\
             - **Current leaning**: cookie for MVP\n\
             - **Resolves when**: concurrent-user load is known\n\n\
             ## RESOLVED — existing-design-boundary — Single-region deploy accepted\n\
             - **Date**: 2026-06-30\n\
             - **Open item**: multi-region was out of scope\n\
             - **Resolution**: single region for v1 (2026-07-01)\n",
        )
        .unwrap();
    }

    #[test]
    fn parses_open_and_resolved_with_categories_and_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        seed_register(tmp.path());
        let all = load_decisions(tmp.path());
        assert_eq!(all.len(), 3, "three entries parsed: {all:?}");
        // Oldest first, statuses + categories parsed.
        assert_eq!(all[0].status, DecisionStatus::Open);
        assert_eq!(
            all[0].category.as_deref(),
            Some("waiting-on-external-condition")
        );
        assert!(
            all[0].title.contains("Stripe live key"),
            "title stripped of status+category: {}",
            all[0].title
        );
        assert!(all[0]
            .open_item
            .as_deref()
            .unwrap()
            .contains("STRIPE_LIVE_KEY"));
        assert!(all[0]
            .resolves_when
            .as_deref()
            .unwrap()
            .contains("STRIPE_LIVE_KEY"));
        assert_eq!(all[2].status, DecisionStatus::Resolved);
        assert_eq!(all[2].category.as_deref(), Some("existing-design-boundary"));
    }

    #[test]
    fn counts_split_unresolved_and_resolved() {
        let tmp = tempfile::TempDir::new().unwrap();
        seed_register(tmp.path());
        assert_eq!(counts(tmp.path()), (2, 1));
        assert_eq!(unresolved(tmp.path()).len(), 2);
    }

    #[test]
    fn load_is_fail_open_on_a_missing_register() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(load_decisions(tmp.path()).is_empty());
        assert_eq!(counts(tmp.path()), (0, 0));
        let missing = Path::new("/nonexistent/umadev/open-decisions/root/xyz");
        assert!(load_decisions(missing).is_empty());
    }

    #[test]
    fn load_is_forgiving_of_malformed_content() {
        // Garbage with no entry headings → zero entries, never a panic.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = register_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "not markdown at all\n<<<garbage>>>\n### only a subheading\n",
        )
        .unwrap();
        assert!(load_decisions(tmp.path()).is_empty());
        assert!(decisions_recall_block(tmp.path(), DECISIONS_FIRMWARE_BUDGET).is_empty());
    }

    #[test]
    fn missing_status_defaults_to_open() {
        // A heading with no status token is treated as OPEN (surface, don't hide).
        let tmp = tempfile::TempDir::new().unwrap();
        let path = register_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "## design-decision-to-evaluate — Which ORM?\n- **Open item**: pick an ORM\n",
        )
        .unwrap();
        let all = load_decisions(tmp.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, DecisionStatus::Open);
        assert!(
            all[0].title.contains("Which ORM"),
            "title: {}",
            all[0].title
        );
    }

    #[test]
    fn recall_block_lists_unresolved_with_the_summary() {
        let tmp = tempfile::TempDir::new().unwrap();
        seed_register(tmp.path());
        let block = decisions_recall_block(tmp.path(), DECISIONS_FIRMWARE_BUDGET);
        assert!(block.contains("OPEN DECISIONS"), "labelled: {block}");
        // The (N unresolved + M resolved) summary is present.
        assert!(
            block.contains("2 unresolved + 1 resolved"),
            "summary present: {block}"
        );
        // Both UNRESOLVED items are recalled…
        assert!(block.contains("Stripe live key"), "recalls item 1: {block}");
        assert!(block.contains("cookie vs Redis"), "recalls item 2: {block}");
        // …and the RESOLVED item is NOT recalled.
        assert!(
            !block.contains("Single-region deploy"),
            "resolved item not recalled: {block}"
        );
    }

    #[test]
    fn recall_block_is_empty_without_unresolved_items() {
        // A register with only resolved items → no recall (fail-open shape).
        let tmp = tempfile::TempDir::new().unwrap();
        let path = register_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "## RESOLVED — existing-design-boundary — done thing\n- **Open item**: x\n",
        )
        .unwrap();
        assert!(decisions_recall_block(tmp.path(), DECISIONS_FIRMWARE_BUDGET).is_empty());
    }

    #[test]
    fn recall_block_is_bounded_under_many_entries() {
        // A register grown to dozens of large open items must keep the block within
        // BOTH the char budget and the item cap.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = register_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut doc = String::from("# Open Decisions Register\n\n");
        for i in 0..80 {
            doc.push_str(&format!(
                "## OPEN — design-decision-to-evaluate — decision {i} {}\n- **Open item**: {}\n\n",
                "t".repeat(60),
                "x".repeat(300),
            ));
        }
        std::fs::write(&path, &doc).unwrap();
        let block = decisions_recall_block(tmp.path(), DECISIONS_FIRMWARE_BUDGET);
        assert!(
            block.chars().count() <= DECISIONS_FIRMWARE_BUDGET,
            "block within budget ({} > {DECISIONS_FIRMWARE_BUDGET})",
            block.chars().count()
        );
        // The summary still counts ALL 80 unresolved even though only a few list.
        assert!(
            block.contains("80 unresolved"),
            "summary counts all: {block}"
        );
        let listed = block.matches("- [design-decision-to-evaluate]").count();
        assert!(
            listed <= MAX_RECALLED_ITEMS,
            "item cap honoured: {listed} listed"
        );
    }

    #[test]
    fn directive_documents_categories_and_fields() {
        let d = decisions_directive();
        // The record-to-register location + discipline.
        assert!(d.contains(REGISTER_REL_PATH), "names the register path");
        assert!(
            d.contains("append-only") || d.to_lowercase().contains("append-only"),
            "append-only discipline"
        );
        assert!(
            d.to_uppercase().contains("RESOLVED-IN-PLACE")
                || d.to_lowercase().contains("resolved-in-place"),
            "resolved-in-place discipline"
        );
        // All three categories.
        for c in CATEGORIES {
            assert!(d.contains(c), "documents category {c}");
        }
        // The structured fields.
        for f in [
            "**Date**",
            "**Source**",
            "**Open item**",
            "**Related constraints**",
            "**Current leaning**",
            "**Blocked by**",
            "**Resolves when**",
        ] {
            assert!(d.contains(f), "documents field {f}");
        }
        // The status headings.
        assert!(d.contains("## OPEN"));
        assert!(d.contains("## RESOLVED"));
    }

    #[test]
    fn append_round_trips_through_the_parser() {
        // The Rust append path writes the exact shape the directive documents, and
        // the parser reads it back — locking the on-disk contract.
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(append_decision(
            tmp.path(),
            &NewDecision {
                category: "waiting-on-external-condition".to_string(),
                title: "OAuth client id pending".to_string(),
                date: "2026-07-02".to_string(),
                source: "auth task".to_string(),
                open_item: "need the Google OAuth client id".to_string(),
                blocked_by: "user must create the OAuth app".to_string(),
                resolves_when: "the client id + secret are provided".to_string(),
                ..Default::default()
            },
        ));
        let all = load_decisions(tmp.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, DecisionStatus::Open);
        assert_eq!(
            all[0].category.as_deref(),
            Some("waiting-on-external-condition")
        );
        assert!(all[0].title.contains("OAuth client id pending"));
        assert!(all[0].open_item.as_deref().unwrap().contains("client id"));
        // The committed file exists at the documented, project-visible path.
        assert!(tmp.path().join(REGISTER_REL_PATH).exists());
    }

    #[test]
    fn append_is_append_only_preserving_the_trail() {
        // A second append must NOT rewrite the first entry (the trail survives).
        let tmp = tempfile::TempDir::new().unwrap();
        append_decision(
            tmp.path(),
            &NewDecision {
                title: "first".to_string(),
                open_item: "one".to_string(),
                ..Default::default()
            },
        );
        append_decision(
            tmp.path(),
            &NewDecision {
                title: "second".to_string(),
                open_item: "two".to_string(),
                ..Default::default()
            },
        );
        let text = std::fs::read_to_string(tmp.path().join(REGISTER_REL_PATH)).unwrap();
        assert!(text.contains("first"), "first entry preserved");
        assert!(text.contains("second"), "second entry appended");
        assert_eq!(load_decisions(tmp.path()).len(), 2);
        // The header is written exactly once.
        assert_eq!(text.matches("# Open Decisions Register").count(), 1);
    }

    #[test]
    fn empty_append_is_a_no_op() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(!append_decision(tmp.path(), &NewDecision::default()));
        assert!(load_decisions(tmp.path()).is_empty());
    }

    #[test]
    fn recall_never_panics_on_a_tiny_budget() {
        let tmp = tempfile::TempDir::new().unwrap();
        seed_register(tmp.path());
        // Pathologically small budgets must still produce a bounded, non-panicking
        // block.
        for b in [0usize, 1, 5, 40] {
            let block = decisions_recall_block(tmp.path(), b);
            assert!(
                block.chars().count() <= b.max(1) + 8,
                "bounded at budget {b}"
            );
        }
    }
}
