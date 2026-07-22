//! Backend-derived picker and command-palette data.

use super::{Action, App, AuthMark, BackendInfo, ChatRole, PickerGroup, PickerItem, PickerStep};

/// Sentinel that prefixes the structured auth metadata a backend probe packs
/// into its human-readable detail field.
pub(crate) const PROBE_AUTH_SENTINEL: char = '\u{1}';

fn backend_label(lang: umadev_i18n::Lang, id: &str) -> String {
    let key = match id {
        "claude-code" => "backend.claude",
        "codex" => "backend.codex",
        "opencode" => "backend.opencode",
        "grok-build" => "backend.grok",
        "kimi-code" => "backend.kimi",
        // Fail-open for an impossible internal mismatch; the caller only iterates
        // the fixed five-id product list below.
        _ => return id.to_string(),
    };
    umadev_i18n::t(lang, key).to_string()
}

/// Build the options for one first-run picker step. Live probe data supplies
/// each backend's readiness, authentication, and remediation details.
pub(super) fn step_items(
    step: PickerStep,
    lang: umadev_i18n::Lang,
    backends: &[BackendInfo],
) -> Vec<PickerItem> {
    match step {
        PickerStep::Language => umadev_i18n::Lang::ALL
            .iter()
            .map(|&lang| PickerItem {
                backend_id: None,
                label: lang.label().to_string(),
                ready: true,
                detail: lang.code().to_string(),
                group: PickerGroup::Language,
                lang: Some(lang),
                auth: AuthMark::LoggedIn,
                login_cmd: String::new(),
                install_cmd: String::new(),
            })
            .collect(),
        PickerStep::BaseCli => crate::FIRST_CLASS_BACKEND_IDS
            .iter()
            .map(|id| {
                let display = backend_label(lang, id);
                let probe = backends.iter().find(|backend| backend.id == *id);
                PickerItem {
                    backend_id: Some((*id).to_string()),
                    label: display,
                    ready: probe.is_some_and(|backend| backend.ready),
                    detail: probe.map_or_else(
                        || "detecting...".to_string(),
                        |backend| backend.detail.clone(),
                    ),
                    group: PickerGroup::HostCli,
                    lang: None,
                    auth: probe.map_or(AuthMark::Unknown, |backend| backend.auth),
                    login_cmd: probe
                        .map(|backend| backend.login_cmd.clone())
                        .unwrap_or_default(),
                    install_cmd: probe
                        .map(|backend| backend.install_cmd.clone())
                        .unwrap_or_default(),
                }
            })
            .collect(),
    }
}

/// Unpack the auth tag `spawn_probe` packed onto a probe `detail`. Returns
/// `(auth_mark, login_cmd, install_cmd, human_detail)`. **Fail-open**: a `detail`
/// with no sentinel (an external emitter, an older build) yields
/// `(Unknown, "", "", detail)`.
pub(crate) fn parse_probe_detail(detail: &str) -> (AuthMark, String, String, String) {
    let Some(rest) = detail.strip_prefix(PROBE_AUTH_SENTINEL) else {
        return (
            AuthMark::Unknown,
            String::new(),
            String::new(),
            detail.to_string(),
        );
    };
    let Some((meta, human)) = rest.split_once(PROBE_AUTH_SENTINEL) else {
        return (
            AuthMark::Unknown,
            String::new(),
            String::new(),
            rest.to_string(),
        );
    };
    let mut auth = AuthMark::Unknown;
    let mut login = String::new();
    let mut install = String::new();
    for field in meta.split('|') {
        if let Some(value) = field.strip_prefix("auth=") {
            auth = AuthMark::from_tag(value);
        } else if let Some(value) = field.strip_prefix("login=") {
            login = value.to_string();
        } else if let Some(value) = field.strip_prefix("install=") {
            install = value.to_string();
        }
    }
    (auth, login, install, human.to_string())
}

pub(super) fn refresh_picker_with_probes(items: &mut [PickerItem], probes: &[BackendInfo]) {
    for item in items.iter_mut() {
        if let Some(id) = item.backend_id.as_deref() {
            if let Some(probe) = probes.iter().find(|probe| probe.id == id) {
                item.ready = probe.ready;
                item.detail.clone_from(&probe.detail);
                item.auth = probe.auth;
                item.login_cmd.clone_from(&probe.login_cmd);
                item.install_cmd.clone_from(&probe.install_cmd);
            }
        }
    }
}

impl App {
    /// Switch the active base only after its latest probe has established a
    /// usable execution path. Authentication remains a two-step override because
    /// local and third-party configurations can make that probe inconclusive.
    pub(super) fn slash_backend(&mut self, backend: Option<&str>) -> Action {
        // A run or streaming chat owns a session pinned to the current base.
        // Switching underneath it would desynchronize the process, UI, and saved
        // configuration, so the user must cancel that work first.
        if self.has_interruptible_work() || self.thinking {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "backend.busy_no_switch"),
            );
            return Action::None;
        }

        let id = backend.unwrap_or("offline").to_string();
        if let Some(probe) = backend.and_then(|target| {
            self.backends
                .iter()
                .find(|candidate| candidate.id == target)
                .cloned()
        }) {
            match probe.auth {
                AuthMark::NotInstalled | AuthMark::Unknown if !probe.ready => {
                    let fix = if probe.install_cmd.trim().is_empty() {
                        probe.detail
                    } else {
                        probe.install_cmd
                    };
                    self.push(
                        ChatRole::System,
                        umadev_i18n::tf(self.lang, "backend.switch_unavailable", &[&id, &fix]),
                    );
                    return Action::None;
                }
                AuthMark::NotLoggedIn => {
                    if self.picker_login_confirm.as_deref() == Some(id.as_str()) {
                        self.picker_login_confirm = None;
                    } else {
                        let fix = if probe.login_cmd.trim().is_empty() {
                            probe.detail
                        } else {
                            probe.login_cmd
                        };
                        self.picker_login_confirm = Some(id.clone());
                        self.push(
                            ChatRole::System,
                            umadev_i18n::tf(
                                self.lang,
                                "backend.switch_login_unverified",
                                &[&id, &fix],
                            ),
                        );
                        return Action::None;
                    }
                }
                AuthMark::LoggedIn | AuthMark::Unknown | AuthMark::NotInstalled => {}
            }
        }

        let previous = self.backend_label.clone();
        self.commit_backend(backend.map(str::to_string));
        self.chat_session_dirty = true;
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "backend.switched", &[&id]),
        );

        // Native deep context cannot cross vendors. Persist an honest, bounded
        // transcript marker so the next base sees exactly what UmaDev carried.
        if previous != id && !self.conversation.is_empty() {
            let handoff = umadev_i18n::tf(self.lang, "backend.handoff", &[&previous, &id]);
            self.record_turn("system", handoff.clone());
            self.persist_chat();
            self.push(ChatRole::System, handoff);
        }
        self.refresh_status();
        Action::BackendChanged
    }

    /// A conclusive active-base failure from the asynchronous startup probe.
    /// `NotLoggedIn` stays overridable; a missing or unhealthy executable cannot
    /// accept a turn and must stop before routing mutates task state.
    pub(super) fn backend_definitely_unavailable(&self, backend: &str) -> Option<BackendInfo> {
        self.backends
            .iter()
            .find(|candidate| candidate.id == backend)
            .filter(|candidate| {
                candidate.auth == AuthMark::NotInstalled
                    || (candidate.auth == AuthMark::Unknown && !candidate.ready)
            })
            .cloned()
    }

    pub(super) fn reject_active_backend_unavailable(&mut self) -> bool {
        let Some(backend) = self.backend.clone() else {
            return false;
        };
        if backend == "offline" {
            return false;
        }
        let Some(probe) = self.backend_definitely_unavailable(&backend) else {
            return false;
        };
        let fix = if probe.install_cmd.trim().is_empty() {
            probe.detail
        } else {
            probe.install_cmd
        };
        let note = umadev_i18n::tf(self.lang, "backend.active_unavailable", &[&backend, &fix]);
        if self
            .history
            .back()
            .is_none_or(|message| message.body().trim() != note.trim())
        {
            self.push(ChatRole::System, note);
        }
        true
    }
}
