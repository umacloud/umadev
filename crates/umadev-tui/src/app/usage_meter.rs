use umadev_runtime::Usage;

/// Quality-preserving live usage state for one TUI conversation.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionUsageMeter {
    tokens: u64,
    /// The session-cumulative deterministic `chars/4` estimate. Kept STRICTLY
    /// separate from the real `tokens` — it is NEVER summed with them and is
    /// surfaced (via [`SessionUsageMeter::est_tokens`]) ONLY as a clearly-LABELED
    /// lower-bound fallback when the base has reported no real usage all session.
    est_tokens: u64,
    seen: bool,
    incomplete: bool,
    cost_usd_ticks: Option<i64>,
    exact_context_input: Option<u64>,
}

impl SessionUsageMeter {
    pub(crate) fn apply(&mut self, usage: Option<Usage>) {
        let had_prior_report = self.seen;
        self.seen = true;
        let Some(usage) = usage else {
            self.incomplete = true;
            self.cost_usd_ticks = None;
            self.exact_context_input = None;
            return;
        };

        self.tokens = self.tokens.saturating_add(usage.total_tokens);
        self.incomplete |= usage.usage_incomplete;
        let current_cost = usage.trusted_cost_usd_ticks();
        self.cost_usd_ticks = if had_prior_report {
            self.cost_usd_ticks
                .zip(current_cost)
                .and_then(|(left, right)| left.checked_add(right))
        } else {
            current_cost
        };
        self.exact_context_input = (!usage.usage_incomplete).then_some(usage.input_tokens);
    }

    /// Accumulate this turn's deterministic `chars/4` estimate (prompt + reply),
    /// tracked in a sibling field so it can back a clearly-LABELED lower bound when
    /// the base reports no real usage. It is NEVER folded into the real [`tokens`]
    /// total — the two stay strictly separate, and the estimate is surfaced only
    /// while `tokens == 0` (see [`SessionUsageMeter::est_tokens`] and the gauge).
    /// Fail-open: a zero estimate is a harmless no-op.
    ///
    /// [`tokens`]: SessionUsageMeter::tokens
    pub(crate) fn observe_estimate(&mut self, est_tokens: u64) {
        self.est_tokens = self.est_tokens.saturating_add(est_tokens);
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) const fn tokens(&self) -> u64 {
        self.tokens
    }

    /// The session-cumulative `chars/4` estimate — a deliberately-labeled lower
    /// bound the gauge shows ONLY when the base reported no real usage
    /// (`is_incomplete() && tokens() == 0`), never as the base's own count.
    pub(crate) const fn est_tokens(&self) -> u64 {
        self.est_tokens
    }

    pub(crate) const fn has_report(&self) -> bool {
        self.seen
    }

    pub(crate) const fn is_incomplete(&self) -> bool {
        self.incomplete
    }

    pub(crate) const fn exact_cost_usd_ticks(&self) -> Option<i64> {
        self.cost_usd_ticks
    }

    pub(crate) const fn exact_context_input(&self) -> Option<u64> {
        self.exact_context_input
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_and_missing_reports_never_become_exact_or_free() {
        let mut meter = SessionUsageMeter::default();
        let incomplete = Usage {
            usage_incomplete: true,
            cost_usd_ticks: Some(99),
            ..Usage::exact(12, 3)
        };
        meter.apply(Some(incomplete));
        assert_eq!(meter.tokens(), 15);
        assert!(meter.is_incomplete());
        assert_eq!(meter.exact_cost_usd_ticks(), None);
        assert_eq!(meter.exact_context_input(), None);

        meter.apply(None);
        assert_eq!(meter.tokens(), 15);
        assert!(meter.is_incomplete());
        assert_eq!(meter.exact_cost_usd_ticks(), None);

        meter.reset();
        meter.apply(Some(Usage::default()));
        assert!(meter.has_report());
        assert_eq!(meter.tokens(), 0);
        assert!(meter.is_incomplete());
        assert_eq!(meter.exact_cost_usd_ticks(), None);
    }

    #[test]
    fn exact_cost_accumulates_only_while_every_turn_has_one() {
        let mut meter = SessionUsageMeter::default();
        meter.apply(Some(Usage {
            cost_usd_ticks: Some(10),
            ..Usage::exact(3, 2)
        }));
        meter.apply(Some(Usage {
            cost_usd_ticks: Some(20),
            ..Usage::exact(4, 1)
        }));
        assert_eq!(meter.tokens(), 10);
        assert_eq!(meter.exact_cost_usd_ticks(), Some(30));
        meter.apply(Some(Usage::exact(1, 1)));
        assert_eq!(meter.exact_cost_usd_ticks(), None);
    }

    #[test]
    fn estimate_accumulates_separately_and_never_merges_with_real_tokens() {
        let mut meter = SessionUsageMeter::default();
        // Two relay turns report no real usage but carry an estimate each.
        meter.apply(None);
        meter.observe_estimate(120);
        meter.apply(None);
        meter.observe_estimate(80);
        assert_eq!(meter.tokens(), 0, "no real usage was ever reported");
        assert_eq!(
            meter.est_tokens(),
            200,
            "the estimate accumulates across turns in its own field"
        );
        assert!(meter.is_incomplete());

        // A real report lands: it drives the REAL total and does NOT absorb the
        // estimate. The estimate keeps its own tally; the gauge simply stops
        // showing it once there is a real number (tokens > 0).
        meter.apply(Some(Usage::exact(300, 200)));
        meter.observe_estimate(50);
        assert_eq!(
            meter.tokens(),
            500,
            "the real count is the base's own number, never the estimate"
        );
        assert_eq!(
            meter.est_tokens(),
            250,
            "the estimate stays strictly separate — never summed into the real total"
        );
    }

    #[test]
    fn reset_clears_the_estimate_too() {
        let mut meter = SessionUsageMeter::default();
        meter.observe_estimate(999);
        assert_eq!(meter.est_tokens(), 999);
        meter.reset();
        assert_eq!(meter.est_tokens(), 0, "/clear starts a fresh estimate");
        assert_eq!(meter.tokens(), 0);
    }
}
