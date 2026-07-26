//! Explicit Grok Build authentication interaction for the terminal surface.
//!
//! The base session opener runs off the render thread. This module keeps the
//! user-facing state deterministic and gives that task a narrow, generation-
//! fenced command bridge. Merely presenting an offer or URL has no side effect:
//! authorization and URL opening are distinct explicit key actions.

use std::fmt;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use umadev_host::session_bootstrap::{
    AuthChallenge, AuthControl, AuthControlError, AuthMethodSummary, AuthMode, AuthOffer,
    SafeAuthUrl, SensitiveText, SessionOpenId,
};

const MAX_MANUAL_CODE_BYTES: usize = 8_192;

/// User command delivered to the one session-opening task that owns a prompt.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum AuthUserDecision {
    /// The user selected and explicitly confirmed this exact advertised method.
    Authorize { generation: u64, method_id: String },
    /// The user explicitly cancelled this authentication generation.
    Cancel { generation: u64 },
}

struct PendingAuthInteraction {
    generation: u64,
    decision_tx: tokio::sync::mpsc::UnboundedSender<AuthUserDecision>,
    control: Option<AuthControl>,
}

/// Shared, short-held bridge between the render loop and the opening task.
#[derive(Clone, Default)]
pub(crate) struct AuthInteractionHolder {
    inner: Arc<Mutex<Option<PendingAuthInteraction>>>,
}

impl AuthInteractionHolder {
    /// Register one generation and return its private decision receiver.
    pub(crate) fn register(
        &self,
        generation: u64,
    ) -> tokio::sync::mpsc::UnboundedReceiver<AuthUserDecision> {
        let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
        if let Ok(mut pending) = self.inner.lock() {
            if let Some(stale) = pending.take() {
                if let Some(control) = stale.control {
                    let _ = control.try_cancel();
                }
                let _ = stale.decision_tx.send(AuthUserDecision::Cancel {
                    generation: stale.generation,
                });
            }
            *pending = Some(PendingAuthInteraction {
                generation,
                decision_tx,
                control: None,
            });
        }
        decision_rx
    }

    /// Attach the challenge's bounded control endpoint to its owning generation.
    pub(crate) fn set_control(&self, generation: u64, control: AuthControl) -> bool {
        let Ok(mut pending) = self.inner.lock() else {
            let _ = control.try_cancel();
            return false;
        };
        let Some(active) = pending.as_mut() else {
            let _ = control.try_cancel();
            return false;
        };
        if active.generation != generation {
            let _ = control.try_cancel();
            return false;
        }
        if let Some(stale) = active.control.replace(control) {
            let _ = stale.try_cancel();
        }
        true
    }

    /// Deliver an explicitly confirmed method to the matching opening task.
    pub(crate) fn authorize(&self, generation: u64, method_id: String) -> bool {
        self.inner.lock().ok().is_some_and(|pending| {
            pending.as_ref().is_some_and(|active| {
                active.generation == generation
                    && active
                        .decision_tx
                        .send(AuthUserDecision::Authorize {
                            generation,
                            method_id,
                        })
                        .is_ok()
            })
        })
    }

    /// Submit a manually entered loopback code through the typed host boundary.
    pub(crate) fn submit_code(
        &self,
        generation: u64,
        code: SensitiveText,
    ) -> Result<(), AuthControlError> {
        let pending = self.inner.lock().map_err(|_| AuthControlError::Closed)?;
        let active = pending.as_ref().ok_or(AuthControlError::Closed)?;
        if active.generation != generation {
            return Err(AuthControlError::Closed);
        }
        active
            .control
            .as_ref()
            .ok_or(AuthControlError::Closed)?
            .try_submit_code(code)
    }

    /// Explicitly cancel an active generation and its host-side challenge.
    pub(crate) fn cancel(&self, generation: u64) -> bool {
        let Ok(mut pending) = self.inner.lock() else {
            return false;
        };
        let Some(active) = pending.as_ref() else {
            return false;
        };
        if active.generation != generation {
            return false;
        }
        let Some(active) = pending.take() else {
            return false;
        };
        if let Some(control) = active.control {
            let _ = control.try_cancel();
        }
        let _ = active
            .decision_tx
            .send(AuthUserDecision::Cancel { generation });
        true
    }

    /// Cancel whichever generation is active (Ctrl-C, quit, backend reset).
    pub(crate) fn cancel_active(&self) -> bool {
        let generation = self
            .inner
            .lock()
            .ok()
            .and_then(|pending| pending.as_ref().map(|active| active.generation));
        generation.is_some_and(|generation| self.cancel(generation))
    }

    /// Remove a successfully settled generation without sending cancellation.
    pub(crate) fn finish(&self, generation: u64) {
        if let Ok(mut pending) = self.inner.lock() {
            if pending
                .as_ref()
                .is_some_and(|active| active.generation == generation)
            {
                pending.take();
            }
        }
    }
}

/// Ephemeral text that is always redacted from debug output.
#[derive(Clone, Eq, PartialEq)]
struct EphemeralSecret(String);

impl EphemeralSecret {
    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn reveal(&self) -> &str {
        &self.0
    }

    fn bullets(&self) -> String {
        "•".repeat(self.0.chars().count())
    }
}

impl fmt::Debug for EphemeralSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EphemeralSecret([REDACTED])")
    }
}

/// Safe renderer snapshot of a validated host challenge.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AuthChallengeView {
    attempt_id: SessionOpenId,
    method_id: String,
    mode: AuthMode,
    url: Option<SafeAuthUrl>,
    command_status: Option<String>,
    device_code: Option<EphemeralSecret>,
}

impl fmt::Debug for AuthChallengeView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthChallengeView")
            .field("attempt_id", &self.attempt_id)
            .field("method_id", &self.method_id)
            .field("mode", &self.mode)
            .field("url", &self.url)
            .field(
                "command_status",
                &self.command_status.as_ref().map(|_| "[REDACTED]"),
            )
            .field("device_code", &self.device_code)
            .finish()
    }
}

impl AuthChallengeView {
    /// Consume a source-validated host challenge at the ephemeral UI boundary.
    pub(crate) fn from_host(challenge: AuthChallenge) -> Self {
        let mode = challenge.mode();
        let url = challenge.safe_url().cloned();
        let command_status = challenge
            .command_status()
            .map(|status| status.reveal_for_render().to_string());
        let device_code = challenge
            .user_code()
            .map(|code| EphemeralSecret::new(code.reveal()));
        Self {
            attempt_id: challenge.attempt_id,
            method_id: challenge.method_id,
            mode,
            url,
            command_status,
            device_code,
        }
    }

    pub(crate) const fn attempt_id(&self) -> SessionOpenId {
        self.attempt_id
    }
}

/// Render-loop event produced by the session-opening task.
#[derive(Debug)]
pub(crate) enum AuthUiEvent {
    /// A non-interactive open stopped before any browser-capable RPC.
    Offer { generation: u64, offer: AuthOffer },
    /// A method was explicitly confirmed and a fresh child is being opened.
    Starting {
        generation: u64,
        attempt_id: SessionOpenId,
        method_id: String,
    },
    /// The host published a validated mode-specific challenge.
    Challenge {
        generation: u64,
        challenge: AuthChallengeView,
    },
    /// The fresh opening attempt failed; the same original turn remains parked.
    Failed {
        generation: u64,
        attempt_id: Option<SessionOpenId>,
        message: String,
    },
    /// Authentication succeeded and the original turn is continuing once.
    Clear { generation: u64 },
}

/// The visible phase of one authentication generation.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum AuthUiPhase {
    Choose,
    Confirm,
    Starting,
    Challenge,
    Failed,
}

/// Pure key-handling effect. Browser and clipboard work happen outside the state
/// machine and only for their corresponding explicit variants.
#[derive(Debug)]
pub(crate) enum AuthUiEffect {
    None,
    Authorize {
        generation: u64,
        method_id: String,
    },
    Cancel {
        generation: u64,
    },
    OpenUrl {
        generation: u64,
        url: SafeAuthUrl,
    },
    CopyUrl {
        generation: u64,
        url: SafeAuthUrl,
    },
    SubmitCode {
        generation: u64,
        code: SensitiveText,
    },
}

fn set_local_error(app: &mut crate::App, generation: u64, message: String) {
    if let Some(auth) = app.auth_ui.as_mut() {
        auth.set_local_error(generation, message);
    }
}

fn copy_url(app: &mut crate::App, terminal: &mut crate::Term, generation: u64, url: SafeAuthUrl) {
    if app
        .auth_ui
        .as_ref()
        .is_none_or(|auth| auth.generation() != generation)
    {
        return;
    }
    let text = url.reveal().to_string();
    if !crate::clipboard::copy_text_to_clipboard(app, terminal, &text) {
        set_local_error(
            app,
            generation,
            umadev_i18n::t(app.lang, "tui.copy_failed").to_string(),
        );
        return;
    }
    if crate::clipboard::clipboard_is_remote() {
        app.transient_status = Some(umadev_i18n::t(app.lang, "auth.grok.url_copied").to_string());
    } else {
        app.transient_status = None;
    }
}

/// Route one authentication-overlay key without adding another branch cluster
/// to the main terminal event loop.
pub(crate) fn handle_loop_key(
    app: &mut crate::App,
    chat_session_holder: &crate::ChatSessionHolder,
    terminal: &mut crate::Term,
    key: KeyEvent,
) -> bool {
    if app.auth_ui.is_none()
        || key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return false;
    }

    let effect = app.auth_ui.as_mut().map_or(AuthUiEffect::None, |auth| {
        auth.handle_key(key.code, key.modifiers)
    });
    match effect {
        AuthUiEffect::None => {}
        AuthUiEffect::Authorize {
            generation,
            method_id,
        } => {
            if !chat_session_holder
                .auth_interaction
                .authorize(generation, method_id)
            {
                set_local_error(
                    app,
                    generation,
                    "authentication task is no longer available".to_string(),
                );
            }
        }
        AuthUiEffect::Cancel { generation } => {
            let _ = chat_session_holder.auth_interaction.cancel(generation);
            app.auth_ui = None;
        }
        AuthUiEffect::OpenUrl { generation, url } => {
            if let Err(error) = crate::link::spawn_opener(url.reveal()) {
                set_local_error(
                    app,
                    generation,
                    umadev_i18n::tf(app.lang, "auth.grok.url_open_failed", &[&error.to_string()]),
                );
            }
        }
        AuthUiEffect::CopyUrl { generation, url } => {
            copy_url(app, terminal, generation, url);
        }
        AuthUiEffect::SubmitCode { generation, code } => {
            if let Err(error) = chat_session_holder
                .auth_interaction
                .submit_code(generation, code)
            {
                set_local_error(
                    app,
                    generation,
                    umadev_i18n::tf(app.lang, "auth.grok.code_failed", &[&error.to_string()]),
                );
            }
        }
    }
    true
}

/// Pure renderer/input state for one explicitly user-owned authentication flow.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AuthUiState {
    generation: u64,
    offer: AuthOffer,
    selected_method: usize,
    phase: AuthUiPhase,
    active_attempt: Option<SessionOpenId>,
    active_method_id: Option<String>,
    challenge: Option<AuthChallengeView>,
    editing_code: bool,
    manual_code: EphemeralSecret,
    error: Option<String>,
}

impl fmt::Debug for AuthUiState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthUiState")
            .field("generation", &self.generation)
            .field("offer", &self.offer)
            .field("selected_method", &self.selected_method)
            .field("phase", &self.phase)
            .field("active_attempt", &self.active_attempt)
            .field("active_method_id", &self.active_method_id)
            .field("challenge", &self.challenge)
            .field("editing_code", &self.editing_code)
            .field("manual_code", &self.manual_code)
            .field("error", &self.error)
            .finish()
    }
}

impl AuthUiState {
    /// Start from one typed offer. No effect is emitted by construction.
    pub(crate) fn new(generation: u64, offer: AuthOffer) -> Self {
        let selected_method = preferred_interactive_method(&offer).unwrap_or(0);
        let error = (!offer.methods.iter().any(|method| method.interactive))
            .then(|| "the base advertised no interactive authentication method".to_string());
        Self {
            generation,
            offer,
            selected_method,
            phase: if error.is_some() {
                AuthUiPhase::Failed
            } else {
                AuthUiPhase::Choose
            },
            active_attempt: None,
            active_method_id: None,
            challenge: None,
            editing_code: false,
            manual_code: EphemeralSecret::new(String::new()),
            error,
        }
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(crate) const fn phase(&self) -> AuthUiPhase {
        self.phase
    }

    pub(crate) fn set_local_error(&mut self, generation: u64, message: String) -> bool {
        if generation != self.generation {
            return false;
        }
        if self.phase == AuthUiPhase::Starting {
            self.phase = AuthUiPhase::Failed;
        }
        self.error = Some(message);
        true
    }

    pub(crate) fn selected_method(&self) -> Option<&AuthMethodSummary> {
        self.offer
            .methods
            .get(self.selected_method)
            .filter(|method| method.interactive)
    }

    /// Apply a task event only when it belongs to this exact generation and
    /// active attempt. Late events are inert.
    pub(crate) fn apply_event(&mut self, event: AuthUiEvent) -> bool {
        match event {
            AuthUiEvent::Offer { generation, offer } => {
                if generation < self.generation {
                    return false;
                }
                *self = Self::new(generation, offer);
                true
            }
            AuthUiEvent::Starting {
                generation,
                attempt_id,
                method_id,
            } if generation == self.generation => {
                self.phase = AuthUiPhase::Starting;
                self.active_attempt = Some(attempt_id);
                self.active_method_id = Some(method_id);
                self.challenge = None;
                self.editing_code = false;
                self.manual_code = EphemeralSecret::new(String::new());
                self.error = None;
                true
            }
            AuthUiEvent::Challenge {
                generation,
                challenge,
            } if generation == self.generation
                && self.active_attempt == Some(challenge.attempt_id()) =>
            {
                self.phase = AuthUiPhase::Challenge;
                self.challenge = Some(challenge);
                self.error = None;
                true
            }
            AuthUiEvent::Failed {
                generation,
                attempt_id,
                message,
            } if generation == self.generation
                && attempt_id.is_none_or(|attempt| self.active_attempt == Some(attempt)) =>
            {
                self.phase = AuthUiPhase::Failed;
                self.challenge = None;
                self.editing_code = false;
                self.manual_code = EphemeralSecret::new(String::new());
                self.error = Some(message);
                true
            }
            AuthUiEvent::Clear { generation } if generation == self.generation => true,
            AuthUiEvent::Starting { .. }
            | AuthUiEvent::Challenge { .. }
            | AuthUiEvent::Failed { .. }
            | AuthUiEvent::Clear { .. } => false,
        }
    }

    /// Handle one key without any process, browser, clipboard, or network work.
    pub(crate) fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> AuthUiEffect {
        if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) {
            return AuthUiEffect::None;
        }
        if code == KeyCode::Esc {
            return AuthUiEffect::Cancel {
                generation: self.generation,
            };
        }
        match self.phase {
            AuthUiPhase::Choose => self.handle_choose(code),
            AuthUiPhase::Confirm => self.handle_confirm(code),
            AuthUiPhase::Starting => AuthUiEffect::None,
            AuthUiPhase::Challenge => self.handle_challenge(code),
            AuthUiPhase::Failed => {
                if matches!(code, KeyCode::Enter | KeyCode::Char('r' | 'R'))
                    && self.offer.methods.iter().any(|method| method.interactive)
                {
                    self.phase = AuthUiPhase::Choose;
                    self.active_attempt = None;
                    self.active_method_id = None;
                    self.error = None;
                }
                AuthUiEffect::None
            }
        }
    }

    /// Paste belongs to the masked loopback-code editor only. Everywhere else
    /// it is consumed without mutating the user's ordinary chat draft.
    pub(crate) fn handle_paste(&mut self, text: &str) -> bool {
        if self.phase != AuthUiPhase::Challenge || !self.editing_code {
            return true;
        }
        if text.chars().any(char::is_control) {
            self.error = Some("authentication code cannot contain control characters".to_string());
            return true;
        }
        if self.manual_code.0.len().saturating_add(text.len()) > MAX_MANUAL_CODE_BYTES {
            self.error = Some("authentication code exceeds 8192 bytes".to_string());
            return true;
        }
        self.manual_code.0.push_str(text);
        self.error = None;
        true
    }

    fn handle_choose(&mut self, code: KeyCode) -> AuthUiEffect {
        match code {
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down | KeyCode::Tab => self.move_selection(1),
            KeyCode::Char(digit @ '1'..='9') => {
                let ordinal = usize::from(digit as u8 - b'1');
                if let Some(index) = interactive_indices(&self.offer).get(ordinal) {
                    self.selected_method = *index;
                }
            }
            KeyCode::Enter if self.selected_method().is_some() => {
                self.phase = AuthUiPhase::Confirm;
            }
            _ => {}
        }
        AuthUiEffect::None
    }

    fn handle_confirm(&mut self, code: KeyCode) -> AuthUiEffect {
        match code {
            KeyCode::Char('n' | 'N') | KeyCode::Backspace => {
                self.phase = AuthUiPhase::Choose;
                AuthUiEffect::None
            }
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                let Some(method_id) = self.selected_method().map(|method| method.id.clone()) else {
                    self.phase = AuthUiPhase::Failed;
                    self.error = Some("the selected method is no longer available".to_string());
                    return AuthUiEffect::None;
                };
                self.phase = AuthUiPhase::Starting;
                self.active_method_id = Some(method_id.clone());
                AuthUiEffect::Authorize {
                    generation: self.generation,
                    method_id,
                }
            }
            _ => AuthUiEffect::None,
        }
    }

    fn handle_challenge(&mut self, code: KeyCode) -> AuthUiEffect {
        let Some(challenge) = self.challenge.as_ref() else {
            return AuthUiEffect::None;
        };
        if self.editing_code {
            match code {
                KeyCode::Enter => {
                    let raw = std::mem::take(&mut self.manual_code.0);
                    match SensitiveText::auth_code(raw) {
                        Ok(code) => {
                            self.editing_code = false;
                            self.error = None;
                            return AuthUiEffect::SubmitCode {
                                generation: self.generation,
                                code,
                            };
                        }
                        Err(error) => self.error = Some(error.to_string()),
                    }
                }
                KeyCode::Backspace => {
                    self.manual_code.0.pop();
                    self.error = None;
                }
                KeyCode::Char(character) => {
                    if self
                        .manual_code
                        .0
                        .len()
                        .saturating_add(character.len_utf8())
                        <= MAX_MANUAL_CODE_BYTES
                    {
                        self.manual_code.0.push(character);
                        self.error = None;
                    } else {
                        self.error = Some("authentication code exceeds 8192 bytes".to_string());
                    }
                }
                _ => {}
            }
            return AuthUiEffect::None;
        }
        match code {
            KeyCode::Char('o' | 'O') => {
                challenge
                    .url
                    .clone()
                    .map_or(AuthUiEffect::None, |url| AuthUiEffect::OpenUrl {
                        generation: self.generation,
                        url,
                    })
            }
            KeyCode::Char('c' | 'C') => {
                challenge
                    .url
                    .clone()
                    .map_or(AuthUiEffect::None, |url| AuthUiEffect::CopyUrl {
                        generation: self.generation,
                        url,
                    })
            }
            KeyCode::Char('i' | 'I') if challenge.mode.accepts_manual_code() => {
                self.editing_code = true;
                self.manual_code = EphemeralSecret::new(String::new());
                self.error = None;
                AuthUiEffect::None
            }
            _ => AuthUiEffect::None,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let indices = interactive_indices(&self.offer);
        if indices.is_empty() {
            return;
        }
        let current = indices
            .iter()
            .position(|index| *index == self.selected_method)
            .unwrap_or(0);
        let len = isize::try_from(indices.len()).unwrap_or(1);
        let next = (isize::try_from(current).unwrap_or(0) + delta).rem_euclid(len);
        self.selected_method = indices[usize::try_from(next).unwrap_or(0)];
    }

    /// Bounded plain rows for the sticky authentication panel.
    pub(crate) fn panel_lines(&self, lang: umadev_i18n::Lang) -> Vec<String> {
        match self.phase {
            AuthUiPhase::Choose => self.choose_lines(lang),
            AuthUiPhase::Confirm => self.confirm_lines(lang),
            AuthUiPhase::Starting => vec![
                umadev_i18n::t(lang, "auth.grok.starting").to_string(),
                umadev_i18n::t(lang, "auth.grok.cancel_hint").to_string(),
            ],
            AuthUiPhase::Challenge => self.challenge_lines(lang),
            AuthUiPhase::Failed => vec![
                umadev_i18n::t(lang, "auth.grok.failed").to_string(),
                self.error.clone().unwrap_or_else(|| {
                    umadev_i18n::t(lang, "auth.grok.failed_unknown").to_string()
                }),
                umadev_i18n::t(lang, "auth.grok.retry_hint").to_string(),
            ],
        }
    }

    pub(crate) fn panel_height(&self) -> u16 {
        u16::try_from(self.panel_lines(umadev_i18n::Lang::En).len())
            .unwrap_or(7)
            .clamp(2, 7)
    }

    fn choose_lines(&self, lang: umadev_i18n::Lang) -> Vec<String> {
        let mut lines = vec![umadev_i18n::t(lang, "auth.grok.choose").to_string()];
        for (ordinal, index) in interactive_indices(&self.offer).into_iter().enumerate() {
            if let Some(method) = self.offer.methods.get(index) {
                let marker = if index == self.selected_method {
                    '>'
                } else {
                    ' '
                };
                lines.push(format!(
                    "{marker} {}. {} ({})",
                    ordinal + 1,
                    method.label,
                    method.id
                ));
            }
        }
        lines.push(umadev_i18n::t(lang, "auth.grok.choose_hint").to_string());
        lines.truncate(7);
        lines
    }

    fn confirm_lines(&self, lang: umadev_i18n::Lang) -> Vec<String> {
        let method = self.selected_method();
        vec![
            umadev_i18n::t(lang, "auth.grok.confirm").to_string(),
            method.map_or_else(String::new, |method| {
                format!("{} ({})", method.label, method.id)
            }),
            if self.offer.may_open_browser {
                umadev_i18n::t(lang, "auth.grok.browser_warning").to_string()
            } else {
                umadev_i18n::t(lang, "auth.grok.explicit_warning").to_string()
            },
            umadev_i18n::t(lang, "auth.grok.confirm_hint").to_string(),
        ]
    }

    fn challenge_lines(&self, lang: umadev_i18n::Lang) -> Vec<String> {
        let Some(challenge) = self.challenge.as_ref() else {
            return vec![umadev_i18n::t(lang, "auth.grok.starting").to_string()];
        };
        let mut lines = vec![match challenge.mode {
            AuthMode::Loopback => umadev_i18n::t(lang, "auth.grok.loopback").to_string(),
            AuthMode::Device => umadev_i18n::t(lang, "auth.grok.device").to_string(),
            AuthMode::Command => umadev_i18n::t(lang, "auth.grok.command").to_string(),
        }];
        if let Some(url) = &challenge.url {
            lines.push(url.to_string());
        }
        if let Some(code) = &challenge.device_code {
            lines.push(umadev_i18n::tf(
                lang,
                "auth.grok.device_code",
                &[code.reveal()],
            ));
        }
        if let Some(status) = &challenge.command_status {
            lines.extend(status.lines().take(3).map(ToString::to_string));
        }
        if self.editing_code {
            lines.push(umadev_i18n::tf(
                lang,
                "auth.grok.code_masked",
                &[&self.manual_code.bullets()],
            ));
            lines.push(umadev_i18n::t(lang, "auth.grok.code_submit_hint").to_string());
        } else {
            lines.push(match challenge.mode {
                AuthMode::Loopback => umadev_i18n::t(lang, "auth.grok.loopback_hint").to_string(),
                AuthMode::Device => umadev_i18n::t(lang, "auth.grok.device_hint").to_string(),
                AuthMode::Command => umadev_i18n::t(lang, "auth.grok.command_hint").to_string(),
            });
        }
        if let Some(error) = &self.error {
            lines.push(error.clone());
        }
        lines.truncate(7);
        lines
    }
}

fn interactive_indices(offer: &AuthOffer) -> Vec<usize> {
    offer
        .methods
        .iter()
        .enumerate()
        .filter_map(|(index, method)| method.interactive.then_some(index))
        .collect()
}

fn preferred_interactive_method(offer: &AuthOffer) -> Option<usize> {
    offer
        .default_method_id
        .as_deref()
        .and_then(|default| {
            offer
                .methods
                .iter()
                .position(|method| method.id == default && method.interactive)
        })
        .or_else(|| offer.methods.iter().position(|method| method.interactive))
}

#[cfg(test)]
mod tests {
    use super::*;
    use umadev_host::session_bootstrap::{AuthUrlDetails, SafeCommandAuthStatus};

    fn offer() -> AuthOffer {
        AuthOffer::new(
            "grok-build",
            vec![
                AuthMethodSummary::new("cached_token", "Cached", false),
                AuthMethodSummary::new("grok.com", "Grok", true),
                AuthMethodSummary::new("oidc", "Enterprise", true),
            ],
            Some("cached_token".to_string()),
            true,
        )
    }

    fn challenge(attempt: u64, mode: AuthMode) -> AuthChallengeView {
        let details = match mode {
            AuthMode::Loopback => AuthUrlDetails::Loopback {
                url: SafeAuthUrl::parse("http://127.0.0.1:1455/callback?state=secret").unwrap(),
            },
            AuthMode::Device => AuthUrlDetails::Device {
                url: SafeAuthUrl::parse("https://grok.com/device?user_code=ABCD-EFGH&state=secret")
                    .unwrap(),
                user_code: Some(SensitiveText::auth_code("ABCD-EFGH").unwrap()),
            },
            AuthMode::Command => AuthUrlDetails::Command {
                status: SafeCommandAuthStatus::from_untrusted("Waiting for enterprise login")
                    .unwrap(),
            },
        };
        AuthChallengeView::from_host(AuthChallenge {
            attempt_id: SessionOpenId::new(attempt),
            method_id: "grok.com".to_string(),
            details,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
        })
    }

    #[test]
    fn offer_requires_selection_then_a_separate_explicit_confirmation() {
        let mut state = AuthUiState::new(7, offer());
        assert_eq!(state.phase(), AuthUiPhase::Choose);
        assert_eq!(state.selected_method().unwrap().id, "grok.com");
        assert!(matches!(
            state.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            AuthUiEffect::None
        ));
        assert_eq!(state.phase(), AuthUiPhase::Confirm);
        assert!(matches!(
            state.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            AuthUiEffect::Authorize {
                generation: 7,
                method_id,
            } if method_id == "grok.com"
        ));
    }

    #[test]
    fn auto_mode_has_no_state_machine_shortcut_to_authorization() {
        let state = AuthUiState::new(9, offer());
        assert_eq!(state.phase(), AuthUiPhase::Choose);
        assert!(state.active_attempt.is_none());
    }

    #[test]
    fn url_is_inert_until_the_explicit_open_or_copy_key() {
        let mut state = AuthUiState::new(3, offer());
        assert!(state.apply_event(AuthUiEvent::Starting {
            generation: 3,
            attempt_id: SessionOpenId::new(31),
            method_id: "grok.com".to_string(),
        }));
        assert!(state.apply_event(AuthUiEvent::Challenge {
            generation: 3,
            challenge: challenge(31, AuthMode::Loopback),
        }));
        assert!(matches!(
            state.handle_key(KeyCode::Char('x'), KeyModifiers::NONE),
            AuthUiEffect::None
        ));
        assert!(matches!(
            state.handle_key(KeyCode::Char('o'), KeyModifiers::NONE),
            AuthUiEffect::OpenUrl { generation: 3, .. }
        ));
        assert!(matches!(
            state.handle_key(KeyCode::Char('c'), KeyModifiers::NONE),
            AuthUiEffect::CopyUrl { generation: 3, .. }
        ));
    }

    #[test]
    fn manual_code_is_masked_and_moves_only_through_sensitive_effect() {
        let mut state = AuthUiState::new(4, offer());
        state.apply_event(AuthUiEvent::Starting {
            generation: 4,
            attempt_id: SessionOpenId::new(41),
            method_id: "grok.com".to_string(),
        });
        state.apply_event(AuthUiEvent::Challenge {
            generation: 4,
            challenge: challenge(41, AuthMode::Loopback),
        });
        state.handle_key(KeyCode::Char('i'), KeyModifiers::NONE);
        state.handle_paste("secret-code");
        let rendered = state.panel_lines(umadev_i18n::Lang::En).join("\n");
        assert!(!rendered.contains("secret-code"));
        assert!(rendered.contains("•••••••••••"));
        assert!(matches!(
            state.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            AuthUiEffect::SubmitCode { generation: 4, code }
                if code.reveal() == "secret-code"
        ));
        assert!(!format!("{state:?}").contains("secret-code"));
    }

    #[test]
    fn manual_code_editor_rejects_input_beyond_the_protocol_limit() {
        let mut state = AuthUiState::new(5, offer());
        state.apply_event(AuthUiEvent::Starting {
            generation: 5,
            attempt_id: SessionOpenId::new(51),
            method_id: "grok.com".to_string(),
        });
        state.apply_event(AuthUiEvent::Challenge {
            generation: 5,
            challenge: challenge(51, AuthMode::Loopback),
        });
        state.handle_key(KeyCode::Char('i'), KeyModifiers::NONE);
        state.handle_paste(&"x".repeat(MAX_MANUAL_CODE_BYTES));
        state.handle_paste("secret-overflow");
        assert_eq!(state.manual_code.0.len(), MAX_MANUAL_CODE_BYTES);
        assert!(state.error.as_deref().unwrap().contains("8192"));
        assert!(!format!("{state:?}").contains("secret-overflow"));
        state.handle_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert_eq!(state.manual_code.0.len(), MAX_MANUAL_CODE_BYTES);
    }

    #[tokio::test]
    async fn cancel_calls_auth_control_and_notifies_only_the_matching_generation() {
        let holder = AuthInteractionHolder::default();
        let mut decisions = holder.register(11);
        let (control, mut control_rx) = AuthControl::channel_for_mode(AuthMode::Loopback);
        assert!(holder.set_control(11, control));
        assert!(!holder.cancel(10));
        assert!(holder.cancel(11));
        assert_eq!(
            control_rx.recv().await,
            Some(umadev_host::session_bootstrap::AuthCommand::Cancel)
        );
        assert_eq!(
            decisions.recv().await,
            Some(AuthUserDecision::Cancel { generation: 11 })
        );
    }

    #[test]
    fn late_challenge_and_failure_cannot_replace_a_new_generation() {
        let mut state = AuthUiState::new(20, offer());
        assert!(!state.apply_event(AuthUiEvent::Challenge {
            generation: 19,
            challenge: challenge(191, AuthMode::Loopback),
        }));
        assert!(!state.apply_event(AuthUiEvent::Failed {
            generation: 19,
            attempt_id: None,
            message: "stale".to_string(),
        }));
        assert_eq!(state.generation(), 20);
        assert_eq!(state.phase(), AuthUiPhase::Choose);
    }

    #[test]
    fn command_and_device_modes_never_accept_manual_code() {
        for mode in [AuthMode::Command, AuthMode::Device] {
            let mut state = AuthUiState::new(5, offer());
            state.apply_event(AuthUiEvent::Starting {
                generation: 5,
                attempt_id: SessionOpenId::new(51),
                method_id: "grok.com".to_string(),
            });
            state.apply_event(AuthUiEvent::Challenge {
                generation: 5,
                challenge: challenge(51, mode),
            });
            assert!(matches!(
                state.handle_key(KeyCode::Char('i'), KeyModifiers::NONE),
                AuthUiEffect::None
            ));
            assert!(!state.editing_code);
        }
    }

    #[tokio::test]
    async fn hermetic_fake_host_waits_for_confirm_then_resumes_original_once() {
        let bridge = AuthInteractionHolder::default();
        let mut decisions = bridge.register(70);
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let resumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resumed_by_host = Arc::clone(&resumed);
        let (authorized_tx, mut authorized_rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge_for_host = bridge.clone();
        let fake = tokio::spawn(async move {
            ui_tx
                .send(AuthUiEvent::Offer {
                    generation: 70,
                    offer: offer(),
                })
                .unwrap();
            let method_id = match decisions.recv().await {
                Some(AuthUserDecision::Authorize {
                    generation: 70,
                    method_id,
                }) => method_id,
                other => panic!("expected explicit authorization, got {other:?}"),
            };
            authorized_tx.send(()).unwrap();
            let attempt_id = SessionOpenId::new(701);
            ui_tx
                .send(AuthUiEvent::Starting {
                    generation: 70,
                    attempt_id,
                    method_id,
                })
                .unwrap();
            let (control, mut control_rx) = AuthControl::channel_for_mode(AuthMode::Loopback);
            assert!(bridge_for_host.set_control(70, control));
            ui_tx
                .send(AuthUiEvent::Challenge {
                    generation: 70,
                    challenge: challenge(701, AuthMode::Loopback),
                })
                .unwrap();
            match control_rx.recv().await {
                Some(umadev_host::session_bootstrap::AuthCommand::SubmitCode(code)) => {
                    assert_eq!(code.reveal(), "callback-secret");
                }
                other => panic!("expected a sensitive callback code, got {other:?}"),
            }
            resumed_by_host.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ui_tx.send(AuthUiEvent::Clear { generation: 70 }).unwrap();
        });

        let AuthUiEvent::Offer { generation, offer } = ui_rx.recv().await.unwrap() else {
            panic!("fake host must begin with an offer");
        };
        let mut state = AuthUiState::new(generation, offer);
        assert!(
            authorized_rx.try_recv().is_err(),
            "offer presentation is inert"
        );
        assert!(matches!(
            state.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            AuthUiEffect::None
        ));
        assert!(
            authorized_rx.try_recv().is_err(),
            "method selection is not consent"
        );
        let AuthUiEffect::Authorize {
            generation,
            method_id,
        } = state.handle_key(KeyCode::Enter, KeyModifiers::NONE)
        else {
            panic!("the second explicit Enter should confirm");
        };
        assert!(bridge.authorize(generation, method_id));
        assert_eq!(
            authorized_rx.recv().await,
            Some(()),
            "only explicit confirmation authorizes the fake host"
        );

        assert!(state.apply_event(ui_rx.recv().await.unwrap()));
        assert!(state.apply_event(ui_rx.recv().await.unwrap()));
        assert!(matches!(
            state.handle_key(KeyCode::Char('x'), KeyModifiers::NONE),
            AuthUiEffect::None
        ));
        state.handle_key(KeyCode::Char('i'), KeyModifiers::NONE);
        state.handle_paste("callback-secret");
        let AuthUiEffect::SubmitCode { generation, code } =
            state.handle_key(KeyCode::Enter, KeyModifiers::NONE)
        else {
            panic!("manual callback code should cross one sensitive boundary");
        };
        bridge.submit_code(generation, code).unwrap();
        fake.await.unwrap();
        assert!(matches!(
            ui_rx.recv().await,
            Some(AuthUiEvent::Clear { generation: 70 })
        ));
        assert_eq!(resumed.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn cancelled_auth_restores_original_turn_ahead_of_a_newer_draft() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = crate::app::App::new(
            "auth-restore",
            crate::config::UserConfig::default(),
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        app.input = "newer draft".to_string();
        app.input_cursor = app.input.chars().count();
        app.record_auth_cancelled(
            crate::app::SubmittedTurn::text("original turn".to_string()),
            "cancelled".to_string(),
        );
        assert_eq!(app.input, "original turn\nnewer draft");
    }
}
