use super::{plan_step_glyph, seat_display_name, Action, App, ChatRole};

fn queued_text_preview(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = compact.chars();
    let preview = chars.by_ref().take(120).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

impl App {
    pub(super) fn show_plan_status(&mut self) {
        self.align_queued_dispatch_kinds();
        let has_plan = !self.plan_steps.is_empty();
        let has_base_plan = !self.base_session_plan.is_empty();
        let has_review = !self.critic_verdicts.is_empty();
        let has_queue = self.queued_count() > 0;
        if !has_plan && !has_base_plan && !has_review && !has_queue {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "plan.none"));
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "plan.steer.usage"),
            );
            return;
        }

        let mut body = String::new();
        if has_plan {
            let done = self
                .plan_steps
                .iter()
                .filter(|step| step.status == "done")
                .count();
            body.push_str(&format!(
                "{} {done}/{}\n",
                umadev_i18n::t(self.lang, "plan.panel.title"),
                self.plan_steps.len()
            ));
            for step in &self.plan_steps {
                body.push_str(&format!(
                    "  {} {} · {}\n",
                    plan_step_glyph(&step.status),
                    step.id,
                    step.title
                ));
            }

            // The compact live panel can clip the roster on short terminals. `/plan`
            // is the scrollable, lossless view promised by that panel, so include
            // every convened teammate here as well as every plan step.
            let roster = self.convened_roster();
            if !roster.is_empty() {
                body.push('\n');
                body.push_str(umadev_i18n::t(self.lang, "team.roster.panel.title"));
                body.push('\n');
                for seat in &roster {
                    let status = umadev_i18n::t(self.lang, seat.status.label_key());
                    let verdict = seat
                        .verdict
                        .map(|(accepts, count)| {
                            if accepts {
                                format!(" · {}", umadev_i18n::t(self.lang, "plan.review.accept"))
                            } else {
                                format!(
                                    " · {}",
                                    umadev_i18n::tf(
                                        self.lang,
                                        "plan.review.block",
                                        &[&count.max(1).to_string()],
                                    )
                                )
                            }
                        })
                        .unwrap_or_default();
                    body.push_str(&format!(
                        "  {} · {status}{verdict}\n",
                        seat_display_name(self.lang, &seat.role)
                    ));
                }
            }
        }

        if has_base_plan {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(umadev_i18n::t(self.lang, "base.plan.panel.title"));
            body.push('\n');
            for entry in &self.base_session_plan {
                let status = match entry.status {
                    umadev_runtime::SessionPlanEntryStatus::Pending => "pending",
                    umadev_runtime::SessionPlanEntryStatus::InProgress => "active",
                    umadev_runtime::SessionPlanEntryStatus::Completed => "done",
                };
                body.push_str(&format!(
                    "  {} {}\n",
                    plan_step_glyph(status),
                    entry.content
                ));
            }
        }

        if has_review {
            if !body.is_empty() {
                body.push('\n');
            }
            let accepts = self
                .critic_verdicts
                .iter()
                .filter(|critic| critic.accepts)
                .count();
            let blocking = self.critic_verdicts.len() - accepts;
            body.push_str(&umadev_i18n::tf(
                self.lang,
                "plan.review.section",
                &[&accepts.to_string(), &blocking.to_string()],
            ));
            body.push('\n');
            for critic in &self.critic_verdicts {
                let verdict = if critic.accepts {
                    umadev_i18n::t(self.lang, "plan.review.accept").to_string()
                } else {
                    umadev_i18n::tf(
                        self.lang,
                        "plan.review.block",
                        &[&critic.blocking.len().max(1).to_string()],
                    )
                };
                body.push_str(&format!("  [{}] {verdict}\n", critic.seat));
                if !critic.accepts {
                    for (index, finding) in critic.blocking.iter().enumerate() {
                        if !finding.trim().is_empty() {
                            body.push_str(&format!("    - {}\n", finding.trim()));
                            if let Some(fix) = critic.fix_for(index) {
                                body.push_str(&format!(
                                    "      {}\n",
                                    umadev_i18n::tf(self.lang, "plan.review.fix", &[fix])
                                ));
                            }
                        }
                    }
                }
                for advisory in &critic.advisory {
                    if !advisory.trim().is_empty() {
                        body.push_str(&format!(
                            "    ~ {}\n",
                            umadev_i18n::tf(self.lang, "plan.review.advisory", &[advisory.trim()],)
                        ));
                    }
                }
            }
        }

        if has_queue {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&umadev_i18n::tf(
                self.lang,
                "plan.queue.section",
                &[&self.queued_count().to_string()],
            ));
            body.push('\n');
            let mut position = 1usize;
            for text in &self.queued_chat {
                body.push_str(&umadev_i18n::tf(
                    self.lang,
                    "plan.queue.next_turn",
                    &[&position.to_string(), &queued_text_preview(text)],
                ));
                body.push('\n');
                position += 1;
            }
            for text in &self.queued_steer {
                body.push_str(&umadev_i18n::tf(
                    self.lang,
                    "plan.queue.current_task",
                    &[&position.to_string(), &queued_text_preview(text)],
                ));
                body.push('\n');
                position += 1;
            }
            for entry in self.prompt_queue.entries() {
                body.push_str(&umadev_i18n::tf(
                    self.lang,
                    "plan.queue.native",
                    &[&position.to_string(), &queued_text_preview(&entry.text)],
                ));
                body.push('\n');
                position += 1;
            }
        }

        body.push_str(umadev_i18n::t(self.lang, "plan.steer.usage"));
        self.push(ChatRole::UmaDev, body);
    }

    /// Fold a plan edit into the next same-session directive. Non-`add` forms
    /// validate a live step id; every form requires a non-empty payload.
    pub(super) fn steer_plan(&mut self, sub: &str, target: &str) -> Action {
        if target.is_empty() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "plan.steer.usage"),
            );
            return Action::None;
        }
        if sub != "add" && !self.plan_steps.iter().any(|step| step.id == target) {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "plan.steer.unknown_step", &[target]),
            );
            return Action::None;
        }

        let (directive, confirmation) = match sub {
            "skip" => (
                format!(
                    "Plan steering: SKIP step `{target}` — do not perform it; proceed with the rest of the plan."
                ),
                umadev_i18n::tf(self.lang, "plan.steer.skip", &[target]),
            ),
            "veto" => (
                format!(
                    "Plan steering: VETO step `{target}` — remove it from the plan entirely and do not perform it."
                ),
                umadev_i18n::tf(self.lang, "plan.steer.veto", &[target]),
            ),
            "up" => (
                format!(
                    "Plan steering: REORDER step `{target}` EARLIER — do it before its current predecessors where dependencies allow."
                ),
                umadev_i18n::tf(self.lang, "plan.steer.move", &[target, "↑"]),
            ),
            "down" => (
                format!(
                    "Plan steering: REORDER step `{target}` LATER — defer it after its current successors where dependencies allow."
                ),
                umadev_i18n::tf(self.lang, "plan.steer.move", &[target, "↓"]),
            ),
            _ => (
                format!("Plan steering: ADD a new step — {target}"),
                umadev_i18n::tf(self.lang, "plan.steer.add", &[target]),
            ),
        };
        self.queued_steer.push_back(directive);
        self.push(ChatRole::UmaDev, confirmation);
        if !self.is_pipeline_active() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "plan.steer.queued"),
            );
        }
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::queued_text_preview;

    #[test]
    fn queued_preview_is_single_line_and_bounded() {
        let text = format!("  one\n\ttwo {}", "界".repeat(130));
        let preview = queued_text_preview(&text);
        assert!(!preview.contains('\n'));
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), 121);
    }
}
