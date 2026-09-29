//! One-launch notices about the user's config file.

use super::{App, AppMode, PickerStep};

impl App {
    /// Surface the one-time retired-backend migration and take the user directly
    /// to the five-base picker. The notice exists only for this process launch;
    /// the migration version persisted by `config` prevents it recurring.
    pub(crate) fn show_retired_backend_migration(&mut self, retired_backend: Option<&str>) {
        let Some(retired_backend) = retired_backend else {
            return;
        };
        self.mode = AppMode::Picker;
        self.goto_picker_step(PickerStep::BaseCli);
        self.picker_notice = Some(umadev_i18n::tf(
            self.lang,
            "backend.migration.retired",
            &[retired_backend],
        ));
        self.refresh_status();
    }

    /// Startup could not read the user's `config.toml`, so this session runs
    /// on defaults and the first-run picker may reopen. Say so on the picker
    /// and in the transcript; the file itself was left untouched, and a later
    /// save keeps a copy of it first (see [`crate::config::save_to`]).
    pub(crate) fn show_unreadable_config(&mut self, error: Option<&str>) {
        let Some(error) = error else {
            return;
        };
        let note = umadev_i18n::tf(
            self.lang,
            "config.unreadable",
            &[&self.config_path.display().to_string(), error],
        );
        self.picker_notice = Some(note.clone());
        self.push_workspace_notice(note);
        self.refresh_status();
    }
}
