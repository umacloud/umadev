//! `/checkpoint` and `/rewind`: file checkpoints over the shadow git store.

use super::{Action, App, ChatRole};

impl App {
    /// `/checkpoint [label]` — snapshot the workspace FILES so a whole phase's
    /// work can be rewound later (shadow git, never touches the user's `.git`).
    /// A snapshot that cannot be taken says why (git missing, the store under
    /// `.umadev/` unusable, the workspace over a whole-tree limit).
    pub(super) fn slash_checkpoint(&mut self, label: &str) -> Action {
        let label = if label.trim().is_empty() {
            umadev_i18n::t(self.lang, "checkpoint.manual_label").to_string()
        } else {
            label.trim().to_string()
        };
        match umadev_agent::checkpoint::try_create_checkpoint(&self.project_root, &label) {
            Ok(id) => self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "checkpoint.created", &[&id, &label, &id]),
            ),
            Err(reason) => self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "checkpoint.unavailable", &[&reason]),
            ),
        }
        Action::None
    }

    /// `/rewind` lists file checkpoints; `/rewind <id>` rewinds the workspace
    /// files to that checkpoint (the present is auto-checkpointed first, so the
    /// rewind is itself undoable).
    pub(super) fn slash_rewind(&mut self, arg: &str) -> Action {
        // A2#11: the same busy-guard as `/redo` — a rewind while a run is writing
        // the workspace is a second writer racing the first (the restore and the
        // base's edits interleave). Politely refuse; `/cancel` first. Uses
        // `has_active_run` so the director/agentic build counts too (a legacy
        // `is_pipeline_active` check would miss it). Listing (`/rewind` with no
        // id) stays allowed below — it is read-only.
        if !arg.trim().is_empty() && self.has_active_run() {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "rewind.busy"));
            return Action::None;
        }
        let arg = arg.trim();
        if arg.is_empty() {
            let list = umadev_agent::checkpoint::list_checkpoints(&self.project_root);
            if list.is_empty() {
                // An empty list after a failed snapshot names that failure
                // rather than implying no snapshot was ever attempted.
                let failure = umadev_agent::checkpoint::last_checkpoint_failure(&self.project_root);
                let empty = failure.map_or_else(
                    || umadev_i18n::t(self.lang, "rewind.empty").to_string(),
                    |reason| umadev_i18n::tf(self.lang, "rewind.empty_unavailable", &[&reason]),
                );
                self.push(ChatRole::System, empty);
                return Action::None;
            }
            let mut out = umadev_i18n::t(self.lang, "rewind.list_header").to_string();
            for c in list.iter().take(20) {
                let when = c.when.split('T').next().unwrap_or(&c.when);
                out.push_str(&format!("  {}  {}  {}\n", c.id, when, c.label));
            }
            self.push(ChatRole::System, out);
            return Action::None;
        }
        match umadev_agent::checkpoint::restore_checkpoint(&self.project_root, arg) {
            Ok(()) => self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "rewind.restored", &[arg]),
            ),
            Err(e) => self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "rewind.failed", &[&e]),
            ),
        }
        Action::None
    }
}
