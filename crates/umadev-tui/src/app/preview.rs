//! `/preview`: start the project's dev server and open it in the browser.
//!
//! The frontend-notes file (`output/<slug>-frontend-notes.md`) is written by
//! the worker, but it can also ship with the repository or be shaped by a
//! prompt-injected model. Its `## Run command` is therefore free-form text
//! from an untrusted source: it is shown and needs `/preview confirm` before
//! it runs (and never runs in Plan mode), and its `## Preview URL` is only
//! honoured when it points at this machine.

use super::{parse_notes_section, read_bounded_utf8, Action, App, ChatRole, MAX_UI_ARTIFACT_BYTES};

impl App {
    /// Path to the frontend-notes markdown the worker writes (holds the
    /// `## Preview URL` + `## Run command` sections).
    fn frontend_notes_path(&self) -> std::path::PathBuf {
        self.project_root
            .join("output")
            .join(format!("{}-frontend-notes.md", self.slug))
    }

    /// Extract the `## Preview URL` value from the frontend-notes file.
    /// Returns `None` when the file is missing, the section is empty, or the
    /// URL names a host other than this machine (`/preview` waits on and
    /// opens it, so a remote host would turn the notes into a probe).
    #[must_use]
    pub fn preview_url_from_notes(&self) -> Option<String> {
        let body = read_bounded_utf8(&self.frontend_notes_path(), MAX_UI_ARTIFACT_BYTES).ok()?;
        parse_notes_section(&body, "Preview URL")
            .map(str::to_string)
            .filter(|u| crate::link::is_loopback_url(u))
    }

    /// Extract the `## Run command` value from the frontend-notes file.
    #[must_use]
    pub fn run_command_from_notes(&self) -> Option<String> {
        let body = read_bounded_utf8(&self.frontend_notes_path(), MAX_UI_ARTIFACT_BYTES).ok()?;
        parse_notes_section(&body, "Run command").map(str::to_string)
    }

    fn notes_preview_is_acceptance_harness(&self) -> bool {
        let Some(cmd) = self.run_command_from_notes() else {
            return false;
        };
        let cmd = cmd.to_ascii_lowercase().replace('\\', "/");
        // Mirror `verify::looks_like_root_acceptance_harness`: require a STRONG
        // harness marker (UmaDev's generated backend entrypoint or its static
        // frontend index). A bare `src/frontend` reference is too broad — a
        // normal app may legitimately record `cd src/frontend && npm run dev`.
        let looks_like_harness =
            cmd.contains("src/backend/server.mjs") || cmd.contains("src/frontend/index.html");
        if !looks_like_harness {
            return false;
        }
        [
            "jeecgboot-vue3",
            "jeecg-boot",
            "jeecguniapp",
            "pigx-ai-ui",
            "pigx-visual",
            "frontend",
            "web",
            "ui",
            "app",
        ]
        .iter()
        .any(|d| self.project_root.join(d).is_dir())
    }

    pub(super) fn preview_url_from_notes_for_product(&self) -> Option<String> {
        if self.notes_preview_is_acceptance_harness() {
            None
        } else {
            self.preview_url_from_notes()
        }
    }

    pub(super) fn run_command_from_notes_for_product(&self) -> Option<String> {
        if self.notes_preview_is_acceptance_harness() {
            None
        } else {
            self.run_command_from_notes()
        }
    }

    /// `/preview` — start the dev server in the background, open the browser,
    /// and tell the user. Falls back to a clear hint when no notes / no URL
    /// yet. `/preview confirm` runs a notes-recorded command its previous
    /// `/preview` displayed.
    pub(super) fn slash_preview(&mut self, arg: &str) -> Action {
        // If a server is already running, just re-open the browser.
        let already = self.preview_server.lock().is_ok_and(|g| g.is_some());
        if already {
            let url = self.effective_preview_url();
            if let Some(ref u) = url {
                let _ = crate::preview::open_url(u);
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "preview.already_running", &[u]),
                );
            }
            return Action::None;
        }

        // PREFERRED path: detect the dev server ourselves (Vite/Next/Astro/
        // CRA/static) from the project manifest. This does NOT depend on the
        // worker having recorded a Preview URL — it works even if the worker
        // forgot or used a different file name. Self-detection wins: we
        // control the command + know the URL.
        let url = self.effective_preview_url();
        if let Some(ds) = umadev_agent::verify::detect_dev_server(&self.project_root) {
            let display_url = url.unwrap_or_else(|| ds.default_url.to_string());
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(
                    self.lang,
                    "preview.detected",
                    &[ds.label, &ds.command, &display_url],
                ),
            );
            return Action::StartPreview {
                url: display_url,
                command: ds.command,
            };
        }
        let Some(url) = url else {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "preview.none_yet").to_string(),
            );
            return Action::None;
        };
        let Some(command) = self.run_command_from_notes_for_product() else {
            let _ = crate::preview::open_url(&url);
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "preview.opened", &[&url]),
            );
            return Action::None;
        };
        // The recorded command is shell text from a file the repository or the
        // model controls: never in Plan mode, and otherwise only the exact
        // command a previous `/preview` displayed (it is re-read every call, so
        // an edit between the preview and the confirmation asks again).
        if self.reject_workspace_mutation_in_plan() {
            return Action::None;
        }
        let confirmed = matches!(arg.trim(), "confirm" | "yes" | "go" | "y" | "确认" | "確認");
        let previewed = self.pending_preview_command.take();
        if !(confirmed && previewed.as_deref() == Some(command.as_str())) {
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(self.lang, "preview.confirm_command", &[&command, &url]),
            );
            self.pending_preview_command = Some(command);
            return Action::None;
        }
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(self.lang, "preview.starting", &[&url, &command]),
        );
        Action::StartPreview { url, command }
    }

    /// The Preview URL to actually open: prefer the worker-recorded value
    /// (it reflects the real port), fall back to the dev-server default
    /// (e.g. 5173 for Vite) when the worker did not record one.
    pub(super) fn effective_preview_url(&self) -> Option<String> {
        if let Some(u) = self.preview_url_from_notes_for_product() {
            return Some(u);
        }
        umadev_agent::verify::detect_dev_server(&self.project_root)
            .map(|ds| ds.default_url.to_string())
    }
}
