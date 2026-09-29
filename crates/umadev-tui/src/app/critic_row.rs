//! One team-review seat row ([`CriticRow`]): its verdict label, its transcript
//! note, and the accept / blocking tally of a review round. A seat that produced
//! no verdict (transport, parse or timeout failure) is an operational fact, not a
//! finding: it renders neutrally and is never counted as a must-fix.

use super::{App, CriticRow};

impl CriticRow {
    /// The suggested one-line fix for the blocking finding at `idx`, if the seat
    /// emitted one (`remediation` is index-aligned with `blocking`). `None` when no
    /// matching, non-blank suggestion exists — the caller then shows the blocker
    /// alone, never a fabricated fix (fail-open).
    #[must_use]
    pub fn fix_for(&self, idx: usize) -> Option<&str> {
        self.remediation
            .get(idx)
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// Whether the seat could not produce a verdict at all.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        self.unavailable.is_some()
    }

    /// Whether the seat raised must-fix findings. An unavailable seat never does.
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        !self.accepts && !self.is_unavailable()
    }

    /// The localized verdict label of a panel / roster row.
    #[must_use]
    pub fn verdict_label(&self, lang: umadev_i18n::Lang) -> String {
        if self.is_unavailable() {
            umadev_i18n::t(lang, "plan.review.unavailable").to_string()
        } else if self.accepts {
            umadev_i18n::t(lang, "plan.review.accept").to_string()
        } else {
            umadev_i18n::tf(
                lang,
                "plan.review.block",
                &[&self.blocking.len().max(1).to_string()],
            )
        }
    }

    /// The transcript note for this verdict — the unbounded, scrollable record
    /// that guarantees a blocking critic's full findings are never hidden behind
    /// the panel's "… +N" clip. An accept is one line; a block lists every
    /// must-fix finding (with its suggested fix) underneath; an unavailable seat
    /// says so with its operational reason. Localized.
    pub(super) fn transcript_note(&self, lang: umadev_i18n::Lang) -> String {
        if let Some(reason) = &self.unavailable {
            return umadev_i18n::tf(
                lang,
                "plan.review.note.unavailable",
                &[&self.seat, reason.trim()],
            );
        }
        let mut body = if self.accepts {
            umadev_i18n::tf(lang, "plan.review.note.accept", &[&self.seat])
        } else {
            umadev_i18n::tf(
                lang,
                "plan.review.note.block",
                &[&self.seat, &self.blocking.len().max(1).to_string()],
            )
        };
        for (i, b) in self.blocking.iter().enumerate() {
            let item = b.trim();
            if item.is_empty() {
                continue;
            }
            body.push_str(&format!("\n  - {item}"));
            // The seat's per-blocker "how to fix" (index-aligned) rides directly
            // under the problem so the transcript shows a concrete next-step, not
            // just what is wrong. Fail-open: no matching suggestion → nothing extra.
            if let Some(fix) = self.fix_for(i) {
                body.push_str(&format!(
                    "\n    {}",
                    umadev_i18n::tf(lang, "plan.review.fix", &[fix])
                ));
            }
        }
        body
    }
}

impl App {
    /// `(accepts, blocking)` over the current review round. An unavailable seat
    /// is in neither count: it produced no verdict, so it is no must-fix.
    #[must_use]
    pub fn review_tally(&self) -> (usize, usize) {
        let accepts = self
            .critic_verdicts
            .iter()
            .filter(|c| c.accepts && !c.is_unavailable())
            .count();
        let blocking = self
            .critic_verdicts
            .iter()
            .filter(|c| c.is_blocking())
            .count();
        (accepts, blocking)
    }
}
