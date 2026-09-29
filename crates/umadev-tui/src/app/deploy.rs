//! `/deploy`: preview and run the recorded or detected deploy command.

use super::{
    parse_notes_section, read_bounded_utf8, which_on_path, Action, App, ChatRole,
    MAX_UI_ARTIFACT_BYTES,
};

impl App {
    /// Path to the delivery-notes markdown (holds deploy/URL/run sections).
    pub(super) fn delivery_notes_path(&self) -> std::path::PathBuf {
        self.project_root
            .join("output")
            .join(format!("{}-delivery-notes.md", self.slug))
    }

    /// Read the `## Deploy command` the worker recorded.
    #[must_use]
    pub fn deploy_command_from_notes(&self) -> Option<String> {
        let body = read_bounded_utf8(&self.delivery_notes_path(), MAX_UI_ARTIFACT_BYTES).ok()?;
        parse_notes_section(&body, "Deploy command").map(str::to_string)
    }

    /// Read the `## Frontend URL` (live URL after a deploy).
    #[must_use]
    pub fn deploy_url_from_notes(&self) -> Option<String> {
        let body = read_bounded_utf8(&self.delivery_notes_path(), MAX_UI_ARTIFACT_BYTES).ok()?;
        parse_notes_section(&body, "Frontend URL")
            .map(str::to_string)
            .filter(|u| crate::link::is_safe_url(u))
    }

    /// `/deploy` — run the deploy command the worker recorded so the project
    /// goes live. The command typically logs into a platform CLI and pushes
    /// (e.g. `npx vercel --prod`). We run it in the foreground so its login
    /// prompts / output reach the user; the URL is surfaced after.
    pub(super) fn slash_deploy(&mut self, arg: &str) -> Action {
        // Detect the deploy target from the workspace's own files (Vercel /
        // Netlify / Fly / Cloudflare / Docker / static host). This drives both
        // the CLI pre-flight check and the fallback command when the base never
        // recorded one.
        let target = umadev_agent::detect_deploy_target(&self.project_root);

        // Pre-flight: check the deploy CLI is installed. Prefer the detected
        // platform's CLI; fall back to the common set so a generic project still
        // gets a useful answer.
        let deploy_cli = target
            .cli_binary()
            .filter(|c| which_on_path(c))
            .or_else(|| {
                ["vercel", "netlify", "wrangler", "flyctl", "docker"]
                    .into_iter()
                    .find(|c| which_on_path(c))
            });
        if deploy_cli.is_none() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "deploy.cli_missing").to_string(),
            );
        } else {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "deploy.cli_ready").to_string(),
            );
        }

        // Surface the detected platform so the user sees what we'll deploy to.
        if target != umadev_agent::DeployTarget::None {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "deploy.detected", &[target.label()]),
            );
        }

        // Command priority: the base-recorded `## Deploy command` (most precise),
        // then the detected platform's canonical command (fail-open fallback so
        // /deploy still works when the base didn't fill in the recipe).
        let Some(cmd) = self
            .deploy_command_from_notes()
            .or_else(|| target.deploy_command())
        else {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "deploy.no_command").to_string(),
            );
            return Action::None;
        };
        // Reversibility floor (fail-SAFE): a deploy reaches the network and
        // ships outward, so it is irreversible BY NATURE — it must be confirmed
        // REGARDLESS of the active trust tier (even `auto` cannot skip it). We
        // consult the `trust::requires_confirmation` floor on a `git push`-class
        // probe (a deploy is publish-outward, exactly the network class the floor
        // escalates) so the gate is mode-independent even for a recipe the
        // generic classifier wouldn't recognise on its own (e.g. `npx vercel
        // --prod`). We protect the user's project — when in doubt, confirm.
        // `/deploy confirm` (or yes / go / 确认) actually deploys.
        let floor_requires_confirm = umadev_agent::requires_confirmation(
            self.effective_trust_mode(),
            &format!("git push (deploy) {cmd}"),
            "",
        );
        //
        // The recipe is re-read from `output/` on every call, so a confirmation
        // only authorizes the exact command its preview displayed. A missing or
        // different preview shows the current command and asks again.
        let confirmed = matches!(arg.trim(), "confirm" | "yes" | "go" | "y" | "确认" | "確認");
        let previewed = self.pending_deploy_command.take();
        if floor_requires_confirm && !(confirmed && previewed.as_deref() == Some(cmd.as_str())) {
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(self.lang, "deploy.confirm_preflight", &[&cmd]),
            );
            self.pending_deploy_command = Some(cmd);
            return Action::None;
        }
        // NOTE: `/deploy confirm` deliberately records NOTHING in the trust ledger.
        // The deploy gate above is mode-independent — it always consults an
        // always-escalating `git push (deploy)` probe — so a remembered rule could
        // never skip a future deploy anyway. But stock recipes (`npx vercel --prod`,
        // `flyctl deploy`, `docker build …`) carry no network token, so they classify
        // as a reversible Shell command: recording one here would mint a
        // `shell:<recipe>` rule that silently auto-allows the identical raw shell
        // invocation as an ordinary later tool call. A one-off outward deploy must not
        // grant standing shell authority, so we do not remember it.
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(self.lang, "deploy.starting", &[&cmd]),
        );
        Action::RunDeploy { command: cmd }
    }
}
