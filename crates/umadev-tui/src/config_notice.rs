//! The one-launch notice about the user's config file.

use crate::app::App;
use crate::config::StartupNotice;

/// Tell the user what startup found in their `config.toml`. An unreadable
/// file leaves this session on defaults, so the first-run picker may reopen:
/// say so on the picker and in the transcript. The file itself was left
/// untouched, and a later save keeps a copy of it first (see
/// [`crate::config::save_to`]).
pub(crate) fn show(app: &mut App, notice: Option<StartupNotice>) {
    let error = match notice {
        None => return,
        Some(StartupNotice::RetiredBackend(id)) => {
            app.show_retired_backend_migration(Some(&id));
            return;
        }
        Some(StartupNotice::Unreadable(error)) => error,
    };
    let note = umadev_i18n::tf(
        app.lang,
        "config.unreadable",
        &[&app.config_path.display().to_string(), &one_line(&error)],
    );
    app.picker_notice = Some(note.clone());
    app.push_workspace_notice(note);
}

/// A TOML parse error spans several lines (the location, a code frame, the
/// message); the notice keeps the location and the message on one line.
fn one_line(error: &str) -> String {
    let mut lines = error.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default();
    match lines.next_back() {
        Some(last) => format!("{first}: {last}"),
        None => first.to_string(),
    }
}
