//! The persisted `/animations` preference in the installation state directory.

use super::MAX_UI_STATE_BYTES;

/// P5d: the initial animation state — `false` (static spinner) when stdout is not
/// a real terminal (CI / piped output) OR the user persisted `animations_enabled
/// = false`; `true` otherwise. Fail-open to `true` (animated, today's behaviour)
/// on any read error.
pub(super) fn animations_enabled_default() -> bool {
    use std::io::IsTerminal;
    // A non-interactive stdout (piped / redirected) never benefits from a spinner
    // and a strobing braille frame just spams the log — render static there.
    if !std::io::stdout().is_terminal() {
        return false;
    }
    // Honor a persisted `/animations off`. Absent / unreadable → animated.
    animation_settings_root(false)
        .as_ref()
        .and_then(read_animation_settings)
        .and_then(|v| {
            v.get("animations_enabled")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(true)
}

pub(super) fn animation_settings_root(create_state: bool) -> Option<umadev_state::fs::RootedDir> {
    umadev_state::privacy::state_root(create_state)
}

pub(super) fn read_animation_settings(
    settings: &umadev_state::fs::RootedDir,
) -> Option<serde_json::Value> {
    let bytes = settings
        .read_bounded(std::path::Path::new("settings.json"), MAX_UI_STATE_BYTES)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}
