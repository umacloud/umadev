//! TUI application model.
//!
//! 4.4+ design (Claude Code-style):
//!
//! - **Picker** — shown only on first launch (no `~/.umadev/config.toml`).
//!   Up/Down through detected backends, Enter to confirm, choice saved.
//! - **Chat** — the main screen. Persistent input box at the bottom,
//!   scrolling message history above (user / umadev / host outputs /
//!   gate prompts), status bar on top.
//!
//! Slash commands inside Chat (`/claude` `/codex` `/opencode` `/offline`
//! `/init` `/continue` `/revise` `/diff` `/spec` `/verify`
//! `/doctor` `/help` `/quit` `/clear` `/history` `/commands`) plus normal
//! text.
//!
//! Plain text is NOT classified by the shell. When a gate is open it is a
//! gate reply (approve / revise); otherwise it is routed to the selected
//! **base** (Claude Code / Codex / `OpenCode`), which decides
//! for itself whether the message is conversation or a build request and
//! replies accordingly — UmaDev is only the shell around that base. The
//! running dialogue is kept in [`App::conversation`] and handed to the base
//! on every turn, so chat has memory instead of being amnesiac one-shots.

use std::collections::VecDeque;

use crossterm::event::KeyCode;
use umadev_agent::{EngineEvent, Gate};
use umadev_spec::{Phase, PHASE_CHAIN};

use crate::config::UserConfig;

/// Max lines kept in the chat history (older lines roll off).
const HISTORY_CAP: usize = 1000;
/// Max conversation-memory messages handed to the base per routed turn.
/// Bounds prompt growth (≈ the last 8 user/assistant exchanges) while keeping
/// enough context for the base to follow a multi-turn dialogue.
const CONVERSATION_CAP: usize = 16;
/// Max chars in the input box.
const INPUT_CAP: usize = 8192;

/// Which screen the TUI is showing.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AppMode {
    /// First-launch guided setup (a small stepped wizard — see [`PickerStep`]).
    Picker,
    /// The conversational main screen.
    Chat,
}

/// The steps of the first-run guided setup, in order: pick a UI language, then
/// pick which logged-in base CLI to drive. umadev is a pure base-CLI driver, so
/// there is no third-party-API or offline choice here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerStep {
    /// Step 1 — choose the UI language.
    Language,
    /// Step 2 — choose which logged-in base CLI to drive.
    BaseCli,
}

impl PickerStep {
    /// 1-based step number for the progress indicator.
    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            PickerStep::Language => 1,
            PickerStep::BaseCli => 2,
        }
    }
}

/// What the event loop should do after a key press.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Action {
    /// Nothing — keep looping.
    None,
    /// Tear down and exit.
    Quit,
    /// Approve the named gate and drive the next block.
    Continue(Gate),
    /// User submitted a fresh requirement — start the initial pipeline block.
    StartRun(String),
    /// `/quick <task>` — run the lightweight fast track (spec-lite -> implement
    /// -> quality, no gates) for a trivial change instead of the full pipeline.
    StartQuick(String),
    /// `/redo <phase>` — re-run a single named phase using the prior run's
    /// context (handy for a phase that degraded because the base went offline).
    RedoPhase(Phase),
    /// User submitted natural language — ask the selected worker to decide
    /// whether this is normal chat or a pipeline requirement.
    Route(String),
    /// User submitted text while a gate was active — record as a revision and
    /// re-run the most recent block.
    Revise(String),
    /// `/cancel` — abort the in-flight pipeline task and return to the prompt
    /// (without quitting the app). The event loop owns the run task handle.
    Cancel,
    /// Backend was switched (saved to config); the engine task should be
    /// restarted on next `StartRun`.
    BackendChanged,
    /// `/setup` — re-open the first-run guide (language + worker picker). The
    /// event loop re-probes the host CLIs so their ready-state is current.
    Reconfigure,
    /// `/preview` — start the dev server in the background so the recorded
    /// Preview URL is live, then open the browser. The event loop owns the
    /// child handle (it lives in `App::preview_server`).
    StartPreview {
        /// The Preview URL the worker recorded.
        url: String,
        /// The exact command to start the dev server (e.g. `cd web && npm run dev`).
        command: String,
    },
    /// `/deploy` — run the deploy command the worker recorded to ship the
    /// project. Runs in the foreground (deploys need interactive CLI login).
    RunDeploy {
        /// The exact deploy command (e.g. `npx vercel --prod`).
        command: String,
    },
    /// `/mouse` toggle — (de)activate mouse capture on the LIVE terminal. The
    /// event loop owns the `terminal`, so the actual `EnableMouseCapture` /
    /// `DisableMouseCapture` escape must be issued there, not from the app
    /// model. `true` = capture on (wheel scrolls), `false` = capture off
    /// (native click-drag text selection restored). Without this the toggle
    /// only flipped a bool and never released the real capture.
    SetMouseCapture(bool),
}

/// Status of one pipeline phase.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PhaseStatus {
    /// Not reached yet.
    Pending,
    /// Currently executing.
    Running,
    /// Finished.
    Done,
}

/// One row in the pipeline status panel (kept compact for the status bar).
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PhaseRow {
    /// The phase this row tracks.
    pub phase: Phase,
    /// Its current status.
    pub status: PhaseStatus,
}

/// Availability of one host backend.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BackendInfo {
    /// Stable backend id (`claude-code` / `codex` / `opencode`).
    pub id: String,
    /// `true` when the host CLI is installed and reachable.
    pub ready: bool,
    /// Version string or failure reason.
    pub detail: String,
}

/// Which group a picker item belongs to — drives the section headers in the
/// first-launch picker so a user sees the three runtime paths at a glance.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PickerGroup {
    /// First-run UI language choice (zh-CN / zh-TW / en), rendered first.
    Language,
    /// Drive a logged-in host CLI subprocess (Claude Code / Codex / `OpenCode`).
    HostCli,
}

/// One item in the first-launch backend picker.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PickerItem {
    /// `Some("claude-code")` etc. `None` represents the `offline` choice
    /// or the custom-API wizard entry (disambiguated by [`PickerItem::group`]).
    pub backend_id: Option<String>,
    /// Display label.
    pub label: String,
    /// Probe state — `Ready` for offline always; for hosts only when CLI is on PATH.
    pub ready: bool,
    /// Detail line (version / "not on PATH" / "deterministic templates").
    pub detail: String,
    /// Which section this item renders under.
    pub group: PickerGroup,
    /// `Some` only for the first-run language items — selecting one switches
    /// the UI language live and stays on the picker (it does not commit a
    /// backend or leave the picker).
    pub lang: Option<umadev_i18n::Lang>,
}

/// Source of a chat message — used to colour the role label.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ChatRole {
    /// The end user typing into the input box.
    You,
    /// UmaDev's own meta-messages (pipeline progress, gate prompts).
    UmaDev,
    /// One line of output captured from a host CLI worker.
    Host,
    /// The pipeline reached a gate and is awaiting approval.
    Gate,
    /// A system event (config saved, error, hint).
    System,
}

/// One row in the chat history.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ChatMessage {
    /// Who "said" this.
    pub role: ChatRole,
    /// The text body (already cleaned of ANSI etc.).
    pub body: String,
}

/// A scrollable full-screen overlay opened by `/spec` / `/verify` /
/// `/doctor` / `/diff`. Closed with Esc.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Overlay {
    /// Window title shown at the top of the overlay border.
    pub title: String,
    /// Pre-split lines for easy clipping (each may be longer than the
    /// visible width; the renderer wraps).
    pub lines: Vec<String>,
    /// Top-of-window cursor (0 = first line).
    pub scroll: usize,
}

impl Overlay {
    /// Build an overlay from a single body string.
    #[must_use]
    pub fn from_body(title: impl Into<String>, body: &str) -> Self {
        let lines: Vec<String> = body.lines().map(String::from).collect();
        Self {
            title: title.into(),
            lines,
            scroll: 0,
        }
    }

    /// Scroll down by `n` lines, clamped at end.
    pub fn scroll_down(&mut self, n: usize) {
        let max = self.lines.len().saturating_sub(1);
        self.scroll = (self.scroll + n).min(max);
    }

    /// Scroll up by `n` lines, clamped at 0.
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
    }
}

/// The whole TUI state.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    /// Active screen.
    pub mode: AppMode,

    /// Persisted user config (backend choice etc.).
    pub config: UserConfig,
    /// Path the picker / slash commands write back to.
    pub config_path: std::path::PathBuf,

    /// Current UI language (zh-CN / zh-TW / en). Resolved from config on launch
    /// (system-detected on first run); switchable at runtime via `/lang`.
    pub lang: umadev_i18n::Lang,

    /// Current step of the first-run guided setup (see [`PickerStep`]).
    pub picker_step: PickerStep,

    /// Picker state (active during `AppMode::Picker`).
    pub picker_items: Vec<PickerItem>,
    /// Cursor in `picker_items`.
    pub picker_selected: usize,
    /// Inline notice shown in the picker footer (e.g. "claude-code 未安装")
    /// so rejecting an un-ready host gives visible feedback ON the picker.
    pub picker_notice: Option<String>,

    /// Chat input buffer (UTF-8 String — mutate via cursor helpers,
    /// never via raw push/pop, so multi-byte chars stay intact).
    pub input: String,
    /// Caret position within `input`, measured in **characters** (not bytes).
    /// `0` = before first char; `chars().count()` = after last char.
    pub input_cursor: usize,
    /// Past submitted texts. ↑↓ in an empty input box recalls them.
    pub input_history: VecDeque<String>,
    /// Recall cursor into `input_history`; `None` = editing a fresh draft.
    pub input_history_idx: Option<usize>,
    /// When `input` starts with `/` and matches command verbs, this is
    /// the highlight in the slash-command palette popover.
    pub palette_selected: usize,

    /// Bounded scrolling chat history (older lines roll off).
    pub history: VecDeque<ChatMessage>,

    /// **Transcript scrollback** — how many wrapped rows the user has scrolled
    /// UP from the bottom. `0` = pinned to the bottom (the default, auto-sticky
    /// as new content arrives). Any positive value means the user is reviewing
    /// history, so the renderer STOPS auto-sticking to the bottom until they
    /// return (End / scroll back down to 0). Clamped each frame against
    /// [`Self::transcript_max_scroll`].
    pub transcript_scroll: usize,
    /// The maximum the transcript can scroll up (= rows hidden above the
    /// viewport), recomputed by the renderer every frame from the CURRENT
    /// width/height. Interior-mutable so the pure `render` fn can publish it for
    /// the key handlers to clamp [`Self::transcript_scroll`] against. `0` when
    /// everything fits (nothing to scroll).
    pub transcript_max_scroll: std::cell::Cell<usize>,
    /// Rows that fit in the transcript viewport, published by the renderer so
    /// the half-page (Ctrl-U/D) and full-page (PageUp/Down) scroll steps match
    /// the actual visible height instead of a guessed constant.
    pub transcript_viewport_rows: std::cell::Cell<usize>,
    /// `true` when the mouse-wheel → transcript-scroll binding is active. A
    /// `/mouse` toggle flips it: capturing the mouse takes over the terminal's
    /// native text selection, so a user who wants to select/copy turns it off
    /// (or holds Shift, which most terminals route around the capture). Default
    /// on. Read by the event loop; the value itself lives here so it survives
    /// across redraws.
    pub mouse_scroll: bool,

    /// **Conversation memory** — the multi-turn transcript handed to the base
    /// on every routed turn so chat is a real conversation, not a sequence of
    /// amnesiac one-shots. Holds ONLY genuine chat turns (user message + base
    /// reply), never pipeline progress noise, so the base sees a clean dialogue
    /// when it decides "chat vs. run" and when it answers conversationally.
    /// Bounded to the most recent [`CONVERSATION_CAP`] turns.
    pub conversation: Vec<umadev_runtime::Message>,

    /// `true` once a host-CLI base has handled at least one chat turn in the
    /// current session. Tells the next routed turn to **resume** that base's
    /// own conversation (`claude --continue` etc.) instead of starting cold —
    /// this is what gives chat real memory for `HostCli` bases. Reset when the
    /// session context breaks: `/clear`, switching backend, or a new
    /// pipeline run. Ignored by `Offline` and bases without a session id.
    pub host_chat_session_active: bool,

    /// A stable UUID pinning THIS chat's base session, minted lazily on the
    /// first host-CLI turn. Lets `claude` resume our own conversation by id
    /// (`--session-id` / `--resume <uuid>`) instead of "the most recent in this
    /// dir", so a parallel `claude` session in the same folder can't bleed in.
    /// Reset (to `None`) at the same points as [`Self::host_chat_session_active`].
    pub chat_session_id: Option<String>,

    /// Currently active backend id (matches `config.backend`).
    /// `None` means offline / no host CLI.
    pub backend: Option<String>,
    /// Display label for the worker — `claude-code` / `codex` / `offline`.
    pub backend_label: String,

    /// Workspace slug (filled in by the caller).
    pub slug: String,
    /// The active requirement once the pipeline starts.
    pub requirement: String,

    /// Phase progress, in `PHASE_CHAIN` order.
    pub phases: Vec<PhaseRow>,
    /// The gate the pipeline is currently paused at, if any.
    pub active_gate: Option<Gate>,
    /// `true` once a delivery proof-pack has landed.
    pub finished: bool,
    /// `true` once the user has kicked off a pipeline run in this session.
    pub run_started: bool,
    /// `true` when the LAST block ended by aborting (returned an error → zero
    /// phases produced) rather than reaching a gate or delivery. Set from the
    /// `ABORT_SENTINEL` terminal note emitted by `spawn_block`; surfaced in the
    /// status bar as an explicit "aborted" terminal state so a wedged run no
    /// longer masquerades as idle "ready / 0/9". Cleared when a new run starts.
    pub aborted: bool,

    /// Whether the cold-start greeting has already been shown this session.
    /// Makes `push_greeting` idempotent so re-entering chat (e.g. picking a base
    /// again via `/setup`) never stacks a second welcome banner on the transcript.
    pub greeted: bool,

    /// A routed chat turn is in flight (message sent, waiting on the base's
    /// reply). Drives the animated "thinking…" status so a submit never looks
    /// frozen. Cleared when the reply / run decision / error lands.
    pub thinking: bool,
    /// When the current thinking turn began — for the live elapsed readout.
    pub thinking_started: Option<std::time::Instant>,
    /// A tools-enabled agentic execution call (the SECOND call after an
    /// `agentic` route classification) is streaming — the base is reading /
    /// running / editing in its own tool loop. Drives Ctrl-C: an interrupt
    /// while this is set must ABORT the backing `run_task`, not just stop the
    /// (fire-and-forget) chat-route spinner. Cleared on every terminal agentic
    /// outcome (done / failed / cancel).
    pub agentic_in_flight: bool,
    /// Session-level override for `auto_approve_gates` set via `/manual`
    /// (`Some(false)`) or `/auto` (`Some(true)`). `None` → use the project's
    /// `.umadevrc` value. Lets the user flip review mode mid-session without
    /// hand-editing config or losing it on restart-of-flow.
    ///
    /// Kept as the compatibility surface for the binary `/auto` `/manual`
    /// toggle; [`trust_mode_override`] is the richer three-tier control that
    /// supersedes it. The two stay consistent — flipping one updates the other.
    pub auto_approve_override: Option<bool>,

    /// Session-level trust / autonomy tier override (`/mode plan|guarded|auto`).
    /// `None` → derive from `.umadevrc` (`auto_approve_gates`). When `Some`, it
    /// takes precedence and also drives the legacy [`auto_approve_override`].
    /// The default tier is `guarded` (the existing human-in-the-loop behaviour).
    pub trust_mode_override: Option<umadev_agent::TrustMode>,

    /// Per-project collaborative trust ledger (`.umadev/trust.json`). Records
    /// how many times in a row each gate passed; after a threshold it *suggests*
    /// (never auto-applies) letting that gate auto-advance. Fail-open.
    pub trust_ledger: umadev_agent::TrustLedger,

    /// Detected host backends (asynchronously populated).
    pub backends: Vec<BackendInfo>,
    /// `true` while the help overlay is open.
    pub show_help: bool,
    /// Scroll offset (in rows) for the help overlay, so it never crops on
    /// small terminals. Reset to 0 each time help opens.
    pub help_scroll: u16,
    /// A scrollable overlay (from `/spec` / `/verify` / `/doctor` /
    /// `/diff`). When `Some`, key input is routed to the overlay
    /// (scroll, close); when `None`, normal chat input.
    pub overlay: Option<Overlay>,
    /// Handle to a running dev-server subprocess spawned by `/preview`, so we
    /// can kill it on `/stop-preview` or quit. `None` when no preview is live.
    pub preview_server: std::sync::Arc<std::sync::Mutex<Option<tokio::process::Child>>>,
    /// Workspace root — surfaced in the status bar as a breadcrumb.
    pub project_root: std::path::PathBuf,
    /// When a pipeline is running and the user presses `q` / Esc, we
    /// stash a "press again to confirm" flag instead of quitting
    /// immediately. Cleared on any other keypress.
    pub pending_quit_confirm: bool,

    /// One-line status shown in the top bar.
    pub status: String,
    /// Spinner animation tick.
    pub tick: u8,
    /// `true` when the user asked to quit.
    pub should_quit: bool,

    /// Wall-clock start of the current running block. Drives the live
    /// `[m:ss]` elapsed counter in the status bar so long worker calls
    /// don't read as "frozen". `None` when nothing is running.
    pub run_started_at: Option<std::time::Instant>,
    /// Wall-clock start of the currently running phase. Reset on every
    /// `PhaseStarted` so the status bar can show per-phase elapsed time.
    pub phase_started_at: Option<std::time::Instant>,

    /// When `auto_approve_gates` is on and a gate just opened, this holds
    /// the gate to auto-continue. The event loop picks it up right after
    /// `apply_engine_event` returns and fires `Action::Continue`.
    pub pending_auto_continue: Option<Gate>,

    /// Messages the user typed WHILE the pipeline was mid-phase. We can't
    /// inject them into the running base subprocess, so (like Claude Code queuing
    /// a turn) we hold them until the next gap — a gate / phase boundary. FIFO:
    /// each extra steer the user types while a phase runs is appended, and all of
    /// them are folded into the next gate's revision in order. A single `Option`
    /// here used to silently OVERWRITE an earlier un-fired steer (and the
    /// `queued N` chip stayed stuck at 1), losing input.
    pub queued_steer: VecDeque<String>,

    /// Set by the gate handler when a [`queued_steer`] message is ready to fire
    /// at the just-opened gate. The event loop consumes it and re-runs the
    /// producing block with the queued text folded in as a revision.
    pub pending_steer: Option<String>,

    /// Chat turns the user typed WHILE a routed turn was still in flight
    /// (`thinking == true`). We never spawn a second `spawn_route` against the
    /// same chat session concurrently — that would resume one `session_id` in
    /// two base subprocesses at once and scramble the reply order / memory.
    /// Instead each extra turn is parked here (FIFO) and the event loop fires
    /// the next one only after the current route result lands. Kept distinct
    /// from [`queued_steer`], which is the *pipeline-run* queue (fires at a
    /// gate), not the chat-routing queue.
    pub queued_chat: std::collections::VecDeque<String>,

    /// **Streaming throttle** — tracks the last tool-use name + count so
    /// consecutive same-type tool calls (e.g. 10 × Read) collapse into one
    /// line `[read] Read (10): file1, file2, …` instead of flooding the chat.
    /// Reset when a different tool or a non-tool event arrives.
    pub stream_tool_batch: Option<(String, u32)>,
    /// **Streaming text append** — when true, the next `Text` delta appends
    /// to the last Host message (typewriter effect) instead of pushing a new
    /// line. Set false by any non-text event (tool use, result, etc.).
    pub stream_text_active: bool,

    /// Wall-clock of the LAST sign of life from the base — any worker stream
    /// event, host output line, or progress note. Drives the honest "stall"
    /// signal: when a phase is running but nothing has arrived for >3s (and no
    /// tool call is mid-flight), the status spinner is painted red so the user
    /// sees a truthful "about to stall" cue instead of a fake-smooth spinner.
    /// Refreshed to `now` on every such event; `None` when nothing is running.
    pub last_output_at: Option<std::time::Instant>,
    /// `true` while a worker tool call is in flight (a `ToolUse` arrived but its
    /// `ToolResult` hasn't yet). A long tool call (a 40s `npm install`) is NOT a
    /// stall — the base IS working — so the red signal is suppressed while this
    /// is set even past the 3s threshold.
    pub tool_in_progress: bool,
}

impl App {
    /// Build a fresh app. Reads existing config from disk; if no
    /// backend is set, opens on the picker.
    ///
    /// `project_root` is shown in the status bar and used by overlays
    /// like `/diff` that need to read workspace artifacts.
    #[must_use]
    pub fn new(
        slug: impl Into<String>,
        config: UserConfig,
        config_path: std::path::PathBuf,
        project_root: std::path::PathBuf,
    ) -> Self {
        let phases = PHASE_CHAIN
            .iter()
            .map(|&phase| PhaseRow {
                phase,
                status: PhaseStatus::Pending,
            })
            .collect();
        let backend = config.backend.clone().filter(|b| b != "offline");
        let backend_label = backend.clone().unwrap_or_else(|| "offline".to_string());
        let lang = config.resolved_lang();
        umadev_i18n::set_lang(lang);
        // Export per-phase model tiers (if configured) so the in-process worker
        // loop drives each phase with the right-sized model.
        config.apply_model_tiers();
        let mode = if config.has_backend() {
            AppMode::Chat
        } else {
            AppMode::Picker
        };
        let mut app = Self {
            mode,
            config,
            config_path,
            lang,
            picker_step: PickerStep::Language,
            picker_items: step_items(PickerStep::Language, lang, &[]),
            picker_selected: lang as usize,
            picker_notice: None,
            input: String::new(),
            input_cursor: 0,
            input_history: VecDeque::new(),
            input_history_idx: None,
            palette_selected: 0,
            history: VecDeque::new(),
            transcript_scroll: 0,
            transcript_max_scroll: std::cell::Cell::new(0),
            transcript_viewport_rows: std::cell::Cell::new(0),
            mouse_scroll: true,
            conversation: Vec::new(),
            host_chat_session_active: false,
            chat_session_id: None,
            backend,
            backend_label,
            slug: slug.into(),
            requirement: String::new(),
            phases,
            active_gate: None,
            finished: false,
            run_started: false,
            aborted: false,
            greeted: false,
            thinking: false,
            thinking_started: None,
            agentic_in_flight: false,
            auto_approve_override: None,
            trust_mode_override: None,
            trust_ledger: umadev_agent::TrustLedger::load(&project_root),
            backends: Vec::new(),
            show_help: false,
            help_scroll: 0,
            overlay: None,
            preview_server: std::sync::Arc::new(std::sync::Mutex::new(None)),
            project_root,
            pending_quit_confirm: false,
            status: String::new(),
            tick: 0,
            should_quit: false,
            run_started_at: None,
            phase_started_at: None,
            pending_auto_continue: None,
            queued_steer: VecDeque::new(),
            pending_steer: None,
            queued_chat: std::collections::VecDeque::new(),
            stream_tool_batch: None,
            stream_text_active: false,
            last_output_at: None,
            tool_in_progress: false,
        };
        app.load_history();
        if app.mode == AppMode::Chat {
            app.push_greeting();
            app.maybe_push_resume_hint();
        }
        app.refresh_status();
        app
    }

    /// Resolve which "brain" runs the pipeline: the selected base CLI, or the
    /// offline template fallback when no base is set.
    #[must_use]
    pub fn brain_spec(&self) -> crate::BrainSpec {
        if let Some(id) = &self.backend {
            if !id.is_empty() && id != "offline" {
                return crate::BrainSpec::HostCli(id.clone());
            }
        }
        crate::BrainSpec::Offline
    }

    /// The model the Agent runs on. UmaDev owns NO model — the base's model IS
    /// the engine. Precedence: an explicit `/model` override > the model SYNCED
    /// from the selected base's own config (`detect_base_model`) > empty (the
    /// base then uses its login / server default, which we never override). The
    /// resolved value is what the drivers receive; empty makes them skip
    /// `--model`, so the base is driven on exactly its own model.
    #[must_use]
    pub fn effective_model(&self) -> String {
        if let Some(m) = self.config.model.as_deref() {
            if !m.trim().is_empty() {
                return m.to_string();
            }
        }
        if let Some(b) = self.backend.as_deref() {
            if !b.is_empty() && b != "offline" {
                if let Some(m) = crate::detect_base_model(b, &self.project_root) {
                    return m;
                }
            }
        }
        String::new()
    }

    fn history_path(&self) -> std::path::PathBuf {
        self.project_root.join(".umadev").join("input-history.txt")
    }

    fn load_history(&mut self) {
        if let Ok(body) = std::fs::read_to_string(self.history_path()) {
            for line in body.lines().rev().take(50) {
                if !line.is_empty() {
                    self.input_history.push_front(line.to_string());
                }
            }
        }
    }

    fn persist_history(&self) {
        let path = self.history_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let lines: Vec<&str> = self.input_history.iter().map(String::as_str).collect();
        let _ = std::fs::write(path, lines.join("\n"));
    }

    /// If a `.umadev/workflow-state.json` exists in the workspace
    /// (meaning a prior session left the pipeline mid-flight), surface
    /// it as a system message so the user can resume with `/continue`
    /// instead of staring at a fresh prompt and wondering "did my
    /// previous work disappear?".
    fn maybe_push_resume_hint(&mut self) {
        let Some(state) = umadev_agent::read_workflow_state(&self.project_root) else {
            return;
        };
        let gate = state.active_gate.clone();
        let req = state.requirement.clone();
        if gate.is_empty() && state.phase == "delivery" {
            // Last session completed; mention the proof-pack but don't
            // nudge a resume.
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "session.completed", &[&req]),
            );
            return;
        }
        if !gate.is_empty() {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "session.paused_at_gate", &[&gate, &req]),
            );
        } else if !req.is_empty() {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "session.unfinished", &[&state.phase, &req]),
            );
        }
    }

    fn push(&mut self, role: ChatRole, body: impl Into<String>) {
        self.history.push_back(ChatMessage {
            role,
            body: body.into(),
        });
        while self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
    }

    fn push_greeting(&mut self) {
        // Idempotent: show the welcome banner at most once per session. Both the
        // cold-start path and the picker-commit path (`/setup`) call this; without
        // the guard a re-pick stacked a duplicate banner on the transcript.
        if self.greeted {
            return;
        }
        self.greeted = true;
        // Value-first: lead with the OUTCOME, not the architecture/config.
        // Localized (zh-CN / zh-TW / en) via the i18n catalog.
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(self.lang, "greeting.main", &[&self.backend_label]),
        );
        // The immediate call-to-action: a curated example menu + the single
        // next thing to do. Progressive disclosure — no MCP/Skill/Knowledge
        // noise on a cold start; that lives behind /help once the user is in.
        self.push(
            ChatRole::System,
            umadev_i18n::t(self.lang, "greeting.examples").to_string(),
        );
    }

    fn refresh_status(&mut self) {
        // Compact phase progress bar, e.g. "●●◐○○○○○○ 2/9" — geometric glyphs
        // read far better at a glance than bracket-tags and stay emoji-free.
        let done = self
            .phases
            .iter()
            .filter(|r| r.status == PhaseStatus::Done)
            .count();
        let total = self.phases.len();
        let bar: String = self
            .phases
            .iter()
            .map(|r| match r.status {
                PhaseStatus::Done => '●',
                // The running circle ROTATES (◐◓◑◒) so the bar itself proves
                // motion — not a static ◐ that reads as frozen.
                PhaseStatus::Running => self.running_circle(),
                PhaseStatus::Pending => '○',
            })
            .collect();
        let dots = if total > 0 {
            format!("{bar} {done}/{total}")
        } else {
            bar
        };
        let running = self
            .phases
            .iter()
            .find(|r| r.status == PhaseStatus::Running)
            .map(|r| {
                // Live per-phase elapsed so a multi-minute worker call
                // visibly advances instead of looking frozen.
                let elapsed = self
                    .phase_started_at
                    .map(|t| format!(" {}", fmt_elapsed(t.elapsed().as_secs())))
                    .unwrap_or_default();
                format!(" {} {}{elapsed}", self.spinner(), r.phase.id())
            })
            .unwrap_or_default();
        let gate_label = self
            .active_gate
            .map(|g| format!(" · [gate] {}", g.id_str()))
            .unwrap_or_default();
        let done_label = if self.finished {
            " · [ok] delivered".to_string()
        } else if self.aborted {
            // Explicit terminal state — never let an aborted run read as idle.
            " · [aborted] 本轮已中止".to_string()
        } else {
            String::new()
        };
        let ds_short = self
            .config
            .design_system
            .as_deref()
            .map(|s| format!(" · [design] {s}"))
            .unwrap_or_default();
        // Total wall-clock since the block started, shown while running so
        // the user has a clear "it's been N minutes" signal.
        let total_elapsed = self
            .run_started_at
            .filter(|_| !self.finished && self.active_gate.is_none())
            .map(|t| format!(" · [time] {}", fmt_elapsed(t.elapsed().as_secs())))
            .unwrap_or_default();
        self.status = format!(
            "● {} · {}{}{}{}{}{}",
            self.backend_label, dots, running, gate_label, done_label, ds_short, total_elapsed
        );
    }

    /// `true` while the user has started a run that hasn't reached
    /// delivery yet. Used by the Esc-to-quit confirmation.
    ///
    /// An **aborted** block is NOT active: the run is over (it produced zero
    /// phases and bailed), so it must not keep claiming the workspace or block a
    /// fresh `/run`. Without this, an aborted run would stay "active" forever and
    /// a retry would be wrongly refused as "a pipeline is already running".
    #[must_use]
    pub fn is_pipeline_active(&self) -> bool {
        self.run_started && !self.finished && !self.aborted
    }

    /// Mark the current block as **aborted**: it ended with an error before
    /// producing any phase, so there is no gate and no delivery. Render the
    /// explicit "this round aborted" line, drop the run out of the active state,
    /// and stop the live elapsed counters so the status bar reflects a real
    /// terminal state instead of the misleading idle "ready / 0/9" look. A new
    /// `/run` (which fires `PipelineStarted`) clears the flag.
    fn mark_block_aborted(&mut self, body: String) {
        self.aborted = true;
        self.active_gate = None;
        self.run_started_at = None;
        self.phase_started_at = None;
        // The run is over — any worker-stall animation must stop.
        self.thinking = false;
        self.tool_in_progress = false;
        self.last_output_at = None;
        self.push(ChatRole::System, body);
        self.refresh_status();
    }

    // ---- transcript scrollback -------------------------------------------
    //
    // `transcript_scroll` is the number of wrapped rows the user has scrolled
    // UP from the bottom (0 = pinned to bottom). The renderer publishes the
    // current upper bound into `transcript_max_scroll` every frame, so these
    // helpers clamp against the real, width-aware overflow instead of guessing.

    /// Scroll the transcript UP by `rows` (toward older history). Any non-zero
    /// scroll makes the renderer STOP auto-sticking to the bottom.
    pub fn transcript_scroll_up(&mut self, rows: usize) {
        let max = self.transcript_max_scroll.get();
        self.transcript_scroll = self.transcript_scroll.saturating_add(rows).min(max);
    }

    /// Scroll the transcript DOWN by `rows` (toward the newest content). Hitting
    /// `0` re-pins to the bottom and re-enables auto-stick.
    pub fn transcript_scroll_down(&mut self, rows: usize) {
        self.transcript_scroll = self.transcript_scroll.saturating_sub(rows);
    }

    /// Jump to the very top of the transcript (oldest content on screen).
    pub fn transcript_scroll_to_top(&mut self) {
        self.transcript_scroll = self.transcript_max_scroll.get();
    }

    /// Jump back to the bottom (newest content) and re-enable auto-stick.
    pub fn transcript_scroll_to_bottom(&mut self) {
        self.transcript_scroll = 0;
    }

    /// Half the transcript viewport, for Ctrl-U / Ctrl-D — at least one row so a
    /// tiny window still scrolls.
    fn transcript_half_page(&self) -> usize {
        (self.transcript_viewport_rows.get() / 2).max(1)
    }

    /// A full transcript viewport (minus one row of overlap, the convention for
    /// PageUp / PageDown), for the page keys — at least one row.
    fn transcript_page(&self) -> usize {
        self.transcript_viewport_rows.get().saturating_sub(1).max(1)
    }

    // ---- input editing helpers (char-cursor over a UTF-8 String) ---------

    /// Number of characters in the input buffer.
    #[must_use]
    pub fn input_len(&self) -> usize {
        self.input.chars().count()
    }

    /// Convert a char-position cursor to a byte index into `input`.
    /// Used to splice multi-byte UTF-8 strings safely.
    fn byte_index(&self, char_pos: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_pos)
            .map_or(self.input.len(), |(i, _)| i)
    }

    /// Insert one character at the cursor and advance.
    pub fn insert_at_cursor(&mut self, c: char) {
        if self.input_len() >= INPUT_CAP {
            return;
        }
        let pos = self.byte_index(self.input_cursor);
        self.input.insert(pos, c);
        self.input_cursor += 1;
        // The slash palette re-filters as you type — reset the highlight to the
        // best (first) match so Enter runs a predictable command.
        self.palette_selected = 0;
    }

    /// Insert a whole string at the cursor (bracketed paste / CJK IME commit).
    /// Newlines are kept (multi-line prompts); other control characters are
    /// dropped so a pasted terminal escape sequence can't corrupt the buffer or
    /// the render. Honors [`INPUT_CAP`] and advances the char-cursor by the
    /// number of characters actually inserted.
    pub fn insert_str_at_cursor(&mut self, text: &str) {
        for c in text.chars() {
            if c != '\n' && c.is_control() {
                continue;
            }
            if self.input_len() >= INPUT_CAP {
                break;
            }
            let pos = self.byte_index(self.input_cursor);
            self.input.insert(pos, c);
            self.input_cursor += 1;
        }
        self.input_history_idx = None;
        self.palette_selected = 0;
    }

    /// Delete the character BEFORE the cursor (Backspace).
    pub fn backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let end = self.byte_index(self.input_cursor);
        let start = self.byte_index(self.input_cursor - 1);
        self.input.replace_range(start..end, "");
        self.input_cursor -= 1;
        self.palette_selected = 0;
    }

    /// Delete the character AT the cursor (forward Delete).
    pub fn forward_delete(&mut self) {
        if self.input_cursor >= self.input_len() {
            return;
        }
        let start = self.byte_index(self.input_cursor);
        let end = self.byte_index(self.input_cursor + 1);
        self.input.replace_range(start..end, "");
    }

    /// Delete from the cursor back to the start of the line (Ctrl+U).
    pub fn delete_to_line_start(&mut self) {
        let start = self.input[..self.byte_index(self.input_cursor)]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        let start_char = self.input[..start].chars().count();
        let end = self.byte_index(self.input_cursor);
        self.input.replace_range(start..end, "");
        self.input_cursor = start_char;
        self.palette_selected = 0;
    }

    /// Delete from the cursor to the end of the line (Ctrl+K).
    pub fn delete_to_line_end(&mut self) {
        let from = self.byte_index(self.input_cursor);
        let end = self.input[from..]
            .find('\n')
            .map_or(self.input.len(), |i| from + i);
        self.input.replace_range(from..end, "");
    }

    /// Delete the word before the cursor (Ctrl+W / Alt+Backspace).
    pub fn delete_word_back(&mut self) {
        let mut c = self.input_cursor;
        // Skip trailing spaces, then the word.
        let ch_at = |app: &Self, i: usize| app.input[..app.byte_index(i + 1)].chars().last();
        while c > 0 && ch_at(self, c - 1).is_some_and(char::is_whitespace) {
            c -= 1;
        }
        while c > 0 && ch_at(self, c - 1).is_some_and(|ch| !ch.is_whitespace()) {
            c -= 1;
        }
        let start = self.byte_index(c);
        let end = self.byte_index(self.input_cursor);
        self.input.replace_range(start..end, "");
        self.input_cursor = c;
        self.palette_selected = 0;
    }

    /// Move cursor by `delta` characters, clamped to `[0, len]`.
    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.input_len();
        if delta < 0 {
            self.input_cursor = self.input_cursor.saturating_sub(delta.unsigned_abs());
        } else {
            #[allow(clippy::cast_sign_loss)]
            let fwd = delta as usize;
            self.input_cursor = self.input_cursor.saturating_add(fwd).min(len);
        }
    }

    /// Clear the input buffer + reset cursor + history-recall index.
    pub fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.input_history_idx = None;
    }

    /// Push a submitted line onto the input-history ring. De-dups
    /// consecutive duplicates (typing the same thing twice doesn't
    /// double-pollute the ↑↓ recall). Also persists to disk so history
    /// survives across TUI sessions.
    pub fn remember_submission(&mut self, text: &str) {
        const HISTORY_CAP_PROMPTS: usize = 100;
        if text.trim().is_empty() {
            return;
        }
        if self.input_history.back().map(String::as_str) == Some(text) {
            return;
        }
        self.input_history.push_back(text.to_string());
        while self.input_history.len() > HISTORY_CAP_PROMPTS {
            self.input_history.pop_front();
        }
        self.persist_history();
    }

    /// Step back through input history. Loads the previous prompt into
    /// the input box. Idempotent at the oldest entry.
    pub fn input_history_back(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let new_idx = match self.input_history_idx {
            None => self.input_history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.input_history_idx = Some(new_idx);
        if let Some(s) = self.input_history.get(new_idx) {
            self.input = s.clone();
            self.input_cursor = self.input_len();
        }
    }

    /// Step forward through input history. At the most-recent entry,
    /// stepping forward once more clears the input (returns to fresh draft).
    pub fn input_history_forward(&mut self) {
        let Some(idx) = self.input_history_idx else {
            return;
        };
        if idx + 1 < self.input_history.len() {
            self.input_history_idx = Some(idx + 1);
            if let Some(s) = self.input_history.get(idx + 1) {
                self.input = s.clone();
                self.input_cursor = self.input_len();
            }
        } else {
            self.input_history_idx = None;
            self.input.clear();
            self.input_cursor = 0;
        }
    }

    // ---- slash command palette ------------------------------------------

    /// Verbs the palette popover suggests, in display order. (verb, hint)
    pub const SLASH_VERBS: &'static [(&'static str, &'static str)] = &[
        ("claude", "switch worker to Claude Code CLI"),
        ("codex", "switch worker to Codex CLI"),
        ("opencode", "switch worker to OpenCode CLI"),
        ("offline", "switch worker to offline templates"),
        ("model", "set the model id (e.g. /model claude-opus-4-7)"),
        ("lang", "switch UI language: /lang [zh-CN|zh-TW|en]"),
        (
            "setup",
            "re-open the first-run guide (language + base picker)",
        ),
        (
            "guide",
            "back to the first-run guide / onboarding (alias of /setup)",
        ),
        ("preview", "start the dev server + open the browser"),
        ("stop-preview", "stop the running preview dev server"),
        ("deploy", "run the recorded deploy command to go live"),
        (
            "pr",
            "open a GitHub PR with the review report + proof-pack as the body",
        ),
        ("usage", "show your worker-call usage statistics"),
        ("animations", "toggle spinner animation on/off"),
        ("bug", "collect diagnostics to report a bug"),
        (
            "design",
            "pick a design system (e.g. /design modern-minimal)",
        ),
        (
            "template",
            "pick a seed template (e.g. /template dashboard)",
        ),
        ("run", "start a new run (/run [slug] <requirement>)"),
        (
            "quick",
            "lightweight fast track for a trivial task (/quick <task>)",
        ),
        ("runs", "view run history and phase timing"),
        (
            "cancel",
            "stop the running pipeline and return to the prompt",
        ),
        (
            "redo",
            "re-run the whole requirement, or one phase (/redo [phase])",
        ),
        (
            "checkpoint",
            "snapshot workspace files (/checkpoint [label])",
        ),
        ("rewind", "list/rewind file checkpoints (/rewind [id])"),
        ("config", "show all current configuration"),
        ("init", "write umadev.yaml manifest"),
        (
            "adopt",
            "adopt an EXISTING project (detect stack, index source, derive contract)",
        ),
        ("continue", "approve the active gate"),
        ("revise", "stay at gate, request changes"),
        ("manual", "review each checkpoint before continuing"),
        ("auto", "auto-approve checkpoints (autonomous)"),
        ("mode", "trust tier: /mode plan|guarded|auto"),
        ("status", "show detailed pipeline status"),
        ("export", "export the latest proof-pack"),
        ("knowledge", "list knowledge + design files"),
        ("pitfalls", "show the self-learning pitfalls knowledge base"),
        (
            "lessons",
            "show what UmaDev has learned (pitfalls + proven patterns)",
        ),
        ("mcp", "manage MCP servers (/mcp list)"),
        ("skill", "manage skill packages (/skill list)"),
        ("spec", "show the UMADEV_HOST_SPEC_V1 spec"),
        ("verify", "show workspace conformance"),
        ("doctor", "self-test"),
        ("diff", "show an artifact (default: PRD)"),
        ("history", "show the conversation history"),
        ("changelog", "show CHANGELOG.md"),
        ("version", "show umadev / spec / worker versions"),
        ("help", "show all keybindings"),
        (
            "mouse",
            "toggle mouse-wheel scrolling (off = native text selection)",
        ),
        ("clear", "clear chat history"),
        ("quit", "exit"),
    ];

    /// Match the verbs prefixed by what comes after `/` in the current
    /// input. Empty input or non-slash input → empty list.
    ///
    /// Combines the static [`SLASH_VERBS`] with the dynamic per-backend
    /// verbs (so typing `/go` suggests `/goose`, typing `/am` suggests
    /// `/claude`, `/codex`, etc.) — kept in sync with `BACKEND_IDS`.
    #[must_use]
    pub fn palette_matches(&self) -> Vec<(&'static str, &'static str)> {
        if !self.input.starts_with('/') {
            return Vec::new();
        }
        let typed = self
            .input
            .strip_prefix('/')
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let mut out: Vec<(&'static str, &'static str)> = Self::SLASH_VERBS
            .iter()
            .filter(|(verb, _)| verb.starts_with(typed.as_str()))
            .copied()
            .collect();
        // Skip ids already covered by the static list (the three first-class
        // base CLIs) to avoid duplicate palette rows.
        let known: std::collections::HashSet<&str> = out.iter().map(|(v, _)| *v).collect();
        for (id, hint) in backend_palette_verbs() {
            if !known.contains(id) && id.starts_with(typed.as_str()) {
                out.push((id, hint));
            }
        }
        out
    }

    /// Replace the input with `/{verb} ` (with trailing space so the
    /// user can immediately type args). Called by Tab autocomplete.
    ///
    /// Second-level: if input is `/design ` or `/template ` (verb +
    /// space + partial arg), Tab completes from the available design
    /// systems / seed templates.
    pub fn autocomplete_palette(&mut self) {
        // Second-level arg completion for /design and /template.
        if let Some(arg_completion) = self.try_arg_completion() {
            self.input = arg_completion;
            self.input_cursor = self.input_len();
            return;
        }
        let matches = self.palette_matches();
        if matches.is_empty() {
            return;
        }
        let selected = self.palette_selected.min(matches.len() - 1);
        let verb = matches[selected].0;
        self.input = format!("/{verb} ");
        self.input_cursor = self.input_len();
        self.palette_selected = 0;
    }

    fn try_arg_completion(&self) -> Option<String> {
        let input = self.input.trim_start();
        let (prefix, partial) = if let Some(rest) = input.strip_prefix("/design ") {
            ("/design ", rest.trim())
        } else if let Some(rest) = input.strip_prefix("/template ") {
            ("/template ", rest.trim())
        } else {
            return None;
        };
        let candidates = if prefix == "/design " {
            self.list_design_systems()
        } else {
            self.list_seed_templates()
        };
        if partial.is_empty() {
            candidates.first().map(|c| format!("{prefix}{c}"))
        } else {
            candidates
                .iter()
                .find(|c| c.starts_with(partial))
                .map(|c| format!("{prefix}{c}"))
        }
    }

    /// Move palette highlight up/down (with wrap-around). Called by
    /// ↑↓ when the palette is showing matches.
    pub fn cycle_palette(&mut self, delta: isize) {
        let count = self.palette_matches().len();
        if count == 0 {
            return;
        }
        // Wrap delta into [0, count). No isize casts → no clippy noise.
        if delta < 0 {
            let back = delta.unsigned_abs() % count;
            self.palette_selected = (self.palette_selected + count - back) % count;
        } else {
            #[allow(clippy::cast_sign_loss)]
            let fwd = delta as usize;
            self.palette_selected = (self.palette_selected + fwd) % count;
        }
    }

    fn set_phase(&mut self, phase: Phase, status: PhaseStatus) {
        if let Some(row) = self.phases.iter_mut().find(|r| r.phase == phase) {
            row.status = status;
        }
    }

    // ---- engine events ----------------------------------------------------

    /// Fold one engine event into the chat history + status bar.
    ///
    /// # Panics
    ///
    /// Panics if the internal phase vector is empty (should never happen —
    /// it's initialized in [`new`](Self::new)).
    pub fn apply_engine(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::PipelineStarted { slug, requirement } => {
                self.slug = slug;
                self.requirement.clone_from(&requirement);
                self.run_started = true;
                // A fresh block clears any prior aborted terminal state — this
                // run is live again.
                self.aborted = false;
                self.run_started_at = Some(std::time::Instant::now());
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::tf(self.lang, "run.started", &[&requirement]),
                );
            }
            EngineEvent::PhaseStarted { phase } => {
                self.set_phase(phase, PhaseStatus::Running);
                self.phase_started_at = Some(std::time::Instant::now());
                // Fresh phase → fresh stall clock; nothing has stalled yet.
                self.last_output_at = Some(std::time::Instant::now());
                self.tool_in_progress = false;
                // Auto-snapshot the workspace BEFORE this phase's base work, so
                // a whole phase can be rewound with /rewind. Offloaded to a
                // blocking task so a slow `git add -A` on a big repo never
                // freezes the UI event loop; best-effort (no git -> no snapshot).
                let root = self.project_root.clone();
                let label = umadev_i18n::tf(self.lang, "checkpoint.phase_label", &[phase.id()]);
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn_blocking(move || {
                        let _ = umadev_agent::checkpoint::create_checkpoint(&root, &label);
                    });
                } else {
                    let _ = umadev_agent::checkpoint::create_checkpoint(&root, &label);
                }
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::tf(self.lang, "run.phase_start", &[phase.id()]),
                );
            }
            EngineEvent::ArtifactWritten { phase, path } => {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("<artifact>");
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::tf(self.lang, "run.artifact_written", &[phase.id(), name]),
                );
            }
            EngineEvent::PhaseCompleted { phase } => {
                self.set_phase(phase, PhaseStatus::Done);
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::tf(self.lang, "event.phase_done", &[phase.id()]),
                );
            }
            EngineEvent::GateOpened { gate } => {
                self.active_gate = Some(gate);
                // Block paused at a gate — stop the live elapsed counters
                // so the status bar doesn't keep ticking while we wait on
                // the user.
                self.run_started_at = None;
                self.phase_started_at = None;

                // If the user QUEUED a steering message while the pipeline ran,
                // this gate is the gap to apply it: stash it for the event loop,
                // which re-runs the producing block with the queued text folded
                // in as a revision (overriding both auto-approve and the manual
                // pause). `active_gate` is already set above, so the loop knows
                // which block produced this gate.
                if !self.queued_steer.is_empty() {
                    // Fold EVERY queued steer (FIFO) into one revision — a single
                    // `Option` used to overwrite all but the last, silently
                    // dropping the earlier turns.
                    let text = self.queued_steer.drain(..).collect::<Vec<_>>().join("\n");
                    self.pending_steer = Some(text);
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::t(self.lang, "run.steer_applied"),
                    );
                    return;
                }

                // **Trust-tiered gate policy.** The active tier (resolved from a
                // `/mode` / `/auto` / `/manual` session override, else the
                // `.umadevrc` default) decides what happens here:
                //   - `auto`    → auto-approve EVERY checkpoint (incl. Clarify),
                //     running end-to-end. Most users aren't engineers — they type
                //     one requirement and expect the agent to self-resolve and
                //     drive. We also record a trust pass (and may *suggest*, never
                //     auto-apply, that a long-trusted gate auto-advance).
                //   - `guarded` → pause and show the gate card (the default,
                //     human-in-the-loop). The user types `c` to approve.
                //   - `plan`    → read-only: stop at the gate, never auto-continue
                //     (the runner already produced research + plan docs only).
                let mode = self.effective_trust_mode();
                if self.run_started && mode.gates_auto_approve() {
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::tf(self.lang, "gate.auto_approved", &[gate.id_str()]),
                    );
                    self.record_trust_pass(gate.id_str());
                    self.pending_auto_continue = Some(gate);
                    return;
                }

                self.push(
                    ChatRole::Gate,
                    gate_card(gate, &self.slug, &self.project_root, self.lang),
                );
                // Plan (read-only) tier: tell the user the run stops here by
                // design and how to execute the plan once they're happy with it.
                if mode == umadev_agent::TrustMode::Plan {
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::t(self.lang, "mode.plan.gate").to_string(),
                    );
                }
                // When the preview gate opens, surface the frontend's
                // recorded Preview URL so the user knows where to look.
                if gate == umadev_agent::gates::Gate::PreviewConfirm {
                    self.maybe_announce_preview();
                }
            }
            EngineEvent::BlockCompleted {
                final_phase,
                paused_at,
            } => {
                if paused_at.is_none() && final_phase == Phase::Delivery {
                    self.finished = true;
                    // A message queued during a late phase (after both gates)
                    // never hit a gap — surface it rather than silently drop it,
                    // so the user knows to resend now that the run is done.
                    if !self.queued_steer.is_empty() {
                        let text = self.queued_steer.drain(..).collect::<Vec<_>>().join("\n");
                        self.push(
                            ChatRole::System,
                            umadev_i18n::tf(self.lang, "run.queued_unsent", &[&text]),
                        );
                    }
                    self.active_gate = None;
                    self.run_started_at = None;
                    self.phase_started_at = None;
                    let lang = self.lang;
                    let release = self.project_root.join("release");
                    let zip_info = std::fs::read_dir(&release)
                        .ok()
                        .and_then(|rd| {
                            let mut zips: Vec<_> = rd
                                .filter_map(Result::ok)
                                .filter(|e| {
                                    e.path().extension().and_then(|s| s.to_str()) == Some("zip")
                                })
                                .collect();
                            zips.sort_by_key(std::fs::DirEntry::file_name);
                            zips.last().map(|z| {
                                let size =
                                    std::fs::metadata(z.path()).map_or(0, |m| m.len() / 1024);
                                umadev_i18n::tf(
                                    lang,
                                    "delivery.latest_zip",
                                    &[&z.file_name().to_string_lossy(), &size.to_string()],
                                )
                            })
                        })
                        .unwrap_or_default();
                    // The shareable HTML scorecard sits next to the zip.
                    let scorecard = std::fs::read_dir(&release)
                        .ok()
                        .and_then(|rd| {
                            let mut cards: Vec<_> = rd
                                .filter_map(Result::ok)
                                .filter(|e| {
                                    e.file_name().to_string_lossy().starts_with("scorecard-")
                                })
                                .collect();
                            cards.sort_by_key(std::fs::DirEntry::file_name);
                            cards.last().map(|c| {
                                umadev_i18n::tf(
                                    lang,
                                    "delivery.scorecard",
                                    &[&c.file_name().to_string_lossy()],
                                )
                            })
                        })
                        .unwrap_or_default();
                    let zip_info = format!("{zip_info}{scorecard}");
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::tf(lang, "delivery.complete_banner", &[&zip_info]),
                    );
                }
            }
            EngineEvent::BackendProbed {
                backend_id,
                ready,
                detail,
            } => {
                // Update or append the probe row.
                if let Some(existing) = self.backends.iter_mut().find(|b| b.id == backend_id) {
                    existing.ready = ready;
                    existing.detail = detail.clone();
                } else {
                    self.backends.push(BackendInfo {
                        id: backend_id.clone(),
                        ready,
                        detail: detail.clone(),
                    });
                }
                // If we're still on the picker, refresh its labels.
                if self.mode == AppMode::Picker {
                    refresh_picker_with_probes(&mut self.picker_items, &self.backends);
                }
            }
            EngineEvent::VerifyStarted { phase, command } => {
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::tf(self.lang, "event.verify_started", &[phase.id(), &command]),
                );
            }
            EngineEvent::VerifySkipped { phase, reason } => {
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::tf(self.lang, "event.verify_skipped", &[phase.id(), &reason]),
                );
            }
            EngineEvent::VerifyPassed { phase, duration_ms } => {
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::tf(
                        self.lang,
                        "event.verify_passed",
                        &[
                            phase.id(),
                            &(duration_ms / 1000).to_string(),
                            &((duration_ms % 1000) / 100).to_string(),
                        ],
                    ),
                );
            }
            EngineEvent::VerifyFailed {
                phase,
                exit_code,
                stderr,
            } => {
                let snippet = stderr.lines().next().unwrap_or("").trim();
                // Turn a raw build failure into an actionable next step
                // instead of a dead-end error code. Match the most common
                // failure signatures and route the user to the fix.
                let lower = stderr.to_ascii_lowercase();
                let action = if lower.contains("command not found")
                    || lower.contains("not found")
                    || lower.contains("no such file")
                {
                    umadev_i18n::t(self.lang, "verify.action.tool_missing")
                } else if lower.contains("cannot find module")
                    || lower.contains("module not found")
                    || lower.contains("unresolved import")
                    || lower.contains("could not resolve")
                {
                    umadev_i18n::t(self.lang, "verify.action.deps_missing")
                } else if lower.contains("type error")
                    || lower.contains("ts(")
                    || lower.contains("expected")
                    || lower.contains("mismatched types")
                {
                    umadev_i18n::t(self.lang, "verify.action.type_error")
                } else {
                    umadev_i18n::t(self.lang, "verify.action.generic")
                };
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(
                        self.lang,
                        "event.verify_failed",
                        &[phase.id(), &exit_code.to_string(), snippet, action],
                    ),
                );
            }
            EngineEvent::HostOutput { phase: _, line } => {
                // A host-output line is a sign of life → reset the stall clock.
                self.mark_output();
                // Cap each line so a 1000-char paragraph doesn't blow the layout.
                let cap = 300;
                let trimmed: String = if line.chars().count() > cap {
                    let cut: String = line.chars().take(cap).collect();
                    format!("{cut}…")
                } else {
                    line
                };
                // Group consecutive host-output lines into the same chat
                // bubble — they belong to one phase's stream and reading
                // them as separate messages is visually noisy.
                if let Some(last) = self.history.back_mut() {
                    if last.role == ChatRole::Host {
                        last.body.push('\n');
                        last.body.push_str(&trimmed);
                    } else {
                        self.push(ChatRole::Host, trimmed);
                    }
                } else {
                    self.push(ChatRole::Host, trimmed);
                }
            }
            EngineEvent::Note(note) => {
                // A TERMINAL-ABORT note (a block that returned `Err` → produced
                // zero phases and is over) carries the `ABORT_SENTINEL`. Treat it
                // as a real run-ending state, NOT an ordinary progress heartbeat:
                // strip the marker, flip into the explicit `aborted` terminal
                // state, and stop the live counters so the status bar shows
                // "aborted" instead of the misleading idle "ready / 0/9" look.
                if let Some(body) = note.strip_prefix(crate::ABORT_SENTINEL) {
                    self.mark_block_aborted(body.to_string());
                    return;
                }
                // A bare progress Note must NOT clear `thinking`. A route is
                // still in flight here (its TERMINAL outcome arrives as a
                // `RouteDecision` on the route channel — `Chat` / `Run` /
                // `Failed` — each of which clears `thinking` itself). An
                // unrelated heartbeat (`route.resume_retry`, a pipeline note,
                // governance) reaching this arm used to prematurely kill the
                // animation, making a live route look "stuck with no result".
                // A progress note is still a sign of life (artifact written, a
                // phase heartbeat, governance) → reset the stall clock so the
                // red cue only fires when the pipeline is TRULY silent.
                self.mark_output();
                self.push(ChatRole::System, note);
            }
            EngineEvent::SubTaskStarted {
                phase,
                task_id,
                label,
            } => {
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(
                        self.lang,
                        "event.subtask_started",
                        &[&format!("{phase:?}"), &task_id, &label],
                    ),
                );
            }
            EngineEvent::SubTaskCompleted { phase, task_id, ok } => {
                let outcome = if ok {
                    umadev_i18n::t(self.lang, "event.subtask_done")
                } else {
                    umadev_i18n::t(self.lang, "event.subtask_failed")
                };
                let tag = if ok { "[ok]" } else { "[fail]" };
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(
                        self.lang,
                        "event.subtask_completed",
                        &[tag, &format!("{phase:?}"), &task_id, outcome],
                    ),
                );
            }
            EngineEvent::WorkerStream { event } => {
                // Real-time streaming display — the user sees the worker's
                // live activity instead of a blank spinner. ANY stream event is
                // a sign of life → reset the stall clock.
                self.mark_output();
                match event {
                    umadev_runtime::StreamEvent::Text { delta } => {
                        self.stream_tool_batch = None;
                        if !delta.trim().is_empty() {
                            // **Typewriter effect**: if the previous event was
                            // also text and the last chat message is from Host,
                            // append this delta to it instead of pushing a new
                            // line. This gives a ChatGPT-like streaming feel.
                            let appended = if self.stream_text_active {
                                if let Some(last) = self.history.back_mut() {
                                    if last.role == ChatRole::Host {
                                        if last.body.len() < 2000 {
                                            last.body.push_str(&delta);
                                            true
                                        } else {
                                            if !last.body.ends_with('…') {
                                                last.body.push_str(" …");
                                            }
                                            true
                                        }
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            } else {
                                false
                            };
                            if !appended {
                                let preview: String = delta.chars().take(2000).collect();
                                self.push(ChatRole::Host, preview);
                                self.stream_text_active = true;
                            }
                        }
                    }
                    umadev_runtime::StreamEvent::ToolUse { name, detail } => {
                        self.stream_text_active = false; // text stream interrupted
                                                         // A tool call is now in flight — a long one (npm install)
                                                         // is WORK, not a stall, so suppress the red signal until
                                                         // its result returns.
                        self.tool_in_progress = true;
                        let icon = match name.as_str() {
                            "Read" | "NotebookEdit" => "[read]",
                            "Write" | "Edit" => "[write]",
                            "Bash" => "[run]",
                            "Grep" | "Glob" => "[search]",
                            "WebSearch" | "WebFetch" => "[web]",
                            "Task" | "Agent" => "[agent]",
                            _ => "[auto]",
                        };
                        let detail_display = if detail.is_empty() {
                            String::new()
                        } else {
                            let d: String = detail.chars().take(80).collect();
                            format!(" `{d}`")
                        };
                        // **Throttle**: if the last message was the same tool
                        // type, increment a counter and update the message
                        // in-place instead of pushing a new line. This turns
                        // 10 × `[read] Read` into one `[read] Read (10): last_file`.
                        let is_same_batch = self
                            .stream_tool_batch
                            .as_ref()
                            .is_some_and(|(prev_name, _)| prev_name == &name);
                        if is_same_batch {
                            // Update the existing batch entry.
                            let count = self.stream_tool_batch.as_ref().unwrap().1 + 1;
                            self.stream_tool_batch = Some((name.clone(), count));
                            // Replace the last Host message with the updated count.
                            if let Some(last) = self.history.back_mut() {
                                if last.role == ChatRole::Host {
                                    last.body = format!("{icon} {name} ({count}){detail_display}");
                                }
                            }
                        } else {
                            // New tool type — push a fresh message.
                            self.stream_tool_batch = Some((name.clone(), 1));
                            self.push(ChatRole::Host, format!("{icon} {name}{detail_display}"));
                        }
                    }
                    umadev_runtime::StreamEvent::ToolResult { ok, summary } => {
                        self.stream_text_active = false;
                        // The in-flight tool call returned → no longer "working
                        // on a tool"; the stall clock applies normally again.
                        self.tool_in_progress = false;
                        let mark = if ok { "[ok]" } else { "[fail]" };
                        let preview: String = summary.chars().take(100).collect();
                        if !preview.trim().is_empty() {
                            self.push(ChatRole::Host, format!("  {mark} {preview}"));
                        }
                    }
                    umadev_runtime::StreamEvent::Warning { message } => {
                        self.stream_text_active = false;
                        self.push(ChatRole::System, format!("[warn] {message}"));
                    }
                    umadev_runtime::StreamEvent::Thinking => {
                        // Show a brief "thinking…" indicator. This is replaced
                        // by actual content when the next text/tool event
                        // arrives (stream_text_active = false resets it).
                        self.stream_text_active = false;
                        self.stream_tool_batch = None;
                        self.push(
                            ChatRole::System,
                            format!(
                                "[thinking] {}",
                                umadev_i18n::t(self.lang, "status.thinking")
                            ),
                        );
                    }
                }
            }
        }
        self.refresh_status();
    }

    // ---- key events -------------------------------------------------------

    /// Fold one key press into the model; return the loop's next action.
    ///
    /// `mods` carries modifiers (Shift, Ctrl, Alt) so multi-line input
    /// via `Shift+Enter` works. Tests that don't care about modifiers
    /// can use [`apply_key`](Self::apply_key) for the no-mods shortcut.
    #[must_use]
    pub fn apply_key_with_mods(
        &mut self,
        key: KeyCode,
        mods: crossterm::event::KeyModifiers,
    ) -> Action {
        // F1 toggles help in any mode.
        if let KeyCode::F(1) = key {
            self.show_help = !self.show_help;
            self.help_scroll = 0;
            return Action::None;
        }
        match self.mode {
            AppMode::Picker => self.picker_key(key),
            AppMode::Chat => self.chat_key(key, mods),
        }
    }

    /// Convenience wrapper for tests / call sites that don't care about
    /// modifier state.
    #[must_use]
    pub fn apply_key(&mut self, key: KeyCode) -> Action {
        self.apply_key_with_mods(key, crossterm::event::KeyModifiers::NONE)
    }

    /// Switch the first-run guided setup to `step`, regenerating its item list
    /// (the language list / the mode choices / the live-probed base-CLI list).
    pub(crate) fn goto_picker_step(&mut self, step: PickerStep) {
        self.picker_step = step;
        self.picker_items = step_items(step, self.lang, &self.backends);
        self.picker_selected = if step == PickerStep::Language {
            self.lang as usize
        } else {
            0
        };
        self.picker_notice = None;
    }

    fn picker_key(&mut self, key: KeyCode) -> Action {
        match key {
            // Esc walks BACK one step; the language step quits.
            KeyCode::Esc => match self.picker_step {
                PickerStep::Language => {
                    self.should_quit = true;
                    Action::Quit
                }
                PickerStep::BaseCli => {
                    self.goto_picker_step(PickerStep::Language);
                    Action::None
                }
            },
            KeyCode::Up => {
                self.picker_notice = None;
                if self.picker_selected > 0 {
                    self.picker_selected -= 1;
                }
                Action::None
            }
            KeyCode::Down => {
                self.picker_notice = None;
                if self.picker_selected + 1 < self.picker_items.len() {
                    self.picker_selected += 1;
                }
                Action::None
            }
            KeyCode::Enter => {
                let chosen = self.picker_items[self.picker_selected].clone();
                // Step 1 - language: set it, then advance to the base picker.
                if let Some(lang) = chosen.lang {
                    self.lang = lang;
                    umadev_i18n::set_lang(lang);
                    self.config.lang = Some(lang.code().to_string());
                    let _ = crate::config::save_to(&self.config, &self.config_path);
                    self.goto_picker_step(PickerStep::BaseCli);
                    return Action::None;
                }
                // A base CLI must be installed/reachable before we commit to it.
                if chosen.backend_id.is_some() && !chosen.ready {
                    self.picker_notice = Some(umadev_i18n::tf(
                        self.lang,
                        "picker.unavailable",
                        &[&chosen.label, &chosen.detail],
                    ));
                    return Action::None;
                }
                // Commit the chosen base CLI id and enter the chat.
                self.commit_backend(chosen.backend_id.clone());
                self.mode = AppMode::Chat;
                self.push_greeting();
                self.refresh_status();
                Action::BackendChanged
            }
            _ => Action::None,
        }
    }
    fn chat_key(&mut self, key: KeyCode, mods: crossterm::event::KeyModifiers) -> Action {
        // Overlay routing first — when an overlay is open, everything
        // is scroll / close.
        if self.overlay.is_some() {
            return self.overlay_key(key);
        }
        if self.show_help {
            match key {
                KeyCode::Esc => {
                    self.show_help = false;
                    return Action::None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                    return Action::None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.help_scroll = self.help_scroll.saturating_add(10);
                    return Action::None;
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(10);
                    return Action::None;
                }
                // F1 toggles help off (handled earlier); any OTHER key is
                // swallowed by the overlay — it must NOT fall through to the
                // chat handler, or keystrokes would land in the hidden input
                // box behind the overlay and Enter could launch a run unseen.
                _ => return Action::None,
            }
        }
        let has_palette = !self.palette_matches().is_empty();
        let shift = mods.contains(crossterm::event::KeyModifiers::SHIFT);
        let ctrl = mods.contains(crossterm::event::KeyModifiers::CONTROL);
        let alt = mods.contains(crossterm::event::KeyModifiers::ALT);
        // Ctrl+Alt half-page scroll keys are matched on the EXACT modifier set
        // (CONTROL | ALT, nothing else) so they never collide with a bare
        // CONTROL editing/shell key (Ctrl-U clears the line, Ctrl-D is EOF).
        let ctrl_alt = ctrl && alt && !shift;

        match key {
            // ---- exit handling ----
            KeyCode::Esc => {
                // Running → Esc INTERRUPTS the pipeline (like Claude Code); it
                // does NOT quit the app. The event loop aborts the in-flight
                // task and `cancel_run` resets back to a clean prompt.
                if self.is_pipeline_active() {
                    return Action::Cancel;
                }
                // An agentic execution turn (routed, not a pipeline run) is
                // streaming in a real base subprocess. Esc must INTERRUPT it (not
                // quit the app) — same as Ctrl-C's `agentic_in_flight` branch, so
                // the subprocess is actually aborted via `Action::Cancel` rather
                // than left running behind a dropped TUI.
                if self.agentic_in_flight {
                    return Action::Cancel;
                }
                // Idle → require a SECOND Esc to actually quit, so a stray
                // keypress (or the very Esc that just interrupted a run) can't
                // drop the whole app by accident.
                if !self.pending_quit_confirm {
                    self.pending_quit_confirm = true;
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "quit.confirm_running"),
                    );
                    return Action::None;
                }
                self.should_quit = true;
                Action::Quit
            }

            // ---- input editing ----
            KeyCode::Backspace => {
                self.pending_quit_confirm = false;
                self.backspace();
                Action::None
            }
            KeyCode::Delete => {
                self.pending_quit_confirm = false;
                self.forward_delete();
                Action::None
            }
            KeyCode::Left => {
                self.move_cursor(-1);
                Action::None
            }
            KeyCode::Right => {
                self.move_cursor(1);
                Action::None
            }
            // ---- transcript scrollback (review history without losing input) --
            // PageUp/Down + Ctrl+Alt+U/D page the transcript; Shift+↑/↓ nudge it one
            // row. Any upward scroll un-pins the view from the bottom until End
            // (or scrolling back to 0).
            //
            // Home/End are context-sensitive: with text in the box they keep
            // their line-editing meaning (cursor to start/end); on an EMPTY box
            // they jump the TRANSCRIPT to top/bottom — the place a user reaches
            // for "scroll to the very top/bottom" when not mid-edit. Ctrl-A /
            // Ctrl-E always do line-start/line-end regardless.
            KeyCode::Home if self.input.is_empty() => {
                self.transcript_scroll_to_top();
                Action::None
            }
            KeyCode::End if self.input.is_empty() => {
                self.transcript_scroll_to_bottom();
                Action::None
            }
            KeyCode::Home => {
                self.input_cursor = 0;
                Action::None
            }
            KeyCode::End => {
                self.input_cursor = self.input_len();
                Action::None
            }
            KeyCode::PageUp => {
                let page = self.transcript_page();
                self.transcript_scroll_up(page);
                Action::None
            }
            KeyCode::PageDown => {
                let page = self.transcript_page();
                self.transcript_scroll_down(page);
                Action::None
            }
            // Shift+↑ / Shift+↓ scroll the transcript one row at a time.
            KeyCode::Up if shift => {
                self.transcript_scroll_up(1);
                Action::None
            }
            KeyCode::Down if shift => {
                self.transcript_scroll_down(1);
                Action::None
            }

            // ---- palette navigation (only when /-prefix has matches) ----
            KeyCode::Up if has_palette => {
                self.cycle_palette(-1);
                Action::None
            }
            KeyCode::Down if has_palette => {
                self.cycle_palette(1);
                Action::None
            }
            KeyCode::Tab if has_palette => {
                self.autocomplete_palette();
                Action::None
            }

            // ---- shift+Tab: cycle gate-approval mode (auto <-> manual) ----
            KeyCode::BackTab => {
                self.cycle_approval_mode();
                Action::None
            }

            // ---- input history recall (no palette + empty-or-recalling input) ----
            KeyCode::Up
                if !has_palette && (self.input.is_empty() || self.input_history_idx.is_some()) =>
            {
                self.input_history_back();
                Action::None
            }
            KeyCode::Down if !has_palette && self.input_history_idx.is_some() => {
                self.input_history_forward();
                Action::None
            }

            // ---- enter: submit, or insert newline with Shift ----
            KeyCode::Enter => {
                if shift {
                    // Shift+Enter inserts a literal newline so the user
                    // can build multi-line prompts inside the chat box.
                    self.insert_at_cursor('\n');
                    return Action::None;
                }
                self.pending_quit_confirm = false;
                // Command-palette ergonomics: pressing Enter on a PARTIAL slash
                // verb (e.g. "/dep" while the palette highlights "/deploy")
                // should RUN the highlighted command — not submit the partial as
                // an "unknown command". Only when the input is a bare verb (no
                // args yet) and isn't already an exact command.
                if self.input.starts_with('/') && !self.input[1..].contains(char::is_whitespace) {
                    let matches = self.palette_matches();
                    if !matches.is_empty() {
                        let typed = self.input[1..].to_ascii_lowercase();
                        let is_exact = matches.iter().any(|(v, _)| *v == typed)
                            || umadev_host::driver_for(&typed).is_some();
                        if !is_exact {
                            let sel = self.palette_selected.min(matches.len() - 1);
                            self.input = format!("/{}", matches[sel].0);
                            self.input_cursor = self.input_len();
                        }
                    }
                }
                let raw = self.input.trim().to_string();
                self.clear_input();
                if raw.is_empty() {
                    return Action::None;
                }
                // Submitting re-pins the transcript to the bottom so the user
                // always sees their own new turn (and the reply) land, even if
                // they were scrolled up reviewing history.
                self.transcript_scroll_to_bottom();
                self.remember_submission(&raw);
                if let Some(action) = self.try_slash_command(&raw) {
                    return action;
                }
                self.submit_text(raw)
            }

            // ---- emacs-style line editing (Claude Code parity) ----
            KeyCode::Char('a') if ctrl => {
                self.input_cursor = 0;
                Action::None
            }
            KeyCode::Char('e') if ctrl => {
                self.input_cursor = self.input_len();
                Action::None
            }
            // Ctrl+Alt+U / Ctrl+Alt+B: half-page scroll UP through the
            // transcript. Moved off bare Ctrl-U (which is the shell "clear line"
            // convention) so the editing key keeps its job. Matched on the exact
            // CONTROL|ALT set (`ctrl_alt`) so it can't fire on a bare Ctrl-U.
            KeyCode::Char('u' | 'b') if ctrl_alt => {
                let half = self.transcript_half_page();
                self.transcript_scroll_up(half);
                Action::None
            }
            // Ctrl+Alt+D / Ctrl+Alt+F: half-page scroll DOWN through the
            // transcript. Moved off bare Ctrl-D (terminal EOF / quit convention).
            KeyCode::Char('d' | 'f') if ctrl_alt => {
                let half = self.transcript_half_page();
                self.transcript_scroll_down(half);
                Action::None
            }
            // Bare Ctrl-U keeps its line-editing meaning (delete to line start).
            KeyCode::Char('u') if ctrl => {
                self.delete_to_line_start();
                Action::None
            }
            KeyCode::Char('k') if ctrl => {
                self.delete_to_line_end();
                Action::None
            }
            KeyCode::Char('w') if ctrl => {
                self.delete_word_back();
                Action::None
            }
            KeyCode::Char('c') if ctrl => {
                // Ctrl+C parity with Claude Code / opencode: while work is in
                // flight (a pipeline run OR a routed chat turn), Ctrl-C
                // INTERRUPTS it immediately — regardless of whether the input
                // box has text. The old behaviour (only-interrupt-on-empty)
                // forced a second keystroke to actually stop a run when the
                // user had half-typed the next message.
                if self.is_pipeline_active() {
                    // Defensive: dropping the input on an interrupt avoids a
                    // half-typed turn silently submitting later.
                    self.clear_input();
                    return Action::Cancel;
                }
                if self.agentic_in_flight {
                    // An agentic execution call is streaming in a real base
                    // subprocess (parked in the event loop's `run_task`). Unlike
                    // a fire-and-forget chat route, simply clearing the spinner
                    // would leave the subprocess running — so route through
                    // `Action::Cancel`, which aborts the task. `cancel_run`
                    // clears `thinking` / `agentic_in_flight` and the queue.
                    self.clear_input();
                    return Action::Cancel;
                }
                if self.thinking {
                    // A routed chat turn is in flight. Stop the spinner and drop
                    // any chat turns parked behind it so the interrupt is clean;
                    // the in-flight route task is fire-and-forget (it only chats,
                    // no workspace mutation) so its late reply is simply ignored.
                    self.thinking = false;
                    self.thinking_started = None;
                    self.queued_chat.clear();
                    self.clear_input();
                    self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.cancelled"));
                    self.refresh_status();
                    return Action::None;
                }
                // Idle: clear a non-empty input; on empty input behave like Esc
                // (quit-confirm).
                if self.input.is_empty() {
                    return self.chat_key(KeyCode::Esc, crossterm::event::KeyModifiers::NONE);
                }
                self.clear_input();
                Action::None
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => {
                self.should_quit = true;
                Action::Quit
            }

            // ---- printable char ----
            KeyCode::Char(c) => {
                self.pending_quit_confirm = false;
                self.input_history_idx = None;
                self.insert_at_cursor(c);
                Action::None
            }

            _ => Action::None,
        }
    }

    /// Treat non-slash text as either a fresh requirement (if no run is
    /// active) or a revision (if a gate is open). Single-letter `c` at a
    /// gate is the documented shortcut for "approve / continue" — match
    /// the gate card so users don't have to type `/continue` every time.
    fn submit_text(&mut self, text: String) -> Action {
        self.push(ChatRole::You, text.clone());
        // A routed turn is still in flight (`thinking`). Spawning a second
        // `spawn_route` now would resume the SAME chat `session_id` in two base
        // subprocesses at once → interleaved / out-of-order replies and a
        // scrambled memory. Park this turn instead; the event loop fires it as
        // the next route only after the current result lands. (A gate is never
        // open while `thinking`, so this check sits ahead of gate handling.)
        if self.thinking {
            self.queued_chat.push_back(text);
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.queued"));
            self.refresh_status();
            return Action::None;
        }
        if let Some(gate) = self.active_gate {
            // ClarifyGate: non-"c" text is an answer (append to
            // answers file); "c" submits all answers + continues.
            if gate == Gate::ClarifyGate {
                if matches!(text.trim(), "c" | "C") {
                    self.active_gate = None;
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::t(self.lang, "gate.clarify_saved").to_string(),
                    );
                    return Action::Continue(gate);
                }
                self.append_clarify_answer(&text);
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::t(self.lang, "gate.clarify_recorded").to_string(),
                );
                return Action::None;
            }
            if matches!(text.trim(), "c" | "C") {
                self.active_gate = None;
                let what = match gate {
                    Gate::DocsConfirm => umadev_i18n::t(self.lang, "gate.confirmed_docs"),
                    Gate::PreviewConfirm => umadev_i18n::t(self.lang, "gate.confirmed_preview"),
                    Gate::ClarifyGate => umadev_i18n::t(self.lang, "gate.confirmed_generic"),
                };
                self.push(ChatRole::UmaDev, format!("[ok] {what}"));
                // A manual approval also builds trust for this gate.
                self.record_trust_pass(gate.id_str());
                return Action::Continue(gate);
            }
            // A revision request resets this gate's trust streak.
            self.record_trust_revision(gate.id_str());
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(self.lang, "gate.revision_received", &[&text]),
            );
            Action::Revise(text)
        } else if self.run_started && self.finished {
            self.record_user_turn(&text);
            self.thinking = true; // animated "thinking…" until the base replies
            self.thinking_started = Some(std::time::Instant::now());
            // Fresh chat turn → fresh stall clock (don't inherit a stale time
            // from an earlier phase and flash red immediately).
            self.last_output_at = None;
            self.tool_in_progress = false;
            self.refresh_status();
            Action::Route(text)
        } else if !self.run_started {
            // Natural-language intent belongs to the selected base
            // (Claude Code / Codex / OpenCode / external model API). UmaDev is
            // only the shell: it relays the base's decision — a conversational
            // reply, or a 9-phase pipeline run — and never classifies the
            // intent itself. The full conversation is carried along so the base
            // answers with memory of what was already said.
            self.record_user_turn(&text);
            self.thinking = true; // animated "thinking…" until the base replies
            self.thinking_started = Some(std::time::Instant::now());
            // Fresh chat turn → fresh stall clock (see above).
            self.last_output_at = None;
            self.tool_in_progress = false;
            self.refresh_status();
            Action::Route(text)
        } else {
            // Pipeline is mid-phase (no gate open). We can't inject into the
            // running base subprocess, so QUEUE the message and fire it at the
            // next gap — gate / phase boundary — like Claude Code queuing a turn.
            self.queued_steer.push_back(text);
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.queued"));
            Action::None
        }
    }

    /// Detect whether the user's input is casual chitchat / a greeting /
    /// a question, rather than a real project requirement that should
    /// launch the 9-phase pipeline.
    fn looks_like_chitchat(text: &str) -> bool {
        let t = text.trim();
        let lower = t.to_lowercase();

        // Greeting keywords (EN + ZH).
        let greetings = [
            "hi",
            "hello",
            "hey",
            "yo",
            "sup",
            "howdy",
            "你好",
            "您好",
            "在吗",
            "在么",
            "嗨",
            "哈喽",
            "thanks",
            "thank you",
            "thx",
            "谢谢",
            "感谢",
            "bye",
            "goodbye",
            "再见",
            "拜拜",
            "ok",
            "okay",
            "好的",
            "嗯",
            "哦",
        ];
        if greetings
            .iter()
            .any(|g| lower == *g || lower.starts_with(g) && t.len() < 20)
        {
            return true;
        }

        // Short question with no action verbs = likely chitchat.
        let has_action_verb = [
            "做",
            "建",
            "开发",
            "构建",
            "写",
            "创建",
            "设计",
            "实现",
            "部署",
            "build",
            "create",
            "make",
            "design",
            "develop",
            "implement",
            "deploy",
            "write",
            "generate",
            "fix",
            "refactor",
            "add",
        ]
        .iter()
        .any(|v| lower.contains(v));

        if !has_action_verb && t.chars().count() < 15 {
            return true;
        }

        // "how are you" / "你好吗" / "what can you do" style questions.
        let questions = [
            "how are you",
            "what can you do",
            "who are you",
            "what is this",
            "你好吗",
            "你是谁",
            "你能做什么",
            "这是什么",
            "怎么用",
            "帮助",
        ];
        if questions.iter().any(|q| lower.contains(q)) {
            return true;
        }

        false
    }

    /// Return true only for plain text that looks like a product/code task.
    ///
    /// This is intentionally conservative. UmaDev is allowed to chat in the
    /// TUI; starting the full pipeline is expensive and should require an
    /// obvious build/fix/design/deploy intent, or an explicit `/run`.
    pub(crate) fn looks_like_project_requirement(text: &str) -> bool {
        let t = text.trim();
        if t.is_empty() || Self::looks_like_chitchat(t) {
            return false;
        }
        let lower = t.to_lowercase();

        let action_verbs = [
            "做",
            "建",
            "开发",
            "构建",
            "写",
            "创建",
            "设计",
            "实现",
            "部署",
            "修复",
            "重构",
            "添加",
            "生成",
            "改造",
            "搭建",
            "build",
            "create",
            "make",
            "design",
            "develop",
            "implement",
            "deploy",
            "write",
            "generate",
            "fix",
            "refactor",
            "add",
            "scaffold",
        ];
        if action_verbs.iter().any(|v| lower.contains(v)) {
            return true;
        }

        let project_nouns = [
            "系统",
            "应用",
            "网站",
            "网页",
            "页面",
            "小程序",
            "平台",
            "后台",
            "前端",
            "后端",
            "接口",
            "api",
            "app",
            "website",
            "page",
            "dashboard",
            "backend",
            "frontend",
            "service",
            "cli",
            "tui",
            "saas",
            "landing page",
            "登录页",
            "博客",
            "商城",
            "论坛",
        ];
        project_nouns.iter().any(|n| lower.contains(n))
    }

    /// Generate a conversational reply for chitchat input, guiding the
    /// user toward entering a real requirement.
    pub(crate) fn chitchat_reply(text: &str) -> String {
        let lower = text.trim().to_lowercase();
        if lower.starts_with("你好")
            || lower.starts_with("您好")
            || lower == "hi"
            || lower == "hello"
            || lower == "hey"
            || lower.contains("你好吗")
            || lower.contains("在吗")
            || lower.contains("在么")
        {
            return umadev_i18n::tl("chitchat.greeting").to_string();
        }
        if lower.contains("谢谢") || lower.contains("感谢") || lower.starts_with("th") {
            return umadev_i18n::tl("chitchat.thanks").to_string();
        }
        if lower.contains("你是谁")
            || lower.contains("what can you do")
            || lower.contains("你能做什么")
        {
            return umadev_i18n::tl("chitchat.who").to_string();
        }
        // Generic fallback for short non-requirement text.
        umadev_i18n::tl("chitchat.fallback").to_string()
    }

    /// Record a user turn into [`App::conversation`] (the memory handed to the
    /// base on the next routed turn). Trims to the most recent
    /// [`CONVERSATION_CAP`] messages so the prompt stays bounded.
    pub(crate) fn record_user_turn(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.conversation.push(umadev_runtime::Message {
            role: "user".to_string(),
            content: text.to_string(),
        });
        self.trim_conversation();
    }

    /// Record the base's conversational reply: render it in the chat as a
    /// `Host` message AND append it to [`App::conversation`] as the assistant
    /// turn, so the next turn the base sees its own previous answer.
    pub(crate) fn record_chat_reply(&mut self, reply: String) {
        self.thinking = false; // reply landed — stop the "thinking…" status
        self.thinking_started = None;
        self.refresh_status();
        let reply = reply.trim().to_string();
        if reply.is_empty() {
            return;
        }
        self.push(ChatRole::Host, reply.clone());
        self.conversation.push(umadev_runtime::Message {
            role: "assistant".to_string(),
            content: reply,
        });
        self.trim_conversation();
    }

    /// Note, as an assistant turn, that the base routed the message into a
    /// pipeline run. Keeps the conversation coherent for any chat that follows
    /// a build (so "what did you just build?" has context).
    pub(crate) fn record_run_started(&mut self, requirement: &str) {
        self.thinking = false; // routed to a pipeline run; the run spinner takes over
        self.thinking_started = None;
        let requirement = requirement.trim();
        if requirement.is_empty() {
            return;
        }
        // Make the routing decision VISIBLE — the base just classified this as
        // build-work (vs a chat reply), and a multi-minute pipeline is about to
        // start. Without this marker the launch is a surprise the user can't
        // predict turn to turn.
        self.push(
            ChatRole::System,
            umadev_i18n::t(self.lang, "run.classified_build").to_string(),
        );
        self.conversation.push(umadev_runtime::Message {
            role: "assistant".to_string(),
            content: umadev_i18n::tf(self.lang, "run.classified_build_memo", &[requirement]),
        });
        self.trim_conversation();
    }

    /// The route ended without a usable reply (base init failed, an empty
    /// reply, or a hard error). This is a TERMINAL route outcome, so — like
    /// [`record_chat_reply`] / [`record_run_started`] — it stops the
    /// "thinking…" status; otherwise the animation would spin forever on a
    /// route that already failed. The human-readable reason is surfaced as a
    /// System note. Also clears `agentic_in_flight`: a failed agentic execution
    /// call flows through here, so this is its terminal cleanup too.
    pub(crate) fn record_route_failed(&mut self, note: String) {
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.refresh_status();
        self.push(ChatRole::System, note);
    }

    /// The base classified the turn as agentic (real work in THIS repo, short of
    /// a full pipeline build) and the tools-enabled streaming call is about to
    /// fire. Make the decision VISIBLE (the user is about to see live tool calls,
    /// not a chat reply) and note it as an assistant turn so a follow-up like
    /// "what did you find?" keeps context. `thinking` is left set by
    /// `fire_agentic` — the stream keeps it alive until the turn ends.
    pub(crate) fn record_agentic_started(&mut self, task: &str) {
        let task = task.trim();
        if task.is_empty() {
            return;
        }
        self.push(
            ChatRole::System,
            umadev_i18n::t(self.lang, "agentic.inspecting").to_string(),
        );
        self.conversation.push(umadev_runtime::Message {
            role: "assistant".to_string(),
            content: umadev_i18n::tf(self.lang, "agentic.working_on", &[task]),
        });
        self.trim_conversation();
    }

    /// An agentic streaming turn finished cleanly. The body ALREADY streamed live
    /// into the transcript (via `WorkerStream`), so we do NOT re-render it — we
    /// only record it as the assistant turn for chat-memory continuity and clear
    /// the waiting state. A TERMINAL agentic outcome, mirroring
    /// [`record_chat_reply`] but without the duplicate render.
    pub(crate) fn record_agentic_done(&mut self, reply: String) {
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.stream_text_active = false;
        self.stream_tool_batch = None;
        self.refresh_status();
        let reply = reply.trim().to_string();
        if reply.is_empty() {
            // The base produced only tool calls / a side-effect with no closing
            // prose. Still a clean finish — leave the streamed activity as the
            // record, but drop a short marker so the turn reads as completed.
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "agentic.done").to_string(),
            );
            return;
        }
        self.conversation.push(umadev_runtime::Message {
            role: "assistant".to_string(),
            content: reply,
        });
        self.trim_conversation();
    }

    /// Pop the oldest chat turn parked by [`submit_text`] while a route was in
    /// flight, if any. The event loop fires it as the NEXT route only after the
    /// current route result has landed, keeping same-session routing strictly
    /// serial (never two base subprocesses resuming one `session_id` at once).
    pub(crate) fn take_next_queued_chat(&mut self) -> Option<String> {
        self.queued_chat.pop_front()
    }

    /// Number of turns currently waiting to be sent — the chat-routing queue
    /// plus a pending pipeline steer. Drives the persistent "queued N" chip so
    /// the user can always see that parked input has NOT been lost, even after
    /// the one-off System note scrolls away.
    #[must_use]
    pub fn queued_count(&self) -> usize {
        self.queued_chat.len() + self.queued_steer.len()
    }

    /// A bounded clone of the conversation memory to hand to a routed turn.
    #[must_use]
    pub(crate) fn conversation_snapshot(&self) -> Vec<umadev_runtime::Message> {
        self.conversation.clone()
    }

    /// Return the UUID pinning this chat's host-CLI session, minting one on
    /// first use. Called when routing a turn to a `HostCli` base so `claude`
    /// can `--session-id` / `--resume` its own conversation deterministically.
    pub(crate) fn ensure_chat_session_id(&mut self) -> String {
        if self.chat_session_id.is_none() {
            self.chat_session_id = Some(new_chat_session_id());
        }
        // Safe: just set above when absent.
        self.chat_session_id.clone().unwrap_or_default()
    }

    /// Keep only the most recent [`CONVERSATION_CAP`] messages.
    fn trim_conversation(&mut self) {
        let len = self.conversation.len();
        if len > CONVERSATION_CAP {
            self.conversation.drain(0..len - CONVERSATION_CAP);
        }
    }

    /// Surface a "this is what I'm about to do" preview so the user
    /// isn't left wondering whether their Enter actually did anything.
    /// Lands BEFORE the background pipeline task spawns + emits its
    /// own `PipelineStarted`.
    fn maybe_suggest_design(&mut self) {
        if self.config.design_system.is_some() {
            return;
        }
        let available = self.list_design_systems();
        if available.is_empty() {
            return;
        }
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "design.suggest_hint", &[&available.join(" · ")]),
        );
    }

    fn push_preflight(&mut self, text: &str) {
        let ds = self.config.design_system.as_deref().unwrap_or("auto");
        let tpl = self.config.seed_template.as_deref().unwrap_or("auto");
        let plan = umadev_i18n::tf(
            self.lang,
            "run.preflight_plan",
            &[text, &self.backend_label, ds, tpl],
        );
        self.push(ChatRole::UmaDev, plan);
    }

    fn reset_for_new_run(&mut self) {
        for row in &mut self.phases {
            row.status = PhaseStatus::Pending;
        }
        self.finished = false;
        self.run_started = false;
        self.active_gate = None;
        // Drop any not-yet-fired queued steers so they can't bleed into a later
        // run and fire at the wrong gate.
        self.queued_steer.clear();
        self.pending_steer = None;
    }

    /// Reset run state after `/cancel` aborts the in-flight pipeline task, and
    /// tell the user we're back at the prompt (workflow state on disk is intact,
    /// so a later run can resume from the last gate).
    pub fn cancel_run(&mut self) {
        self.reset_for_new_run();
        self.run_started_at = None;
        self.phase_started_at = None;
        // A route may have been in flight when the run was cancelled (e.g. the
        // turn that classified into this run, or a post-run chat). Clear the
        // "thinking…" animation too, otherwise it spins forever after an
        // interrupt with a route still notionally outstanding.
        self.thinking = false;
        self.thinking_started = None;
        // An agentic execution call (if any) is the thing being aborted here —
        // clear its flag so a later Ctrl-C doesn't think one is still running.
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        // Drop chat turns parked behind the in-flight route so they can't fire
        // into a freshly-reset state.
        self.queued_chat.clear();
        self.pending_quit_confirm = false;
        self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.cancelled"));
    }

    pub(crate) fn prepare_worker_routed_run(&mut self, requirement: &str) {
        if self.run_started {
            self.reset_for_new_run();
        }
        // The pipeline runs its own base sessions (one per phase block); the
        // post-run chat should start fresh rather than resume a chat session
        // that predates the build.
        self.host_chat_session_active = false;
        self.chat_session_id = None;
        self.maybe_suggest_design();
        self.push_preflight(requirement);
    }

    /// `Some(action)` if `raw` was a recognised slash command; `None`
    /// means "not a slash command, treat as ordinary input".
    fn try_slash_command(&mut self, raw: &str) -> Option<Action> {
        if !raw.starts_with('/') {
            return None;
        }
        let mut parts = raw[1..].splitn(2, char::is_whitespace);
        let verb = parts.next().unwrap_or("").to_ascii_lowercase();
        let rest = parts.next().unwrap_or("").trim();
        self.push(ChatRole::You, raw.to_string());
        let action = match verb.as_str() {
            "help" | "?" | "commands" => {
                self.show_help = true;
                self.help_scroll = 0;
                Action::None
            }
            "quit" | "q" | "exit" => {
                self.should_quit = true;
                Action::Quit
            }
            "clear" => {
                self.history.clear();
                self.conversation.clear();
                self.transcript_scroll = 0;
                // A cleared transcript means the base should start a fresh
                // session on the next turn, not resume the old one.
                self.host_chat_session_active = false;
                self.chat_session_id = None;
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "slash.history_cleared"),
                );
                Action::None
            }
            "claude" | "claude-code" => self.slash_backend(Some("claude-code")),
            "codex" => self.slash_backend(Some("codex")),
            "opencode" => self.slash_backend(Some("opencode")),
            "offline" => self.slash_backend(None),
            "init" => {
                let slug = if self.slug.is_empty() {
                    self.project_root
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("project")
                        .to_string()
                } else {
                    self.slug.clone()
                };
                let manifest = umadev_agent::SpecManifest::new(&slug);
                match manifest.write_to(&self.project_root, false) {
                    Ok(path) => {
                        let ds_count = self.scaffold_design_files();
                        let ds_msg = if ds_count > 0 {
                            umadev_i18n::tf(
                                self.lang,
                                "init.design_files",
                                &[&ds_count.to_string()],
                            )
                        } else {
                            String::new()
                        };
                        self.push(
                            ChatRole::UmaDev,
                            umadev_i18n::tf(
                                self.lang,
                                "init.manifest_written",
                                &[
                                    &path.display().to_string(),
                                    manifest.level.as_str(),
                                    manifest.profile.as_str(),
                                    &slug,
                                    &ds_msg,
                                ],
                            ),
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let ds_count = self.scaffold_design_files();
                        let ds_msg = if ds_count > 0 {
                            umadev_i18n::tf(
                                self.lang,
                                "init.design_files_new",
                                &[&ds_count.to_string()],
                            )
                        } else {
                            umadev_i18n::t(self.lang, "init.already_exists").to_string()
                        };
                        self.push(ChatRole::System, ds_msg);
                    }
                    Err(e) => self.push(
                        ChatRole::System,
                        umadev_i18n::tf(self.lang, "init.failed", &[&e.to_string()]),
                    ),
                }
                Action::None
            }
            "continue" => {
                if let Some(gate) = self.active_gate.take() {
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::tf(self.lang, "slash.gate_approved", &[gate.id_str()]),
                    );
                    self.record_trust_pass(gate.id_str());
                    Action::Continue(gate)
                } else {
                    let hint = if self.run_started && !self.finished {
                        umadev_i18n::t(self.lang, "continue.running")
                    } else if self.finished {
                        umadev_i18n::t(self.lang, "continue.finished")
                    } else {
                        umadev_i18n::t(self.lang, "continue.not_started")
                    };
                    self.push(ChatRole::System, hint);
                    Action::None
                }
            }
            "revise" => {
                if rest.is_empty() {
                    self.push(ChatRole::System, umadev_i18n::t(self.lang, "revise.usage"));
                    Action::None
                } else if let Some(gate) = self.active_gate {
                    self.record_trust_revision(gate.id_str());
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::tf(self.lang, "gate.revision_received", &[rest]),
                    );
                    Action::Revise(rest.to_string())
                } else {
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "revise.no_gate"),
                    );
                    Action::None
                }
            }
            "spec" => {
                self.open_spec_overlay();
                Action::None
            }
            "verify" => {
                self.open_verify_overlay();
                Action::None
            }
            "doctor" => {
                self.open_doctor_overlay();
                Action::None
            }
            "diff" => {
                self.open_diff_overlay(rest);
                Action::None
            }
            "runs" | "history-runs" => {
                self.open_runs_overlay();
                Action::None
            }
            "history" => {
                self.open_history_overlay();
                Action::None
            }
            "manual" => self.slash_set_review_mode(false),
            "auto" => self.slash_set_review_mode(true),
            "mode" => self.slash_mode(rest),
            "model" => self.slash_model(rest),
            "lang" | "language" | "语言" | "語言" => self.slash_lang(rest),
            "setup" | "reconfigure" | "guide" | "配置" | "設定" => self.slash_setup(),
            "preview" => self.slash_preview(),
            "stop-preview" => self.slash_stop_preview(),
            "deploy" => self.slash_deploy(rest),
            "pr" => {
                // PR mode shares ONE implementation with the `umadev pr` verb —
                // delegate to the subprocess (fail-open there) so the TUI never
                // force-pushes on its own and the readiness rails live in one
                // place. `/pr` is a dry run; `/pr create` opens the PR.
                let wants_create = rest.eq_ignore_ascii_case("create");
                self.push(ChatRole::UmaDev, umadev_i18n::t(self.lang, "pr.scanning"));
                let output =
                    self.run_subprocess_cli(if wants_create { "pr --create" } else { "pr" });
                self.push(ChatRole::System, output);
                Action::None
            }
            "usage" => self.slash_usage(),
            "animations" => self.slash_toggle_animations(),
            "mouse" => self.slash_toggle_mouse(),
            "bug" => self.slash_bug(),
            "design" => self.slash_design(rest),
            "template" => self.slash_template(rest),
            "run" => self.slash_run(rest),
            "quick" => self.slash_quick(rest),
            "status" => {
                self.open_status_overlay();
                Action::None
            }
            "export" => {
                self.slash_export();
                Action::None
            }
            "knowledge" => {
                self.open_knowledge_overlay();
                Action::None
            }
            "pitfalls" | "踩坑" => {
                let body = umadev_agent::pitfall_overview(&self.project_root);
                self.overlay = Some(Overlay::from_body(
                    umadev_i18n::t(self.lang, "pitfalls.overlay_title"),
                    &body,
                ));
                Action::None
            }
            "lessons" => self.slash_lessons(),
            "mcp" => {
                let output = self.run_subprocess_cli("mcp-manage list");
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "slash.mcp_header", &[&output]),
                );
                Action::None
            }
            "skill" => {
                let output = self.run_subprocess_cli("skill list");
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "slash.skill_header", &[&output]),
                );
                Action::None
            }
            "adopt" => {
                // Brownfield onboarding of the CURRENT workspace. Delegates to
                // the `umadev adopt` subprocess (fail-open there) so the TUI
                // shares one implementation with the CLI verb, then surfaces
                // its summary in the transcript.
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::t(self.lang, "adopt.tui_running"),
                );
                let output = self.run_subprocess_cli("adopt");
                self.push(ChatRole::System, output);
                Action::None
            }
            "cancel" | "abort" => {
                if self.is_pipeline_active() {
                    Action::Cancel
                } else {
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "cancel.none_running"),
                    );
                    Action::None
                }
            }
            "redo" => self.slash_redo(rest),
            "checkpoint" | "snapshot" => self.slash_checkpoint(rest),
            "rewind" | "rollback-files" => self.slash_rewind(rest),
            "config" => {
                self.open_config_overlay();
                Action::None
            }
            "version" => {
                self.open_version_overlay();
                Action::None
            }
            "changelog" => {
                self.open_changelog_overlay();
                Action::None
            }
            _ => {
                // Dynamic backend verbs: only a verb that resolves to a REAL
                // registered driver switches the worker. (Never special-case a
                // name that `driver_for` can't build — committing an unbuildable
                // backend id to config leaves the next run permanently broken.)
                // Keeps the TUI in lock-step with umadev-host's BACKEND_IDS.
                if umadev_host::driver_for(&verb).is_some() {
                    return Some(self.slash_backend(Some(&verb)));
                }
                let hint = Self::did_you_mean(&verb)
                    .map(|s| umadev_i18n::tf(self.lang, "slash.did_you_mean", &[s]))
                    .unwrap_or_default();
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "slash.unknown", &[&verb, &hint]),
                );
                Action::None
            }
        };
        Some(action)
    }

    /// Closest recognised slash verb to `typed`, if any sits within a
    /// useful "did-you-mean" radius. Used to suggest a fix when the user
    /// mistypes a command verb.
    fn did_you_mean(typed: &str) -> Option<&'static str> {
        if typed.is_empty() {
            return None;
        }
        // Prefix match first — handles `/c` → `claude` and `/rev` → `revise`.
        if let Some((verb, _)) = Self::SLASH_VERBS.iter().find(|(v, _)| v.starts_with(typed)) {
            return Some(verb);
        }
        // Also consider the dynamic backend verbs (goose, amp, junie, …).
        if let Some((verb, _)) = backend_palette_verbs()
            .iter()
            .find(|(v, _)| v.starts_with(typed))
        {
            return Some(verb);
        }
        // Otherwise Levenshtein ≤ 2 against known verbs (static + dynamic).
        let typed_lower = typed.to_ascii_lowercase();
        let (mut best, mut best_dist) = (None, usize::MAX);
        let all_verbs = Self::SLASH_VERBS
            .iter()
            .map(|(v, _)| *v)
            .chain(backend_palette_verbs().iter().map(|(v, _)| *v));
        for verb in all_verbs {
            let d = lev(&typed_lower, verb);
            if d < best_dist && d <= 2 {
                best = Some(verb);
                best_dist = d;
            }
        }
        best
    }

    fn slash_backend(&mut self, backend: Option<&str>) -> Action {
        let id = backend.unwrap_or("offline").to_string();
        self.commit_backend(backend.map(str::to_string));
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "backend.switched", &[&id]),
        );
        self.refresh_status();
        Action::BackendChanged
    }

    fn slash_model(&mut self, arg: &str) -> Action {
        if arg.is_empty() {
            let current = self
                .config
                .model
                .clone()
                .unwrap_or_else(|| umadev_i18n::t(self.lang, "model.base_default").to_string());
            // Per-base menu — the valid ids differ by base, so a generic list
            // would just invite typos that fail on the next run.
            let menu = match self.backend.as_deref() {
                Some("claude-code") => umadev_i18n::t(self.lang, "model.menu.claude"),
                Some("codex") => umadev_i18n::t(self.lang, "model.menu.codex"),
                Some("opencode") => umadev_i18n::t(self.lang, "model.menu.opencode"),
                _ => umadev_i18n::t(self.lang, "model.menu.external"),
            };
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "model.current", &[&current, menu]),
            );
            // Surface per-phase tiers so a configured split is visible (and the
            // feature is discoverable for those who haven't set it).
            let plan = self.config.model_plan.as_deref();
            let build = self.config.model_build.as_deref();
            if plan.is_some() || build.is_some() {
                let default = umadev_i18n::t(self.lang, "model.tiers_default");
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(
                        self.lang,
                        "model.tiers_current",
                        &[plan.unwrap_or(default), build.unwrap_or(default)],
                    ),
                );
            } else {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "model.tiers_hint").to_string(),
                );
            }
            return Action::None;
        }
        // Per-phase model TIERS: `/model plan <m>` / `/model build <m>` (or
        // `... off` to clear). Plan with a cheaper/faster model, write code with
        // a stronger one — the per-phase model assignment top agents converged on.
        let mut parts = arg.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or("");
        if head == "plan" || head == "build" {
            // apply_model_tiers mutates process-global env (set_var); a running
            // worker reads that same env on another thread. Refuse mid-run so we
            // never race a concurrent env read (a data race in edition 2021).
            if self.is_pipeline_active() {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "model.tiers_busy").to_string(),
                );
                return Action::None;
            }
            let value = parts.next().unwrap_or("").trim();
            let cleared = value.is_empty() || value.eq_ignore_ascii_case("off");
            let slot = if head == "plan" {
                &mut self.config.model_plan
            } else {
                &mut self.config.model_build
            };
            *slot = if cleared {
                None
            } else {
                Some(value.to_string())
            };
            self.config.apply_model_tiers();
            let _ = crate::config::save_to(&self.config, &self.config_path);
            let default = umadev_i18n::t(self.lang, "model.tiers_default_paren");
            let plan = self.config.model_plan.as_deref().unwrap_or(default);
            let build = self.config.model_build.as_deref().unwrap_or(default);
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "model.tiers_updated", &[plan, build]),
            );
            return Action::None;
        }
        self.config.model = Some(arg.to_string());
        if let Err(e) = crate::config::save_to(&self.config, &self.config_path) {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(
                    self.lang,
                    "config.write_failed",
                    &[&self.config_path.display().to_string(), &e.to_string()],
                ),
            );
        }
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "model.switched", &[arg]),
        );
        Action::None
    }
    fn slash_design(&mut self, arg: &str) -> Action {
        let available = self.list_design_systems();
        if arg.is_empty() {
            // No arg → open overlay listing all design systems with previews
            self.open_design_picker_overlay(&available);
            return Action::None;
        }
        if !available.contains(&arg.to_string()) && !available.is_empty() {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(
                    self.lang,
                    "design.not_found",
                    &[arg, &available.join(" · ")],
                ),
            );
            return Action::None;
        }
        self.config.design_system = Some(arg.to_string());
        if let Err(e) = crate::config::save_to(&self.config, &self.config_path) {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(
                    self.lang,
                    "config.write_failed",
                    &[&self.config_path.display().to_string(), &e.to_string()],
                ),
            );
        }
        // Show a rich preview of the selected design system
        let preview = self.read_design_system_preview(arg);
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(self.lang, "design.applied", &[arg, &preview]),
        );
        self.refresh_status();
        Action::None
    }

    fn slash_template(&mut self, arg: &str) -> Action {
        let available = self.list_seed_templates();
        if arg.is_empty() {
            let current = self
                .config
                .seed_template
                .as_deref()
                .unwrap_or("(auto-detect)");
            let list = if available.is_empty() {
                "(no seed templates found in knowledge/seed-templates/)".to_string()
            } else {
                available.join(" · ")
            };
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "template.usage", &[current, &list]),
            );
            return Action::None;
        }
        if !available.contains(&arg.to_string()) && !available.is_empty() {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(
                    self.lang,
                    "template.not_found",
                    &[arg, &available.join(" · ")],
                ),
            );
            return Action::None;
        }
        self.config.seed_template = Some(arg.to_string());
        if let Err(e) = crate::config::save_to(&self.config, &self.config_path) {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(
                    self.lang,
                    "config.write_failed",
                    &[&self.config_path.display().to_string(), &e.to_string()],
                ),
            );
        }
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(self.lang, "template.switched", &[arg]),
        );
        self.refresh_status();
        Action::None
    }

    fn open_design_picker_overlay(&mut self, available: &[String]) {
        let current = self.config.design_system.as_deref().unwrap_or("");
        let mut body = String::from("Design Systems\n==============\n\n");
        if available.is_empty() {
            body.push_str("No design systems found.\nRun /init to scaffold them into knowledge/design-systems/\n");
        } else {
            body.push_str("Usage: /design <name>\n\n");
            for name in available {
                let mark = if name == current { "●" } else { "[pending]" };
                let path = self
                    .project_root
                    .join("knowledge/design-systems")
                    .join(format!("{name}.md"));
                let preview = Self::extract_design_preview_static(&path);
                body.push_str(&format!("{mark} {name}\n{preview}\n\n"));
            }
        }
        self.overlay = Some(Overlay::from_body(
            " /design — pick a design system · Esc close ",
            &body,
        ));
    }

    fn read_design_system_preview(&self, name: &str) -> String {
        let path = self
            .project_root
            .join("knowledge/design-systems")
            .join(format!("{name}.md"));
        Self::extract_design_preview_static(&path)
    }

    fn extract_design_preview_static(path: &std::path::Path) -> String {
        let Ok(content) = std::fs::read_to_string(path) else {
            return "  (file not readable)".to_string();
        };
        let mut preview = String::new();

        for line in content.lines() {
            if let Some(desc) = line.strip_prefix("> ") {
                preview.push_str(&format!("  {desc}\n"));
                break;
            }
        }

        // Extract "When to use" section
        let mut in_when = false;
        for line in content.lines() {
            if line.starts_with("## When to use") {
                in_when = true;
                continue;
            }
            if in_when {
                if line.starts_with("## ") {
                    break;
                }
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    preview.push_str(&format!("  Use: {trimmed}\n"));
                    break;
                }
            }
        }

        // Extract key colors from :root block
        let mut colors = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("--color-bg:")
                || trimmed.starts_with("--color-primary:")
                || trimmed.starts_with("--color-text:")
                || trimmed.starts_with("--color-accent:")
            {
                if let Some(val) = trimmed.split(':').nth(1) {
                    let hex = val.trim().trim_end_matches(';').trim();
                    let name = trimmed
                        .split(':')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .trim_start_matches('-');
                    colors.push(format!("{name}: {hex}"));
                }
            }
            if colors.len() >= 4 {
                break;
            }
        }
        if !colors.is_empty() {
            preview.push_str(&format!("  Palette: {}\n", colors.join(" · ")));
        }

        // Extract font families
        for line in content.lines() {
            if line.contains("**Headings**:") || line.contains("**Body**:") {
                let trimmed = line.trim().trim_start_matches("- ");
                preview.push_str(&format!("  {trimmed}\n"));
            }
        }

        // Count total tokens
        let token_count = content.matches("--").count();
        preview.push_str(&format!("  Tokens: {token_count} CSS variables"));

        preview
    }

    fn list_design_systems(&self) -> Vec<String> {
        let dir = self.project_root.join("knowledge/design-systems");
        Self::list_md_stems(&dir)
    }

    fn list_seed_templates(&self) -> Vec<String> {
        let dir = self.project_root.join("knowledge/seed-templates");
        Self::list_md_stems(&dir)
    }

    fn list_md_stems(dir: &std::path::Path) -> Vec<String> {
        let mut names = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.extension().and_then(|s| s.to_str()) == Some("md") {
                    if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                        if !stem.starts_with("00-") {
                            names.push(stem.to_string());
                        }
                    }
                }
            }
        }
        names.sort();
        names
    }

    fn slash_run(&mut self, arg: &str) -> Action {
        if self.is_pipeline_active() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "run.busy_reopen"),
            );
            return Action::None;
        }
        if arg.is_empty() {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.usage"));
            return Action::None;
        }
        let (slug, req) = if let Some((first, rest)) = arg.split_once(' ') {
            if rest.trim().is_empty() {
                (String::new(), first.to_string())
            } else {
                (first.to_string(), rest.trim().to_string())
            }
        } else {
            (String::new(), arg.to_string())
        };
        if !slug.is_empty() {
            if slug.contains(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_') {
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "run.slug_invalid", &[&slug]),
                );
                return Action::None;
            }
            self.slug = slug;
        }
        if self.run_started {
            self.reset_for_new_run();
        }
        self.maybe_suggest_design();
        self.push_preflight(&req);
        Action::StartRun(req)
    }

    fn open_status_overlay(&mut self) {
        let mut body = String::from("Pipeline Status\n===============\n\n");
        body.push_str(&format!("worker:        {}\n", self.backend_label));
        body.push_str(&format!(
            "design system: {}\n",
            self.config.design_system.as_deref().unwrap_or("(none)")
        ));
        body.push_str(&format!(
            "seed template: {}\n",
            self.config.seed_template.as_deref().unwrap_or("(auto)")
        ));
        body.push_str(&format!(
            "slug:          {}\n",
            if self.slug.is_empty() {
                "(not set)"
            } else {
                &self.slug
            }
        ));
        body.push_str(&format!(
            "requirement:   {}\n",
            if self.requirement.is_empty() {
                "(none yet)"
            } else {
                &self.requirement
            }
        ));
        body.push_str("\n## Pipeline phases\n\n");
        body.push_str("| # | Phase | Status |\n|---|---|---|\n");
        for (i, row) in self.phases.iter().enumerate() {
            let icon = match row.status {
                PhaseStatus::Done => "[ok]",
                PhaseStatus::Running => "[running]",
                PhaseStatus::Pending => "[pending]",
            };
            body.push_str(&format!("| {} | {} | {} |\n", i + 1, row.phase.id(), icon));
        }
        if let Some(gate) = self.active_gate {
            body.push_str(&format!("\n[gate] Active gate: `{}`\n", gate.id_str()));
        }
        if self.finished {
            body.push_str("\n[ok] Pipeline complete — proof-pack in release/\n");
        }
        // Artifacts
        let output_dir = self.project_root.join("output");
        if output_dir.is_dir() {
            body.push_str("\n## Artifacts\n\n");
            if let Ok(rd) = std::fs::read_dir(&output_dir) {
                let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
                entries.sort_by_key(std::fs::DirEntry::file_name);
                for e in entries.iter().take(20) {
                    let name = e.file_name();
                    let size = std::fs::metadata(e.path()).map_or(0, |m| m.len());
                    body.push_str(&format!(
                        "  · {} ({} bytes)\n",
                        name.to_string_lossy(),
                        size
                    ));
                }
            }
        }
        // Quality gate results
        let qg_path = output_dir.join(format!("{}-quality-gate.json", self.slug));
        if qg_path.is_file() {
            body.push_str("\n## Quality gate\n\n");
            if let Ok(qg_content) = std::fs::read_to_string(&qg_path) {
                let score = crate::app::extract_json_number(&qg_content, "score");
                let passed = crate::app::extract_json_bool(&qg_content, "passed");
                body.push_str(&format!(
                    "  Score: {}/100 · {}\n",
                    score.map_or("?".to_string(), |n| n.to_string()),
                    match passed {
                        Some(true) => "PASSED [ok]",
                        Some(false) => "BLOCKED [fail]",
                        None => "?",
                    }
                ));
            }
        }

        // Knowledge RAG info — reflect the configured retrieval engine.
        let project_cfg = umadev_agent::config::load_project_config(&self.project_root);
        let rag_engine =
            if project_cfg.knowledge.enabled && project_cfg.knowledge.engine == "hybrid" {
                "BM25 + vector hybrid (RRF-fused)"
            } else if project_cfg.knowledge.enabled {
                "BM25 (keyword inverted index)"
            } else {
                "keyword-scoring (legacy)"
            };
        body.push_str(&format!("\n## Knowledge RAG ({rag_engine})\n\n"));
        body.push_str("| Phase | Knowledge domains |\n|---|---|\n");
        body.push_str("| research | ALL (whole-tree scan) |\n");
        body.push_str("| docs | product, architecture, design, frontend, industries |\n");
        body.push_str("| spec | development, governance, product |\n");
        body.push_str("| frontend | frontend, design, design-systems, seed-templates |\n");
        body.push_str("| backend | backend, api, database, security, cloud-native |\n");
        body.push_str("| quality | testing, security, governance |\n");
        body.push_str("| delivery | cicd, operations, governance, security |\n");

        self.overlay = Some(Overlay::from_body(" status — Esc close ", &body));
    }

    fn slash_export(&mut self) {
        let release = self.project_root.join("release");
        if !release.is_dir() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "export.no_release_dir"),
            );
            return;
        }
        let mut zips: Vec<_> = std::fs::read_dir(&release)
            .ok()
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("zip"))
                    .collect()
            })
            .unwrap_or_default();
        zips.sort_by_key(std::fs::DirEntry::file_name);
        if zips.is_empty() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "export.release_empty"),
            );
            return;
        }
        let latest = zips.last().unwrap();
        let size = std::fs::metadata(latest.path()).map_or(0, |m| m.len() / 1024);
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(
                self.lang,
                "export.latest_proof_pack",
                &[
                    &latest.file_name().to_string_lossy(),
                    &size.to_string(),
                    &latest.path().display().to_string(),
                    &latest.path().display().to_string(),
                ],
            ),
        );
    }

    /// Run a `umadev` CLI subcommand and return its stdout output.
    /// Used by `/mcp`, `/skill` etc. to surface CLI results in the TUI.
    fn run_subprocess_cli(&self, args: &str) -> String {
        let bin = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("umadev"));
        let mut cmd = std::process::Command::new(&bin);
        cmd.current_dir(&self.project_root);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        for arg in args.split_whitespace() {
            cmd.arg(arg);
        }
        match cmd.output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                if !stdout.is_empty() {
                    stdout
                } else if !stderr.is_empty() {
                    stderr
                } else {
                    "(no output)".into()
                }
            }
            Err(e) => format!("error running `umadev {args}`: {e}"),
        }
    }

    fn open_knowledge_overlay(&mut self) {
        let mut body = String::from("knowledge base\n==============\n\n");
        // Design systems
        body.push_str("## Design systems (knowledge/design-systems/)\n\n");
        let ds = self.list_design_systems();
        if ds.is_empty() {
            body.push_str("  (none found)\n");
        } else {
            let active = self.config.design_system.as_deref().unwrap_or("");
            for name in &ds {
                let mark = if name == active { "●" } else { "[pending]" };
                body.push_str(&format!("  {mark} {name}\n"));
            }
        }
        // Seed templates
        body.push_str("\n## Seed templates (knowledge/seed-templates/)\n\n");
        let tpl = self.list_seed_templates();
        if tpl.is_empty() {
            body.push_str("  (none found)\n");
        } else {
            let active = self.config.seed_template.as_deref().unwrap_or("");
            for name in &tpl {
                let mark = if name == active { "●" } else { "[pending]" };
                body.push_str(&format!("  {mark} {name}\n"));
            }
        }
        // General knowledge
        body.push_str("\n## Knowledge files (knowledge/)\n\n");
        let kdir = self.project_root.join("knowledge");
        if kdir.is_dir() {
            let mut count = 0;
            if let Ok(rd) = std::fs::read_dir(&kdir) {
                let mut dirs: Vec<_> = rd
                    .filter_map(Result::ok)
                    .filter(|e| e.path().is_dir())
                    .collect();
                dirs.sort_by_key(std::fs::DirEntry::file_name);
                for d in &dirs {
                    let name = d.file_name();
                    let n = name.to_string_lossy();
                    if n == "design-systems" || n == "seed-templates" {
                        continue;
                    }
                    let file_count = std::fs::read_dir(d.path()).map_or(0, Iterator::count);
                    body.push_str(&format!("  [dir] {n}/ ({file_count} files)\n"));
                    count += file_count;
                }
            }
            body.push_str(&format!("\n  Total: {count} knowledge files\n"));
        } else {
            body.push_str("  (no knowledge/ directory)\n");
        }
        self.overlay = Some(Overlay::from_body(" knowledge — Esc close ", &body));
    }

    /// `/checkpoint [label]` — snapshot the workspace FILES so a whole phase's
    /// work can be rewound later (shadow git, never touches the user's `.git`).
    fn slash_checkpoint(&mut self, label: &str) -> Action {
        let label = if label.trim().is_empty() {
            umadev_i18n::t(self.lang, "checkpoint.manual_label").to_string()
        } else {
            label.trim().to_string()
        };
        match umadev_agent::checkpoint::create_checkpoint(&self.project_root, &label) {
            Some(id) => self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "checkpoint.created", &[&id, &label, &id]),
            ),
            None => self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "checkpoint.git_required").to_string(),
            ),
        }
        Action::None
    }

    /// `/rewind` lists file checkpoints; `/rewind <id>` rewinds the workspace
    /// files to that checkpoint (the present is auto-checkpointed first, so the
    /// rewind is itself undoable).
    fn slash_rewind(&mut self, arg: &str) -> Action {
        let arg = arg.trim();
        if arg.is_empty() {
            let list = umadev_agent::checkpoint::list_checkpoints(&self.project_root);
            if list.is_empty() {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "rewind.empty").to_string(),
                );
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

    /// `/quick <task>` — the lightweight fast track. Skips the heavy phases and
    /// runs a lean single shot (spec-lite -> implement -> quality, no gates) for
    /// a trivial change. Mirrors [`Self::slash_run`]'s guards.
    fn slash_quick(&mut self, arg: &str) -> Action {
        if self.is_pipeline_active() {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "quick.busy"));
            return Action::None;
        }
        let task = arg.trim();
        if task.is_empty() {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "quick.usage"));
            return Action::None;
        }
        if self.run_started {
            self.reset_for_new_run();
        }
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(self.lang, "quick.starting", &[task]),
        );
        Action::StartQuick(task.to_string())
    }

    /// `/redo [phase]` — with NO argument, re-run the whole requirement from
    /// scratch (the original behaviour). With a phase name, re-run just that ONE
    /// phase using the prior run's context (e.g. recover a base-offline degrade).
    fn slash_redo(&mut self, arg: &str) -> Action {
        if self.is_pipeline_active() {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "redo.busy"));
            return Action::None;
        }
        let arg = arg.trim();
        // Single-phase redo: `/redo frontend`, `/redo backend`, …
        if !arg.is_empty() {
            let valid = umadev_agent::redoable_phase_ids().join(", ");
            let Some(phase) = umadev_agent::phase_from_id(arg) else {
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "redo.phase.unknown", &[arg, &valid]),
                );
                return Action::None;
            };
            if self.requirement.is_empty() && !self.run_started {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "redo.phase.no_run"),
                );
                return Action::None;
            }
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(self.lang, "redo.phase.rerunning", &[phase.id()]),
            );
            return Action::RedoPhase(phase);
        }
        // No phase → re-run the whole requirement from scratch (legacy behaviour).
        if self.requirement.is_empty() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "redo.no_requirement"),
            );
            return Action::None;
        }
        let req = self.requirement.clone();
        self.reset_for_new_run();
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(self.lang, "redo.rerunning", &[&req]),
        );
        self.push_preflight(&req);
        Action::StartRun(req)
    }

    fn open_config_overlay(&mut self) {
        let mut body = String::from("Configuration\n=============\n\n");
        body.push_str(&format!(
            "worker:          {}\n",
            self.config
                .backend
                .as_deref()
                .unwrap_or("(use picker to select)")
        ));
        body.push_str(&format!(
            "model:           {}\n",
            self.config
                .model
                .as_deref()
                .unwrap_or("(default for worker)")
        ));
        body.push_str(&format!(
            "design system:   {}\n",
            self.config
                .design_system
                .as_deref()
                .unwrap_or("(none — /design to pick)")
        ));
        body.push_str(&format!(
            "seed template:   {}\n",
            self.config
                .seed_template
                .as_deref()
                .unwrap_or("(auto-detect)")
        ));
        body.push_str(&format!(
            "slug:            {}\n",
            if self.slug.is_empty() {
                "(auto from dir name)"
            } else {
                &self.slug
            }
        ));
        body.push_str(&format!(
            "workspace:       {}\n",
            self.project_root.display()
        ));
        body.push_str(&format!(
            "config file:     {}\n",
            self.config_path.display()
        ));
        body.push_str(&format!(
            "input history:   {}\n",
            self.history_path().display()
        ));
        body.push_str(&format!("history entries: {}\n", self.input_history.len()));

        // .umadevrc project config
        let rc_path = self.project_root.join(".umadevrc");
        if rc_path.is_file() {
            let cfg = umadev_agent::config::load_project_config(&self.project_root);
            body.push_str("\n## Project Config (.umadevrc)\n\n");
            body.push_str(&format!("quality threshold:   {}\n", cfg.quality.threshold));
            body.push_str(&format!(
                "max review rounds:   {}\n",
                cfg.pipeline.max_review_rounds
            ));
            if !cfg.pipeline.skip_phases.is_empty() {
                body.push_str(&format!(
                    "skip phases:         {}\n",
                    cfg.pipeline.skip_phases.join(", ")
                ));
            }
            if !cfg.quality.skip_checks.is_empty() {
                body.push_str(&format!(
                    "skip checks:         {}\n",
                    cfg.quality.skip_checks.join(", ")
                ));
            }
            if let Some(ref ck) = cfg.experts.custom_knowledge {
                body.push_str(&format!("custom knowledge:    {ck}\n"));
            }
        } else {
            body.push_str("\n## Project Config\n\n");
            body.push_str("  no .umadevrc — using defaults (threshold=90, rounds=3)\n");
        }

        body.push_str("\n## How to change\n\n");
        body.push_str("  /claude /codex /opencode      switch base CLI (or /offline)\n");
        body.push_str("  /manual  /auto                review mode: pause vs autonomous\n");
        body.push_str("  /model <id>                   override the base's model\n");
        body.push_str("  /design <name>                switch design system\n");
        body.push_str("  /template <name>              switch seed template\n");
        body.push_str("  /run <slug> <req>             set slug + requirement\n");
        body.push_str("  edit .umadevrc               project-level overrides\n");
        self.overlay = Some(Overlay::from_body(" config — Esc close ", &body));
    }

    fn scaffold_design_files(&self) -> usize {
        let files: &[(&str, &str)] = &[
            (
                "knowledge/design-systems/modern-minimal.md",
                include_str!("../../../knowledge/design-systems/modern-minimal.md"),
            ),
            (
                "knowledge/design-systems/editorial-clean.md",
                include_str!("../../../knowledge/design-systems/editorial-clean.md"),
            ),
            (
                "knowledge/design-systems/tech-utility.md",
                include_str!("../../../knowledge/design-systems/tech-utility.md"),
            ),
            (
                "knowledge/design-systems/soft-warm.md",
                include_str!("../../../knowledge/design-systems/soft-warm.md"),
            ),
            (
                "knowledge/design-systems/bold-geometric.md",
                include_str!("../../../knowledge/design-systems/bold-geometric.md"),
            ),
            (
                "knowledge/design-systems/00-craft-rules.md",
                include_str!("../../../knowledge/design-systems/00-craft-rules.md"),
            ),
            (
                "knowledge/seed-templates/saas-landing.md",
                include_str!("../../../knowledge/seed-templates/saas-landing.md"),
            ),
            (
                "knowledge/seed-templates/dashboard.md",
                include_str!("../../../knowledge/seed-templates/dashboard.md"),
            ),
            (
                "knowledge/seed-templates/blog-content.md",
                include_str!("../../../knowledge/seed-templates/blog-content.md"),
            ),
            (
                "knowledge/seed-templates/e-commerce.md",
                include_str!("../../../knowledge/seed-templates/e-commerce.md"),
            ),
            (
                "knowledge/seed-templates/auth-system.md",
                include_str!("../../../knowledge/seed-templates/auth-system.md"),
            ),
            (
                "knowledge/seed-templates/settings-page.md",
                include_str!("../../../knowledge/seed-templates/settings-page.md"),
            ),
            (
                "knowledge/seed-templates/docs-site.md",
                include_str!("../../../knowledge/seed-templates/docs-site.md"),
            ),
            (
                "knowledge/experts/product-manager/methodology.md",
                include_str!("../../../knowledge/experts/product-manager/methodology.md"),
            ),
            (
                "knowledge/experts/architect/api-design.md",
                include_str!("../../../knowledge/experts/architect/api-design.md"),
            ),
            (
                "knowledge/experts/architect/security.md",
                include_str!("../../../knowledge/experts/architect/security.md"),
            ),
            (
                "knowledge/experts/frontend-lead/methodology.md",
                include_str!("../../../knowledge/experts/frontend-lead/methodology.md"),
            ),
            (
                "knowledge/experts/backend-lead/methodology.md",
                include_str!("../../../knowledge/experts/backend-lead/methodology.md"),
            ),
            (
                "knowledge/experts/qa-lead/test-strategy.md",
                include_str!("../../../knowledge/experts/qa-lead/test-strategy.md"),
            ),
            (
                "knowledge/experts/uiux-designer/methodology.md",
                include_str!("../../../knowledge/experts/uiux-designer/methodology.md"),
            ),
            (
                "knowledge/experts/devops/methodology.md",
                include_str!("../../../knowledge/experts/devops/methodology.md"),
            ),
        ];
        let mut count = 0;
        for (rel, content) in files {
            let target = self.project_root.join(rel);
            if target.exists() {
                continue;
            }
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(&target, content).is_ok() {
                count += 1;
            }
        }
        count
    }

    fn open_runs_overlay(&mut self) {
        let path = self.project_root.join(".umadev/runs.jsonl");
        let mut body = String::from("Run History\n===========\n\n");
        match std::fs::read_to_string(&path) {
            Ok(content) if !content.trim().is_empty() => {
                body.push_str("| # | Timestamp | Slug | Quality | Artifacts |\n");
                body.push_str("|---|---|---|---|---|\n");
                for (i, line) in content.lines().rev().take(20).enumerate() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                        let ts = v["timestamp"].as_str().unwrap_or("?");
                        let slug = v["slug"].as_str().unwrap_or("?");
                        let passed = if v["quality_passed"].as_bool().unwrap_or(false) {
                            "[ok] PASS"
                        } else {
                            "[fail] FAIL"
                        };
                        let count = v["artifact_count"].as_u64().unwrap_or(0);
                        body.push_str(&format!(
                            "| {} | {} | {} | {} | {} |\n",
                            i + 1,
                            ts.chars().take(16).collect::<String>(),
                            slug,
                            passed,
                            count
                        ));
                    }
                }
            }
            _ => {
                body.push_str("No runs yet. Start one by typing a requirement.\n");
            }
        }
        // Phase timing
        let timing_path = self.project_root.join(".umadev/phase-timing.jsonl");
        if let Ok(content) = std::fs::read_to_string(&timing_path) {
            body.push_str("\n## Phase Timing (latest run)\n\n");
            body.push_str("| Phase | Duration |\n|---|---|\n");
            for line in content.lines().rev().take(9) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    let phase = v["phase"].as_str().unwrap_or("?");
                    let ms = v["elapsed_ms"].as_u64().unwrap_or(0);
                    #[allow(clippy::cast_precision_loss)]
                    let secs = ms as f64 / 1000.0;
                    body.push_str(&format!("| {phase} | {secs:.1}s |\n"));
                }
            }
        }
        self.overlay = Some(Overlay::from_body(
            " /runs — run history · Esc close ",
            &body,
        ));
    }

    fn open_version_overlay(&mut self) {
        let mut body = String::new();
        body.push_str("UmaDev — version\n");
        body.push_str("===================\n\n");
        body.push_str(&format!(
            "binary       umadev {} (built from rev {})\n",
            env!("CARGO_PKG_VERSION"),
            option_env!("VERGEN_GIT_SHA").unwrap_or("unreleased"),
        ));
        body.push_str(&format!("spec         {}\n", umadev_spec::SPEC_VERSION));
        body.push_str(&format!("worker       {}\n", self.backend_label));
        let driving = self.effective_model();
        if driving.is_empty() {
            body.push_str(&format!(
                "model        {} login default\n",
                self.backend_label
            ));
        } else {
            body.push_str(&format!("model        {driving}\n"));
        }
        if let Some(b) = self.backend.as_deref() {
            if !b.is_empty() && b != "offline" {
                if let Some(r) = crate::detect_base_reasoning(b, &self.project_root) {
                    body.push_str(&format!("reasoning    {r} (from {b}, not overridden)\n"));
                }
            }
        }
        body.push_str(&format!(
            "design       {}\n",
            self.config
                .design_system
                .as_deref()
                .unwrap_or("(none — use /design to pick)")
        ));
        body.push_str(&format!(
            "template     {}\n",
            self.config
                .seed_template
                .as_deref()
                .unwrap_or("(auto-detect)")
        ));
        body.push_str(&format!("workspace    {}\n", self.project_root.display()));
        body.push_str(&format!("config       {}\n", self.config_path.display()));
        body.push('\n');
        body.push_str("Project home: https://github.com/umacloud/umadev\n");
        self.overlay = Some(Overlay::from_body(" version — Esc close ", &body));
    }

    fn open_changelog_overlay(&mut self) {
        // Embedded at compile time so the overlay matches the binary,
        // not whatever CHANGELOG.md happens to be in the user's cwd.
        let body = include_str!("../../../CHANGELOG.md");
        self.overlay = Some(Overlay::from_body(
            " CHANGELOG — Esc close, ↑↓ scroll ",
            body,
        ));
    }

    // ---- overlays --------------------------------------------------------

    fn overlay_key(&mut self, key: KeyCode) -> Action {
        let Some(ov) = self.overlay.as_mut() else {
            return Action::None;
        };
        match key {
            KeyCode::Esc | KeyCode::Char('q' | 'Q') => {
                self.overlay = None;
            }
            KeyCode::Down | KeyCode::Char('j' | 'J') => ov.scroll_down(1),
            KeyCode::Up | KeyCode::Char('k' | 'K') => ov.scroll_up(1),
            KeyCode::PageDown | KeyCode::Char(' ') => ov.scroll_down(10),
            KeyCode::PageUp => ov.scroll_up(10),
            KeyCode::Home | KeyCode::Char('g') => ov.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => {
                ov.scroll = ov.lines.len().saturating_sub(1);
            }
            _ => {}
        }
        Action::None
    }

    fn open_spec_overlay(&mut self) {
        // Embedded at compile-time via include_str! from the same file
        // `umadev spec` prints, so the overlay is always fresh.
        let body = include_str!("../../../spec/UMADEV_HOST_SPEC_V1.md");
        self.overlay = Some(Overlay::from_body(
            umadev_i18n::t(self.lang, "spec.overlay_title"),
            body,
        ));
    }

    fn open_doctor_overlay(&mut self) {
        let lang = self.lang;
        let mut body = format!("{}\n======\n\n", umadev_i18n::t(lang, "doctor.heading"));
        body.push_str(&umadev_i18n::tf(
            lang,
            "doctor.binary",
            &[env!("CARGO_PKG_VERSION"), umadev_spec::SPEC_VERSION],
        ));
        body.push('\n');
        body.push_str(&umadev_i18n::tf(
            lang,
            "doctor.workspace",
            &[&self.project_root.display().to_string()],
        ));
        body.push('\n');
        body.push_str(&umadev_i18n::tf(
            lang,
            "doctor.worker",
            &[&self.backend_label],
        ));
        body.push('\n');
        // Spec manifest
        let manifest = umadev_agent::SpecManifest::read_from(&self.project_root);
        if let Some(m) = manifest {
            body.push_str(&umadev_i18n::tf(
                lang,
                "doctor.manifest_present",
                &[m.level.as_str(), m.profile.as_str()],
            ));
            body.push('\n');
        } else {
            body.push_str(umadev_i18n::t(lang, "doctor.manifest_missing"));
            body.push('\n');
        }
        // Backend probes
        body.push('\n');
        body.push_str(umadev_i18n::t(lang, "doctor.worker_availability"));
        body.push('\n');
        if self.backends.is_empty() {
            body.push_str(umadev_i18n::t(lang, "doctor.probing"));
            body.push('\n');
        } else {
            for b in &self.backends {
                let mark = if b.ready { "[ok]" } else { "[fail]" };
                body.push_str(&format!("  {mark} {:<14} {}\n", b.id, b.detail));
            }
        }
        // Design systems + seed templates
        body.push('\n');
        body.push_str(umadev_i18n::t(lang, "doctor.design_infra"));
        body.push('\n');
        let ds_list = self.list_design_systems();
        if ds_list.is_empty() {
            body.push_str(umadev_i18n::t(lang, "doctor.no_design_systems"));
            body.push('\n');
        } else {
            let active = self.config.design_system.as_deref().unwrap_or("");
            for ds in &ds_list {
                let mark = if ds == active { "●" } else { "[pending]" };
                body.push_str(&format!("  {mark} {ds}\n"));
            }
        }
        let tpl_list = self.list_seed_templates();
        if !tpl_list.is_empty() {
            let active = self.config.seed_template.as_deref().unwrap_or("");
            body.push_str(umadev_i18n::t(lang, "doctor.templates_label"));
            let labels: Vec<String> = tpl_list
                .iter()
                .map(|t| {
                    if t == active {
                        format!("[{t}]")
                    } else {
                        t.clone()
                    }
                })
                .collect();
            body.push_str(&labels.join(" · "));
            body.push('\n');
        }

        // Knowledge base health
        body.push('\n');
        body.push_str(umadev_i18n::t(lang, "doctor.knowledge_base"));
        body.push('\n');
        let experts_dir = self.project_root.join("knowledge/experts");
        if experts_dir.is_dir() {
            let roles: Vec<_> = std::fs::read_dir(&experts_dir)
                .ok()
                .map(|rd| {
                    rd.filter_map(Result::ok)
                        .filter(|e| e.path().is_dir())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            body.push_str(&umadev_i18n::tf(
                lang,
                "doctor.expert_roles",
                &[&roles.len().to_string(), &roles.join(", ")],
            ));
            body.push('\n');
        } else {
            body.push_str(umadev_i18n::t(lang, "doctor.no_experts"));
            body.push('\n');
        }
        let knowledge_dir = self.project_root.join("knowledge");
        if knowledge_dir.is_dir() {
            let md_count = walkdir_count_md(&knowledge_dir);
            body.push_str(&umadev_i18n::tf(
                lang,
                "doctor.knowledge_files",
                &[&md_count.to_string()],
            ));
            body.push('\n');
        }

        // .umadevrc
        let rc_path = self.project_root.join(".umadevrc");
        body.push('\n');
        body.push_str(umadev_i18n::t(lang, "doctor.project_config"));
        body.push('\n');
        if rc_path.is_file() {
            let cfg = umadev_agent::config::load_project_config(&self.project_root);
            body.push_str(&umadev_i18n::tf(
                lang,
                "doctor.rc_present",
                &[
                    &cfg.quality.threshold.to_string(),
                    &cfg.pipeline.max_review_rounds.to_string(),
                ],
            ));
            body.push('\n');
        } else {
            body.push_str(umadev_i18n::t(lang, "doctor.rc_missing"));
            body.push('\n');
        }

        // Audit trail
        body.push('\n');
        body.push_str(umadev_i18n::t(lang, "doctor.audit_trail"));
        body.push('\n');
        let audit_dir = self.project_root.join(".umadev/audit");
        if audit_dir.is_dir() {
            for name in [
                "tool-calls.jsonl",
                "frontend-api-calls.jsonl",
                "verify.jsonl",
            ] {
                let p = audit_dir.join(name);
                if p.is_file() {
                    let lines = std::fs::read_to_string(&p)
                        .map_or(0, |t| t.lines().filter(|l| !l.trim().is_empty()).count());
                    body.push_str(&umadev_i18n::tf(
                        lang,
                        "doctor.audit_present",
                        &[name, &lines.to_string()],
                    ));
                    body.push('\n');
                } else {
                    body.push_str(&umadev_i18n::tf(lang, "doctor.audit_missing", &[name]));
                    body.push('\n');
                }
            }
        } else {
            body.push_str(umadev_i18n::t(lang, "doctor.no_audit"));
            body.push('\n');
        }

        self.overlay = Some(Overlay::from_body(
            umadev_i18n::t(lang, "doctor.overlay_title"),
            &body,
        ));
    }

    fn open_verify_overlay(&mut self) {
        let mut body = String::from(
            "Workspace Verify\n\
             ================\n\n",
        );
        body.push_str(&format!("workspace: {}\n\n", self.project_root.display()));

        // Spec manifest section
        body.push_str("## Spec manifest (UD-META-001)\n");
        match umadev_agent::SpecManifest::read_from(&self.project_root) {
            Some(m) => body.push_str(&format!(
                "  version={} level={} profile={} declared_by={}\n",
                m.spec_version,
                m.level.as_str(),
                m.profile.as_str(),
                m.declared_by,
            )),
            None => body.push_str("  <missing — type /init to create>\n"),
        }

        // Workflow state
        body.push_str("\n## Workflow state\n");
        match umadev_agent::read_workflow_state(&self.project_root) {
            Some(s) => body.push_str(&format!(
                "  phase={} active_gate={} worker={} slug={}\n  requirement={}\n",
                s.phase,
                if s.active_gate.is_empty() {
                    "<none>"
                } else {
                    &s.active_gate
                },
                if s.backend.is_empty() {
                    "offline-templates"
                } else {
                    s.backend.as_str()
                },
                s.slug,
                s.requirement,
            )),
            None => body.push_str("  <none — pipeline has not run yet>\n"),
        }

        // Output directory contents
        body.push_str("\n## Artifacts (output/)\n");
        let output_dir = self.project_root.join("output");
        if output_dir.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(&output_dir)
                .ok()
                .map(|rd| rd.filter_map(Result::ok).collect())
                .unwrap_or_default();
            entries.sort_by_key(std::fs::DirEntry::file_name);
            if entries.is_empty() {
                body.push_str("  (empty)\n");
            } else {
                for e in entries.iter().take(20) {
                    body.push_str(&format!("  · {}\n", e.file_name().to_string_lossy()));
                }
            }
        } else {
            body.push_str("  (output/ not yet created)\n");
        }

        // Quality gate — quick verdict so users don't have to open the JSON.
        body.push_str("\n## Quality gate\n");
        let qg_paths: Vec<_> = std::fs::read_dir(&output_dir)
            .ok()
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .ends_with("-quality-gate.json")
                    })
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default();
        if qg_paths.is_empty() {
            body.push_str("  (quality phase has not produced a gate report yet)\n");
        } else {
            for p in &qg_paths {
                let score_line = match std::fs::read_to_string(p) {
                    Ok(s) => {
                        let score = extract_json_number(&s, "score")
                            .map_or_else(|| "?".to_string(), |n| n.to_string());
                        let verdict = match extract_json_bool(&s, "passed") {
                            Some(true) => "PASSED",
                            Some(false) => "BLOCKED",
                            None => "?",
                        };
                        format!(
                            "  · {} → {score}/100 ({verdict})\n",
                            p.file_name().unwrap_or_default().to_string_lossy(),
                        )
                    }
                    Err(_) => format!("  · {} (unreadable)\n", p.display()),
                };
                body.push_str(&score_line);
            }
        }

        // Release proof-packs
        body.push_str("\n## Proof packs (release/)\n");
        let release = self.project_root.join("release");
        if release.is_dir() {
            let mut zips: Vec<_> = std::fs::read_dir(&release)
                .ok()
                .map(|rd| {
                    rd.filter_map(Result::ok)
                        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("zip"))
                        .collect()
                })
                .unwrap_or_default();
            zips.sort_by_key(std::fs::DirEntry::file_name);
            if zips.is_empty() {
                body.push_str("  (none — pipeline must reach delivery first)\n");
            } else {
                for z in zips.iter().rev().take(3) {
                    let size = std::fs::metadata(z.path()).map_or(0, |m| m.len() / 1024);
                    body.push_str(&format!(
                        "  · {} ({size} KiB)\n",
                        z.file_name().to_string_lossy()
                    ));
                }
            }
        } else {
            body.push_str("  (release/ not yet created)\n");
        }

        self.overlay = Some(Overlay::from_body(" verify — press Esc to close ", &body));
    }

    fn open_diff_overlay(&mut self, arg: &str) {
        let slug = if self.slug.is_empty() {
            "<slug>"
        } else {
            self.slug.as_str()
        };
        // Pick a sensible artifact: explicit arg → exact name; bare /diff
        // → the PRD (the most-asked-for read).
        let name = if arg.is_empty() { "prd" } else { arg };
        let candidate = self
            .project_root
            .join("output")
            .join(format!("{slug}-{name}.md"));
        let body = if let Ok(text) = std::fs::read_to_string(&candidate) {
            text
        } else {
            // Fallback: list available artifacts so the user can pick.
            let mut hint = umadev_i18n::tf(
                self.lang,
                "diff.not_found",
                &[&candidate.display().to_string()],
            );
            let output_dir = self.project_root.join("output");
            if let Ok(rd) = std::fs::read_dir(&output_dir) {
                for entry in rd.flatten() {
                    if entry.path().extension().and_then(|s| s.to_str()) == Some("md") {
                        hint.push_str(&format!("  · {}\n", entry.file_name().to_string_lossy()));
                    }
                }
            } else {
                hint.push_str(umadev_i18n::t(self.lang, "diff.no_output_dir"));
            }
            hint.push_str(umadev_i18n::t(self.lang, "diff.usage"));
            hint
        };
        self.overlay = Some(Overlay::from_body(
            format!(" diff: {} — Esc close ", candidate.display()),
            &body,
        ));
    }

    fn open_history_overlay(&mut self) {
        let mut body = String::new();
        for msg in &self.history {
            let label = match msg.role {
                ChatRole::You => "you",
                ChatRole::UmaDev => "umadev",
                ChatRole::Host => "worker",
                ChatRole::Gate => "GATE",
                ChatRole::System => "system",
            };
            body.push_str(&format!("[{label}] {}\n", msg.body));
            body.push('\n');
        }
        if body.is_empty() {
            body.push_str("(empty)");
        }
        self.overlay = Some(Overlay::from_body(
            " conversation history — Esc close, ↑↓ scroll ",
            &body,
        ));
    }

    fn commit_backend(&mut self, backend: Option<String>) {
        self.backend.clone_from(&backend);
        self.backend_label = backend.clone().unwrap_or_else(|| "offline".to_string());
        self.config.backend = Some(self.backend_label.clone());
        // A different base means a different session — don't resume the old
        // base's conversation into the new one.
        self.host_chat_session_active = false;
        self.chat_session_id = None;
        // Persist; failures only surface as a system message so the TUI
        // never panics on config save errors.
        if let Err(e) = crate::config::save_to(&self.config, &self.config_path) {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(
                    self.lang,
                    "config.write_failed",
                    &[&self.config_path.display().to_string(), &e.to_string()],
                ),
            );
        }
        // Sync the base's OWN model so the user sees what drives the Agent —
        // UmaDev owns no model. Read it from the base's config; if the base pins
        // none (pure login), the Agent still runs on the base's login default.
        if let Some(b) = self
            .backend
            .clone()
            .filter(|b| !b.is_empty() && b != "offline")
        {
            match crate::detect_base_model(&b, &self.project_root) {
                Some(m) => self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "model.synced", &[&m, &self.backend_label]),
                ),
                None => self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "model.synced_default", &[&self.backend_label]),
                ),
            }
            // The base's reasoning / thinking effort is shared too (we never
            // override it) — surface it so the user sees the full setup.
            if let Some(r) = crate::detect_base_reasoning(&b, &self.project_root) {
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "model.reasoning", &[&r]),
                );
            }
        }
    }

    /// Path to the frontend-notes markdown the worker writes (holds the
    /// `## Preview URL` + `## Run command` sections).
    fn frontend_notes_path(&self) -> std::path::PathBuf {
        self.project_root
            .join("output")
            .join(format!("{}-frontend-notes.md", self.slug))
    }

    /// Extract the `## Preview URL` value from the frontend-notes file.
    /// Returns `None` when the file is missing or the section is empty.
    #[must_use]
    pub fn preview_url_from_notes(&self) -> Option<String> {
        let body = std::fs::read_to_string(self.frontend_notes_path()).ok()?;
        parse_notes_section(&body, "Preview URL")
            .map(str::to_string)
            .filter(|u| u.starts_with("http"))
    }

    /// Extract the `## Run command` value from the frontend-notes file.
    #[must_use]
    pub fn run_command_from_notes(&self) -> Option<String> {
        let body = std::fs::read_to_string(self.frontend_notes_path()).ok()?;
        parse_notes_section(&body, "Run command").map(str::to_string)
    }

    /// `/preview` — read the Preview URL the worker recorded, start the dev
    /// server in the background, open the browser, and tell the user. Falls
    /// back to a clear hint when no notes / no URL yet.
    fn slash_preview(&mut self) -> Action {
        // If a server is already running, just re-open the browser.
        let already = self.preview_server.lock().is_ok_and(|g| g.is_some());
        if already {
            let url = self.effective_preview_url();
            if let Some(ref u) = url {
                let _ = open_browser(u);
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
        // forgot or used a different file name. Only falls back to the
        // worker-recorded URL when no manifest-based detection matches.
        let detected = umadev_agent::verify::detect_dev_server(&self.project_root);
        let url = self.effective_preview_url();
        let command = match (&detected, self.run_command_from_notes()) {
            // Self-detection wins — we control the command + know the URL.
            (Some(ds), _) => Some(ds.command.to_string()),
            // Worker recorded a run command — use it.
            (None, Some(cmd)) => Some(cmd),
            (None, None) => None,
        };

        match (detected.as_ref(), url.as_ref(), command.as_ref()) {
            (Some(ds), u, Some(cmd)) => {
                let display_url = u.cloned().unwrap_or_else(|| ds.default_url.to_string());
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::tf(
                        self.lang,
                        "preview.detected",
                        &[ds.label, cmd, &display_url],
                    ),
                );
                Action::StartPreview {
                    url: display_url,
                    command: cmd.clone(),
                }
            }
            (None, Some(u), Some(cmd)) => {
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::tf(self.lang, "preview.starting", &[u, cmd]),
                );
                Action::StartPreview {
                    url: u.clone(),
                    command: cmd.clone(),
                }
            }
            (None, Some(u), None) => {
                let _ = open_browser(u);
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "preview.opened", &[u]),
                );
                Action::None
            }
            _ => {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "preview.none_yet").to_string(),
                );
                Action::None
            }
        }
    }

    /// The Preview URL to actually open: prefer the worker-recorded value
    /// (it reflects the real port), fall back to the dev-server default
    /// (e.g. 5173 for Vite) when the worker did not record one.
    fn effective_preview_url(&self) -> Option<String> {
        if let Some(u) = self.preview_url_from_notes() {
            return Some(u);
        }
        umadev_agent::verify::detect_dev_server(&self.project_root)
            .map(|ds| ds.default_url.to_string())
    }

    /// `/stop-preview` — kill the background dev server if one is running.
    fn slash_stop_preview(&mut self) -> Action {
        let killed = self
            .preview_server
            .lock()
            .is_ok_and(|mut g| g.take().is_some_and(|mut c| c.start_kill().is_ok()));
        if killed {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "preview.stopped").to_string(),
            );
        } else {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "preview.none_running").to_string(),
            );
        }
        Action::None
    }

    /// Path to the delivery-notes markdown (holds deploy/URL/run sections).
    fn delivery_notes_path(&self) -> std::path::PathBuf {
        self.project_root
            .join("output")
            .join(format!("{}-delivery-notes.md", self.slug))
    }

    /// Read the `## Deploy command` the worker recorded.
    #[must_use]
    pub fn deploy_command_from_notes(&self) -> Option<String> {
        let body = std::fs::read_to_string(self.delivery_notes_path()).ok()?;
        parse_notes_section(&body, "Deploy command").map(str::to_string)
    }

    /// Read the `## Frontend URL` (live URL after a deploy).
    #[must_use]
    pub fn deploy_url_from_notes(&self) -> Option<String> {
        let body = std::fs::read_to_string(self.delivery_notes_path()).ok()?;
        parse_notes_section(&body, "Frontend URL")
            .map(str::to_string)
            .filter(|u| u.starts_with("http"))
    }

    /// `/deploy` — run the deploy command the worker recorded so the project
    /// goes live. The command typically logs into a platform CLI and pushes
    /// (e.g. `npx vercel --prod`). We run it in the foreground so its login
    /// prompts / output reach the user; the URL is surfaced after.
    fn slash_deploy(&mut self, arg: &str) -> Action {
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
        let confirmed = matches!(arg.trim(), "confirm" | "yes" | "go" | "y" | "确认" | "確認");
        if floor_requires_confirm && !confirmed {
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(self.lang, "deploy.confirm_preflight", &[&cmd]),
            );
            return Action::None;
        }
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(self.lang, "deploy.starting", &[&cmd]),
        );
        Action::RunDeploy { command: cmd }
    }

    /// `/animations` — toggle spinner animation on/off (accessibility).
    /// When off, the spinner shows a static `…` instead of braille dots.
    /// `/manual` (review = on) / `/auto` (autonomous) — flip whether the
    /// docs/preview gates pause for review this session. The Clarify gate
    /// always pauses regardless. Session-level override; for a permanent
    /// default set `auto_approve_gates` in `.umadevrc`.
    /// Shift+Tab cycles the gate-approval mode (auto <-> manual), Claude-Code
    /// style. The current mode shows in the prompt meta row.
    pub fn cycle_approval_mode(&mut self) {
        let auto = matches!(self.effective_trust_mode(), umadev_agent::TrustMode::Auto);
        self.slash_set_review_mode(!auto);
    }

    /// Resolve the active trust tier: an explicit `/mode` (or `/auto` /
    /// `/manual`) session override wins; otherwise derive from `.umadevrc`'s
    /// `auto_approve_gates` (`true` → `auto`, `false` → `guarded`). The default
    /// is `guarded` — the existing human-in-the-loop behaviour.
    #[must_use]
    pub fn effective_trust_mode(&self) -> umadev_agent::TrustMode {
        if let Some(m) = self.trust_mode_override {
            return m;
        }
        // Legacy binary override (set via `/auto` / `/manual` before any
        // `/mode`) still maps onto a tier for back-compat.
        if let Some(auto) = self.auto_approve_override {
            return if auto {
                umadev_agent::TrustMode::Auto
            } else {
                umadev_agent::TrustMode::Guarded
            };
        }
        let config_auto = umadev_agent::config::load_project_config(&self.project_root)
            .pipeline
            .auto_approve_gates;
        if config_auto {
            umadev_agent::TrustMode::Auto
        } else {
            umadev_agent::TrustMode::Guarded
        }
    }

    /// Whether gates currently auto-approve (true) or pause for review (false).
    /// Kept for the prompt meta-row chip + back-compat; `auto` tier → true.
    #[must_use]
    pub fn auto_approve_on(&self) -> bool {
        matches!(self.effective_trust_mode(), umadev_agent::TrustMode::Auto)
    }

    fn slash_set_review_mode(&mut self, auto: bool) -> Action {
        let mode = if auto {
            umadev_agent::TrustMode::Auto
        } else {
            umadev_agent::TrustMode::Guarded
        };
        self.set_trust_mode(mode);
        let msg = if auto {
            umadev_i18n::t(self.lang, "review.auto_on")
        } else {
            umadev_i18n::t(self.lang, "review.manual_on")
        };
        self.push(ChatRole::UmaDev, msg.to_string());
        Action::None
    }

    /// `/mode plan|guarded|auto` — set the trust / autonomy tier for this
    /// session. `plan` is read-only (research + plan, no execution); `guarded`
    /// (default) pauses at every gate; `auto` runs end-to-end. With no/unknown
    /// argument, print the current tier and the options.
    fn slash_mode(&mut self, rest: &str) -> Action {
        let arg = rest.trim();
        if arg.is_empty() {
            let cur = self.effective_trust_mode();
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(
                    self.lang,
                    "mode.current",
                    &[umadev_i18n::t(self.lang, cur.chip_key())],
                ),
            );
            return Action::None;
        }
        match umadev_agent::TrustMode::parse(arg) {
            Some(mode) => {
                self.set_trust_mode(mode);
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::t(self.lang, mode.desc_key()).to_string(),
                );
            }
            None => {
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::tf(self.lang, "mode.unknown", &[arg]),
                );
            }
        }
        Action::None
    }

    /// Apply a trust tier as the session override, keeping the legacy binary
    /// `auto_approve_override` consistent so the prompt chip + any old code path
    /// reads the same state.
    fn set_trust_mode(&mut self, mode: umadev_agent::TrustMode) {
        self.trust_mode_override = Some(mode);
        self.auto_approve_override = Some(mode.gates_auto_approve());
    }

    /// Record that a gate passed (auto or manual) into the per-project trust
    /// ledger, persist it, and — if this gate has now passed enough times in a
    /// row — SUGGEST (never auto-apply) letting it auto-advance next time. The
    /// whole path is fail-open: a ledger IO error never blocks the pipeline.
    fn record_trust_pass(&mut self, gate_id: &str) {
        if let Some(suggestion) = self.trust_ledger.record_pass(gate_id) {
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(
                    self.lang,
                    "trust.suggest_auto",
                    &[&suggestion.consecutive.to_string(), &suggestion.gate_id],
                ),
            );
        }
        self.trust_ledger.save(&self.project_root);
    }

    /// Record that a gate was revised — resets its consecutive-pass streak so a
    /// revision walks back the accumulated trust. Persisted, fail-open.
    fn record_trust_revision(&mut self, gate_id: &str) {
        self.trust_ledger.record_revision(gate_id);
        self.trust_ledger.save(&self.project_root);
    }

    /// Toggle mouse-wheel transcript scrolling. ON (default) lets the wheel
    /// page the history; OFF releases the terminal's native text selection /
    /// copy (which the mouse capture otherwise intercepts). The event loop reads
    /// `mouse_scroll` each turn and only routes wheel events when it's on; a
    /// Shift-held drag bypasses the capture in most terminals regardless.
    fn slash_toggle_mouse(&mut self) -> Action {
        self.mouse_scroll = !self.mouse_scroll;
        let key = if self.mouse_scroll {
            "slash.mouse_on"
        } else {
            "slash.mouse_off"
        };
        self.push(ChatRole::System, umadev_i18n::t(self.lang, key));
        // Drive the real terminal: ON re-enables capture so the wheel pages the
        // transcript; OFF actually issues DisableMouseCapture so native
        // click-drag selection works again. The event loop owns `terminal`.
        Action::SetMouseCapture(self.mouse_scroll)
    }

    fn slash_toggle_animations(&mut self) -> Action {
        let path = std::env::var("HOME")
            .map(|h| {
                std::path::PathBuf::from(h)
                    .join(".umadev")
                    .join("settings.json")
            })
            .unwrap_or_default();
        let current = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("animations_enabled")
                    .and_then(serde_json::Value::as_bool)
            })
            .unwrap_or(true);
        let new_val = !current;
        // Read-merge-write so toggling animations never clobbers sibling keys in
        // settings.json, and write atomically (temp+rename) so a crash mid-write
        // can't corrupt it.
        let mut root = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        root["animations_enabled"] = serde_json::json!(new_val);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(body) = serde_json::to_string_pretty(&root) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, body).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        let state = if new_val {
            umadev_i18n::t(self.lang, "anim.on")
        } else {
            umadev_i18n::t(self.lang, "anim.off")
        };
        let mode = if new_val {
            "braille dots"
        } else {
            umadev_i18n::t(self.lang, "anim.static")
        };
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "anim.toggled", &[state, mode]),
        );
        Action::None
    }

    /// `/usage` — show recorded worker token usage per run / per phase, with run
    /// totals, a grand total, and a rough advisory cost estimate. Pure read of
    /// the usage log (mirrors the `umadev usage` CLI verb).
    fn slash_usage(&mut self) -> Action {
        let body = format_usage_report(self.lang, &umadev_agent::runner::usage_report());
        self.push(ChatRole::System, body);
        Action::None
    }

    /// `/lessons` — make UmaDev's self-evolution visible: high-frequency
    /// pitfalls, the failed fixes it now steers away from, and validated
    /// success patterns. Pure read of `.umadev/learned/` (mirrors the
    /// `umadev lessons` CLI verb). Shown in a scrollable overlay.
    fn slash_lessons(&mut self) -> Action {
        let report = umadev_agent::lessons_report(&self.project_root);
        let body = format_lessons_report(self.lang, &report);
        self.overlay = Some(Overlay::from_body(
            umadev_i18n::t(self.lang, "lessons.title"),
            &body,
        ));
        Action::None
    }

    /// `/lang [zh-CN|zh-TW|en]` — show or switch the UI language. No arg lists
    /// the current language + options; an arg (code, native label, or a common
    /// shorthand) switches and persists it to config.
    fn slash_lang(&mut self, arg: &str) -> Action {
        use umadev_i18n::Lang;
        let arg = arg.trim();
        if arg.is_empty() {
            let cur = umadev_i18n::tf(self.lang, "lang.current", &[self.lang.label()]);
            let opts = Lang::ALL
                .iter()
                .map(|l| format!("  {} — {}", l.code(), l.label()))
                .collect::<Vec<_>>()
                .join("\n");
            let hint = umadev_i18n::t(self.lang, "lang.picker.hint");
            self.push(ChatRole::System, format!("{cur}\n{opts}\n{hint}"));
            return Action::None;
        }
        let pick = Lang::from_code(arg).or_else(|| match arg.to_lowercase().as_str() {
            "简体" | "简体中文" | "简" | "简中" => Some(Lang::ZhCn),
            "繁体" | "繁體" | "繁體中文" | "繁" | "繁中" => Some(Lang::ZhTw),
            "english" | "英文" | "英语" | "英語" => Some(Lang::En),
            _ => None,
        });
        let Some(lang) = pick else {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "lang.unknown"));
            return Action::None;
        };
        self.lang = lang;
        umadev_i18n::set_lang(lang);
        self.config.lang = Some(lang.code().to_string());
        let _ = crate::config::save_to(&self.config, &self.config_path);
        self.push(
            ChatRole::System,
            umadev_i18n::tf(lang, "lang.changed", &[lang.label()]),
        );
        Action::None
    }

    /// `/setup` — re-open the first-run guide (the logo + language + worker
    /// picker) so the user can reconfigure language / base without restarting.
    /// The event loop re-probes the host CLIs so their ready-state is fresh.
    fn slash_setup(&mut self) -> Action {
        // Restart the guided setup from step 1 (language).
        self.goto_picker_step(PickerStep::Language);
        self.mode = AppMode::Picker;
        Action::Reconfigure
    }

    /// `/bug` — collect diagnostics (version, backend, workspace
    /// state, recent usage, last error) into a file the user can
    /// attach to a bug report. Closes the error-feedback loop.
    fn slash_bug(&mut self) -> Action {
        let slug = &self.slug;
        let ws_state =
            std::fs::read_to_string(self.project_root.join(".umadev/workflow-state.json"))
                .unwrap_or_else(|_| "(no workflow state)".to_string());
        let usage = umadev_agent::runner::usage_summary();
        let backend = self
            .backend
            .clone()
            .unwrap_or_else(|| "offline".to_string());
        let report = format!(
            "# UmaDev bug report\n\n\
             version: {}\n\
             backend: {backend}\n\
             slug: {slug}\n\
             project_root: {}\n\n\
             ## workflow state\n\n{}\n\n\
             ## usage\n\n{usage}\n\n\
             ## last 10 chat messages\n\n{}",
            env!("CARGO_PKG_VERSION"),
            self.project_root.display(),
            ws_state,
            self.history
                .iter()
                .rev()
                .take(10)
                .rev()
                .map(|m| {
                    format!(
                        "- [{:?}] {}",
                        m.role,
                        m.body.chars().take(120).collect::<String>()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let report_path = self.project_root.join("umadev-bug-report.md");
        match std::fs::write(&report_path, &report) {
            Ok(()) => {
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(
                        self.lang,
                        "doctor.report_written",
                        &[&report_path.display().to_string()],
                    ),
                );
            }
            Err(e) => {
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "report.write_failed", &[&e.to_string()]),
                );
            }
        }
        Action::None
    }

    /// Called by `apply_engine` when the preview gate opens: surface the
    /// recorded URL so the user knows where to look before pressing `c`.
    /// Append the user's answer to `output/{slug}-clarify-answers.md`.
    /// Called during `ClarifyGate` so each answer is persisted; on resume
    /// `merged_requirement` reads this file and folds answers into the
    /// requirement for research.
    fn append_clarify_answer(&self, answer: &str) {
        let path = self
            .project_root
            .join("output")
            .join(format!("{}-clarify-answers.md", self.slug));
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let updated = if existing.trim().is_empty() {
            answer.to_string()
        } else {
            format!("{existing}\n{answer}")
        };
        let _ = std::fs::write(&path, updated);
    }

    /// Called by `apply_engine` when the preview gate opens: surface the
    /// recorded URL so the user knows where to look.
    pub fn maybe_announce_preview(&mut self) {
        if let Some(url) = self.preview_url_from_notes() {
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(self.lang, "preview.gate_announce", &[&url]),
            );
        } else {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "preview.gate_no_url").to_string(),
            );
        }
    }

    /// Advance the spinner animation frame; status bar is regenerated
    /// so the spinner glyph actually rotates while a phase is running.
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.refresh_status();
    }

    /// Current spinner glyph for a running phase.
    #[must_use]
    pub fn spinner(&self) -> char {
        // Braille-dots spinner, one frame per tick (~80ms) so it visibly spins.
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        FRAMES[(self.tick as usize) % FRAMES.len()]
    }

    /// Animated glyph for the IN-PROGRESS phase circle in the progress bar.
    ///
    /// The bar previously drew a STATIC `◐` for the running phase, so the only
    /// motion on screen was the tiny bottom-bar spinner — when that was off the
    /// edge of attention the whole UI read as frozen. Rotating the running
    /// circle itself (`◐◓◑◒`) makes the bar prove it's alive on its own. The
    /// 80ms tick is faster than we want for the quarter-circle rotation (it
    /// would blur), so we step one frame per ~120ms by dividing the tick.
    #[must_use]
    pub fn running_circle(&self) -> char {
        const FRAMES: [char; 4] = ['◐', '◓', '◑', '◒'];
        // tick is ~80ms; /2 → ~160ms per frame (close to the ~120ms target,
        // and an integer divisor of the tick so the cadence stays steady).
        FRAMES[((self.tick as usize) / 2) % FRAMES.len()]
    }

    /// `true` when a phase is running but the base has gone quiet past the
    /// stall threshold — no worker output for >3s AND no tool call mid-flight.
    /// This is the HONEST "about to hang" signal: the UI paints the status red
    /// so the user sees a truthful cue instead of a fake-smooth spinner. Returns
    /// `false` whenever nothing is running, a tool call is in progress (a long
    /// `npm install` is work, not a stall), or output arrived within 3s.
    #[must_use]
    pub fn is_stalled(&self) -> bool {
        const STALL: std::time::Duration = std::time::Duration::from_secs(3);
        // Stall only makes sense while something is ACTIVELY running: a phase is
        // in flight, or a chat turn is "thinking". At a gate (paused for the
        // user) `phase_started_at` is cleared, so we never falsely go red there.
        let active = self.phase_started_at.is_some() || self.thinking;
        if !active || self.tool_in_progress {
            return false;
        }
        match self.last_output_at {
            Some(t) => t.elapsed() >= STALL,
            // Nothing has arrived yet this turn: only call it a stall once the
            // active block has been running > 3s (a just-started phase isn't
            // stalled, it's spinning up).
            None => self
                .phase_started_at
                .or(self.thinking_started)
                .is_some_and(|t| t.elapsed() >= STALL),
        }
    }

    /// Record a sign of life from the base — call on every worker stream event /
    /// host output line / progress note so [`Self::is_stalled`] resets.
    fn mark_output(&mut self) {
        self.last_output_at = Some(std::time::Instant::now());
    }

    /// Seconds since the current "thinking" turn began — for the live elapsed
    /// readout in the waiting indicator. `0` when not waiting.
    #[must_use]
    pub fn thinking_elapsed_secs(&self) -> u64 {
        self.thinking_started.map_or(0, |t| t.elapsed().as_secs())
    }
}

/// Parse a `## <heading>` section out of a markdown body and return the first
/// non-empty, non-italic-placeholder line under it. Returns `None` when the
/// section is absent or only contains placeholder text (`_(…)_`).
/// Minimal `which`: true when `program` is on PATH.
/// Mint a fresh RFC-4122 version-4-*formatted* UUID for a chat session id.
///
/// Not cryptographically random — it only needs to be unique per chat session
/// on one machine and a syntactically valid UUID (claude's `--session-id`
/// validates the format). Entropy mixes wall-clock nanoseconds, a per-process
/// atomic counter, and the pid, so two ids minted back-to-back in the same
/// process still differ. No external crate (UmaDev stays dependency-light).
fn new_chat_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos());
    let counter = u128::from(COUNTER.fetch_add(1, Ordering::Relaxed));
    let pid = u128::from(std::process::id());

    // Mix the three sources into 128 bits, then avalanche so low-entropy
    // inputs spread across all bytes (splitmix-style finaliser).
    let mut x = nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (pid << 64);
    x ^= x >> 47;
    x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    x ^= x >> 47;

    let mut u = x.to_be_bytes();
    u[6] = (u[6] & 0x0F) | 0x40; // version 4
    u[8] = (u[8] & 0x3F) | 0x80; // RFC-4122 variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]
    )
}

fn which_on_path(program: &str) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("sh")
            .args(["-c", &format!("command -v {program}")])
            .output()
            .is_ok_and(|o| o.status.success())
    }
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", &format!("where {program}")])
            .output()
            .is_ok_and(|o| o.status.success())
    }
}

fn parse_notes_section<'a>(body: &'a str, heading: &str) -> Option<&'a str> {
    let needle = format!("## {heading}");
    let after = body.split(&needle).nth(1)?;
    // Take lines until the next `## ` heading.
    for line in after.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("## ") {
            break;
        }
        // Skip italic placeholder lines like `_(example)_`.
        if trimmed.starts_with("_(") || trimmed.starts_with('_') {
            continue;
        }
        return Some(trimmed);
    }
    None
}

/// Best-effort cross-platform "open URL in default browser". Uses `open` on
/// macOS, `xdg-open` on Linux, `start` on Windows. Failures are silent — the
/// user can copy the URL manually.
fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let prog = "xdg-open";
    #[cfg(target_os = "windows")]
    let prog = "cmd";
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new(prog)
            .args(["/C", "start", "", url])
            .spawn()?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new(prog).arg(url).spawn()?;
        Ok(())
    }
}

/// Slash-verb entries for every registered host backend, derived from
/// `umadev_host::BACKEND_IDS` so the palette + did-you-mean can never
/// drift from the driver registry. Each entry is `(id, "switch worker to <display>")`.
///
/// Computed once and cached in a [`OnceLock`] (the backend registry is
/// immutable for the process lifetime), so callers get `&'static` refs
/// without per-keystroke allocation or leaks.
fn backend_palette_verbs() -> &'static [(&'static str, &'static str)] {
    static CACHE: std::sync::OnceLock<Vec<(&'static str, &'static str)>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        umadev_host::BACKEND_IDS
            .iter()
            .map(|id| {
                let display = umadev_host::driver_for(id)
                    .map_or_else(|| (*id).to_string(), |d| d.display_name().to_string());
                // Leak once at first use: the registry never changes, so the
                // table is process-lived. This is the standard pattern for
                // turning runtime-built data into &'static for const-shaped APIs.
                let hint: &'static str =
                    Box::leak(format!("switch worker to {display}").into_boxed_str());
                (*id, hint)
            })
            .collect()
    })
}

/// The fixed set of options the picker shows. Probe results refine the
/// labels at runtime.
fn step_items(
    step: PickerStep,
    _lang: umadev_i18n::Lang,
    backends: &[BackendInfo],
) -> Vec<PickerItem> {
    match step {
        // Step 1 - UI language.
        PickerStep::Language => umadev_i18n::Lang::ALL
            .iter()
            .map(|&l| PickerItem {
                backend_id: None,
                label: l.label().to_string(),
                ready: true,
                detail: l.code().to_string(),
                group: PickerGroup::Language,
                lang: Some(l),
            })
            .collect(),
        // Step 2 - which logged-in base CLI (ready-state from the live probe).
        PickerStep::BaseCli => umadev_host::BACKEND_IDS
            .iter()
            .map(|id| {
                let display = umadev_host::driver_for(id)
                    .map_or_else(|| (*id).to_string(), |d| d.display_name().to_string());
                let probe = backends.iter().find(|b| b.id == *id);
                PickerItem {
                    backend_id: Some((*id).to_string()),
                    label: display,
                    ready: probe.is_some_and(|p| p.ready),
                    detail: probe.map_or_else(|| "detecting...".to_string(), |p| p.detail.clone()),
                    group: PickerGroup::HostCli,
                    lang: None,
                }
            })
            .collect(),
    }
}

/// Tiny scalar extractors used by the `/verify` overlay so we don't need
/// a JSON dependency just to surface "score: 95 / passed: true" from the
/// quality-gate file. Returns `None` if the key isn't present or the
/// value isn't shaped like a JSON number / bool.
fn extract_json_number(json: &str, key: &str) -> Option<u32> {
    let needle = format!("\"{key}\"");
    let after = json.split(&needle).nth(1)?;
    let colon = after.find(':')?;
    let rest = &after[colon + 1..];
    let digits: String = rest
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse::<u32>().ok()
}

fn extract_json_bool(json: &str, key: &str) -> Option<bool> {
    let needle = format!("\"{key}\"");
    let after = json.split(&needle).nth(1)?;
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// Classic Levenshtein distance — used by the slash command typo
/// "did you mean" suggestion. Kept O(n·m) since `n` and `m` are
/// always under ~15 chars (verb names).
fn lev(a: &str, b: &str) -> usize {
    let a_bytes: Vec<char> = a.chars().collect();
    let b_bytes: Vec<char> = b.chars().collect();
    let n = a_bytes.len();
    let m = b_bytes.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = usize::from(a_bytes[i - 1] != b_bytes[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

/// Build the multi-line card shown in chat history when a UmaDev gate
/// pauses the pipeline. Lists exactly which artifacts are waiting for the
/// user's eyes and which slash commands move it forward — so the user
/// doesn't have to remember what `docs_confirm` vs `preview_confirm`
/// actually means.
fn gate_card(
    gate: Gate,
    slug: &str,
    project_root: &std::path::Path,
    lang: umadev_i18n::Lang,
) -> String {
    use umadev_i18n::{t, tf};
    let slug = if slug.is_empty() { "<slug>" } else { slug };
    let (title_key, artifacts, checklist_key, next_key) = match gate {
        Gate::ClarifyGate => (
            "gate.clarify.title",
            vec![format!("output/{slug}-clarify.md")],
            "gate.clarify.checklist",
            "gate.clarify.next",
        ),
        Gate::DocsConfirm => (
            "gate.docs.title",
            vec![
                format!("output/{slug}-prd.md"),
                format!("output/{slug}-architecture.md"),
                format!("output/{slug}-uiux.md"),
            ],
            "gate.docs.checklist",
            "gate.docs.next",
        ),
        Gate::PreviewConfirm => (
            "gate.preview.title",
            vec![
                format!("output/{slug}-frontend-notes.md"),
                format!("output/{slug}-execution-plan.md"),
            ],
            "gate.preview.checklist",
            "gate.preview.next",
        ),
    };

    let mut out = String::new();
    out.push_str(&format!("[gate] {}\n", t(lang, title_key)));
    out.push_str(&format!("  {}\n", t(lang, "gate.artifacts_header")));
    let mut warnings = Vec::new();
    for a in &artifacts {
        let path = project_root.join(a);
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let lines = content.lines().count();
        let is_scaffold =
            content.contains("Offline scaffold") || content.contains("offline scaffold");
        let detail = if lines == 0 {
            "[warn] MISSING".to_string()
        } else if is_scaffold {
            warnings.push(tf(lang, "gate.scaffold_warn", &[a.as_str()]));
            format!("{lines} lines [warn] SCAFFOLD")
        } else if lines < 30 {
            let ln = lines.to_string();
            warnings.push(tf(lang, "gate.short_warn", &[a.as_str(), ln.as_str()]));
            format!("{lines} lines [warn] SHORT")
        } else {
            format!("{lines} lines [ok]")
        };
        out.push_str(&format!("    - {a} ({detail})\n"));
    }
    if !warnings.is_empty() {
        out.push_str(&format!("  {}\n", t(lang, "gate.quality_warn_header")));
        for w in &warnings {
            out.push_str(&format!("    - {w}\n"));
        }
        out.push_str(&format!("    {}\n", t(lang, "gate.suggest_revise")));
    }
    // Quick quality indicators for docs_confirm
    if matches!(gate, Gate::DocsConfirm) {
        let uiux_path = project_root.join(format!("output/{slug}-uiux.md"));
        if let Ok(content) = std::fs::read_to_string(&uiux_path) {
            let tokens = content.matches("--").count().to_string();
            let has_dark = content
                .to_ascii_lowercase()
                .contains("prefers-color-scheme");
            let dark = if has_dark { "[ok]" } else { "[fail] missing" };
            out.push_str(&format!(
                "  {}\n",
                tf(lang, "gate.quality_line", &[tokens.as_str(), dark])
            ));
        }
    }
    out.push_str(&format!("  {}\n", t(lang, "gate.checklist_header")));
    for item in t(lang, checklist_key).split('\n') {
        out.push_str(&format!("    [ ] {item}\n"));
    }
    out.push_str(&format!("  {}\n", t(lang, "gate.actions_header")));
    out.push_str(&format!("    - {}\n", t(lang, "gate.action.continue")));
    out.push_str(&format!("    - {}\n", t(lang, "gate.action.revise")));
    out.push_str(&format!("    - {}\n", t(lang, "gate.action.diff_prd")));
    out.push_str(&format!("    - {}\n", t(lang, "gate.action.diff_arch")));
    out.push_str(&format!("    - {}\n", t(lang, "gate.action.diff_uiux")));
    out.push_str(&format!(
        "  {}",
        tf(lang, "gate.guide_prefix", &[t(lang, next_key)])
    ));
    out
}

fn refresh_picker_with_probes(items: &mut [PickerItem], probes: &[BackendInfo]) {
    for item in items.iter_mut() {
        if let Some(id) = item.backend_id.as_deref() {
            if let Some(p) = probes.iter().find(|p| p.id == id) {
                item.ready = p.ready;
                item.detail = p.detail.clone();
            }
        }
    }
}

/// Format a duration in seconds as a compact `m:ss` (or `s` when under a
/// minute) human counter for the status bar.
fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

fn walkdir_count_md_inner(d: &std::path::Path, c: &mut usize, depth: usize) {
    if depth > 6 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(d) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walkdir_count_md_inner(&p, c, depth + 1);
        } else if p.extension().and_then(|s| s.to_str()) == Some("md") {
            *c += 1;
        }
    }
}

fn walkdir_count_md(dir: &std::path::Path) -> usize {
    let mut count = 0;
    walkdir_count_md_inner(dir, &mut count, 0);
    count
}

/// Truncate a string to `max` chars with an ellipsis (char-safe).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// Render a [`umadev_agent::runner::UsageReport`] as an i18n plain-text block
/// for the `/usage` chat reply. Mirrors the `umadev usage` CLI layout so both
/// surfaces read identically. Empty report → the friendly empty state.
fn format_usage_report(
    lang: umadev_i18n::Lang,
    report: &umadev_agent::runner::UsageReport,
) -> String {
    if report.is_empty() {
        return umadev_i18n::t(lang, "usage.empty").to_string();
    }
    let mut out = umadev_i18n::tf(
        lang,
        "usage.title",
        &[
            &report.total_calls.to_string(),
            &report.runs.len().to_string(),
        ],
    );
    out.push('\n');
    for run in &report.runs {
        let backends = if run.backends.is_empty() {
            "offline".to_string()
        } else {
            run.backends.join(", ")
        };
        out.push('\n');
        out.push_str(&umadev_i18n::tf(
            lang,
            "usage.run_header",
            &[&run.index.to_string(), &backends],
        ));
        out.push('\n');
        for p in &run.phases {
            out.push_str(&umadev_i18n::tf(
                lang,
                "usage.phase_line",
                &[&p.phase, &p.calls.to_string(), &p.tokens.to_string()],
            ));
            out.push('\n');
        }
        out.push_str(&umadev_i18n::tf(
            lang,
            "usage.run_total",
            &[&run.calls.to_string(), &run.tokens.to_string()],
        ));
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&umadev_i18n::tf(
        lang,
        "usage.grand_total",
        &[&report.total_tokens.to_string()],
    ));
    out.push('\n');
    let cost = format!(
        "{:.2}",
        umadev_agent::runner::rough_cost_usd(report.total_tokens)
    );
    out.push_str(&umadev_i18n::tf(lang, "usage.cost_estimate", &[&cost]));
    out.push('\n');
    out.push_str(umadev_i18n::t(lang, "usage.note_combined"));
    out
}

/// Map a pitfall status to its (icon, i18n status-label key) for the lessons view.
fn lesson_status_chrome(status: umadev_agent::PitfallStatus) -> (&'static str, &'static str) {
    use umadev_agent::PitfallStatus;
    match status {
        PitfallStatus::Validated => ("[ok]", "lessons.status.validated"),
        PitfallStatus::Recurring => ("[warn]", "lessons.status.recurring"),
        PitfallStatus::Active => ("[pitfall]", "lessons.status.active"),
    }
}

/// Render a [`umadev_agent::LessonsReport`] as an i18n plain-text block for the
/// `/lessons` overlay. Mirrors the `umadev lessons` CLI layout. Empty report →
/// the friendly empty state.
fn format_lessons_report(lang: umadev_i18n::Lang, report: &umadev_agent::LessonsReport) -> String {
    if report.is_empty() {
        return umadev_i18n::t(lang, "lessons.empty").to_string();
    }
    let e = report.efficacy;
    let mut out = umadev_i18n::tf(
        lang,
        "lessons.efficacy",
        &[
            &e.total.to_string(),
            &e.validated.to_string(),
            &e.recurring.to_string(),
            &e.active.to_string(),
        ],
    );
    out.push_str("\n\n");

    if !report.top_pitfalls.is_empty() {
        out.push_str(umadev_i18n::t(lang, "lessons.top_header"));
        out.push('\n');
        for p in &report.top_pitfalls {
            let (icon, status_key) = lesson_status_chrome(p.status);
            let status = umadev_i18n::t(lang, status_key);
            out.push_str(&umadev_i18n::tf(
                lang,
                "lessons.pitfall_line",
                &[icon, &p.title, &p.hits.to_string(), status],
            ));
            out.push('\n');
            if !p.fix.is_empty() {
                out.push_str(&umadev_i18n::tf(
                    lang,
                    "lessons.pitfall_fix",
                    &[&truncate_chars(&p.fix, 200)],
                ));
                out.push('\n');
            }
            if !p.context.is_empty() {
                out.push_str(&umadev_i18n::tf(
                    lang,
                    "lessons.pitfall_ctx",
                    &[&p.context.join(", ")],
                ));
                out.push('\n');
            }
        }
        out.push('\n');
    }

    // Failed fixes UmaDev is now steering away from (deduped across pitfalls).
    let mut avoid: Vec<String> = Vec::new();
    for p in &report.recurring {
        for f in &p.failed_fixes {
            let f = truncate_chars(f, 160);
            if !avoid.contains(&f) {
                avoid.push(f);
            }
        }
    }
    if !avoid.is_empty() {
        out.push_str(umadev_i18n::t(lang, "lessons.recurring_header"));
        out.push('\n');
        for f in &avoid {
            out.push_str(&umadev_i18n::tf(lang, "lessons.avoid_line", &[f]));
            out.push('\n');
        }
        out.push('\n');
    }

    if !report.validated_patterns.is_empty() {
        out.push_str(umadev_i18n::t(lang, "lessons.validated_header"));
        out.push('\n');
        for v in &report.validated_patterns {
            out.push_str(&umadev_i18n::tf(
                lang,
                "lessons.validated_line",
                &[&v.title, &v.summary],
            ));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserConfig;

    fn fresh_app(backend: Option<&str>) -> App {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let cfg = UserConfig {
            backend: backend.map(str::to_string),
            model: None,
            // Pin zh-CN so language-sensitive UI assertions (gate cards etc.)
            // are deterministic regardless of the test host's locale.
            lang: Some("zh-CN".to_string()),
            ..Default::default()
        };
        // Each test gets a unique workspace dir to avoid file races between
        // parallel tests. The .umadevrc disables auto_approve_gates so
        // gate-card tests see the manual-approval path.
        let workspace = std::path::PathBuf::from(format!("/tmp/sd-test-ws-{id}"));
        let _ = std::fs::create_dir_all(&workspace);
        let _ = std::fs::write(
            workspace.join(".umadevrc"),
            "[pipeline]\nauto_approve_gates = false\n",
        );
        App::new(
            "demo",
            cfg,
            std::path::PathBuf::from(format!("/tmp/sd-test-cfg-{id}.toml")),
            workspace,
        )
    }

    #[test]
    fn shift_up_scrolls_transcript_and_stops_auto_stick() {
        let mut app = fresh_app(Some("offline"));
        // Simulate a render having published a scroll bound + viewport.
        app.transcript_max_scroll.set(20);
        app.transcript_viewport_rows.set(10);
        // Shift+↑ nudges the transcript up one row (un-pins from the bottom).
        let _ = app.apply_key_with_mods(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::SHIFT,
        );
        assert_eq!(app.transcript_scroll, 1);
        // Shift+↓ brings it back.
        let _ = app.apply_key_with_mods(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::SHIFT,
        );
        assert_eq!(app.transcript_scroll, 0);
    }

    #[test]
    fn page_and_home_end_scroll_against_published_viewport() {
        let mut app = fresh_app(Some("offline"));
        app.transcript_max_scroll.set(100);
        app.transcript_viewport_rows.set(20);
        // PageUp = viewport - 1 rows.
        let _ = app.apply_key(crossterm::event::KeyCode::PageUp);
        assert_eq!(app.transcript_scroll, 19);
        // Home jumps to the very top (= max scroll).
        let _ = app.apply_key(crossterm::event::KeyCode::Home);
        assert_eq!(app.transcript_scroll, 100);
        // End re-pins to the bottom.
        let _ = app.apply_key(crossterm::event::KeyCode::End);
        assert_eq!(app.transcript_scroll, 0);
        // Ctrl+Alt+U = half a page up (the half-page scroll moved off bare
        // Ctrl-U so the shell "clear line" key keeps its job).
        let _ = app.apply_key_with_mods(
            crossterm::event::KeyCode::Char('u'),
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
        );
        assert_eq!(app.transcript_scroll, 10);
    }

    #[test]
    fn ctrl_alt_u_and_d_half_page_scroll_transcript() {
        let mut app = fresh_app(Some("offline"));
        app.transcript_max_scroll.set(100);
        app.transcript_viewport_rows.set(20);
        let cmd_alt = crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT;
        // Ctrl+Alt+U → half a viewport up (20 / 2 = 10).
        let _ = app.apply_key_with_mods(crossterm::event::KeyCode::Char('u'), cmd_alt);
        assert_eq!(
            app.transcript_scroll, 10,
            "Ctrl+Alt+U scrolls half a page up"
        );
        // Ctrl+Alt+D → half a viewport back down.
        let _ = app.apply_key_with_mods(crossterm::event::KeyCode::Char('d'), cmd_alt);
        assert_eq!(
            app.transcript_scroll, 0,
            "Ctrl+Alt+D scrolls half a page down"
        );
        // Ctrl+Alt+B / Ctrl+Alt+F are paging aliases.
        let _ = app.apply_key_with_mods(crossterm::event::KeyCode::Char('b'), cmd_alt);
        assert_eq!(app.transcript_scroll, 10, "Ctrl+Alt+B aliases scroll-up");
        let _ = app.apply_key_with_mods(crossterm::event::KeyCode::Char('f'), cmd_alt);
        assert_eq!(app.transcript_scroll, 0, "Ctrl+Alt+F aliases scroll-down");
    }

    #[test]
    fn bare_ctrl_u_and_ctrl_d_no_longer_scroll_transcript() {
        let mut app = fresh_app(Some("offline"));
        app.transcript_max_scroll.set(100);
        app.transcript_viewport_rows.set(20);
        // Empty input + bare Ctrl-U: must NOT scroll (it's the line-clear key,
        // and the input is empty so there is nothing to delete either).
        let _ = app.apply_key_with_mods(
            crossterm::event::KeyCode::Char('u'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert_eq!(
            app.transcript_scroll, 0,
            "bare Ctrl-U must not move the transcript"
        );
        // Scroll up first, then bare Ctrl-D: it must NOT scroll back (Ctrl-D is
        // the terminal EOF/quit convention, not a scroll key). On empty input
        // it routes to quit, so assert via should_quit and a still-scrolled view.
        app.transcript_scroll = 30;
        let _ = app.apply_key_with_mods(
            crossterm::event::KeyCode::Char('d'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert_eq!(
            app.transcript_scroll, 30,
            "bare Ctrl-D must not move the transcript"
        );
        assert!(app.should_quit, "bare Ctrl-D on empty input quits (EOF)");
    }

    #[test]
    fn slash_mouse_emits_set_capture_action_and_uses_i18n() {
        let mut app = fresh_app(Some("offline"));
        assert!(app.mouse_scroll, "wheel scroll defaults on");
        // Turning OFF must emit SetMouseCapture(false) so the event loop issues
        // the real DisableMouseCapture, not just flip a bool.
        let action = app.slash_toggle_mouse();
        assert_eq!(action, Action::SetMouseCapture(false));
        assert!(!app.mouse_scroll);
        // Toggling back ON emits SetMouseCapture(true).
        let action = app.slash_toggle_mouse();
        assert_eq!(action, Action::SetMouseCapture(true));
        assert!(app.mouse_scroll);
        // The pushed status line must be the i18n string, not a raw literal.
        let last = app.history.back().expect("a status line was pushed");
        assert_eq!(
            last.body,
            umadev_i18n::t(app.lang, "slash.mouse_on"),
            "/mouse status text must come from the i18n catalog"
        );
    }

    #[test]
    fn submitting_a_turn_repins_transcript_to_bottom() {
        let mut app = fresh_app(Some("offline"));
        app.transcript_max_scroll.set(50);
        app.transcript_scroll = 30; // user is reviewing history
        for c in "hello".chars() {
            let _ = app.apply_key(crossterm::event::KeyCode::Char(c));
        }
        let _ = app.apply_key(crossterm::event::KeyCode::Enter);
        assert_eq!(
            app.transcript_scroll, 0,
            "submitting must snap back to the newest content"
        );
    }

    #[test]
    fn slash_mouse_toggles_wheel_scroll_flag() {
        let mut app = fresh_app(Some("offline"));
        assert!(app.mouse_scroll, "wheel scroll defaults on");
        for c in "/mouse".chars() {
            let _ = app.apply_key(crossterm::event::KeyCode::Char(c));
        }
        let _ = app.apply_key(crossterm::event::KeyCode::Enter);
        assert!(!app.mouse_scroll, "/mouse turns the wheel binding off");
    }

    #[test]
    fn conversation_memory_threads_turns_and_bounds_length() {
        let mut app = fresh_app(Some("claude-code"));

        app.record_user_turn("你好");
        app.record_chat_reply("你好,我是底座".to_string());
        app.record_user_turn("我刚才说了什么?");

        // The snapshot handed to the base is the running dialogue, in order,
        // with correct roles — this is what makes chat a real conversation.
        let snap = app.conversation_snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].role, "user");
        assert_eq!(snap[0].content, "你好");
        assert_eq!(snap[1].role, "assistant");
        assert_eq!(snap[2].content, "我刚才说了什么?");

        // The base's reply is also rendered in the visible chat as a Host line.
        assert!(app
            .history
            .iter()
            .any(|m| m.role == ChatRole::Host && m.body == "你好,我是底座"));

        // Memory stays bounded to the most recent CONVERSATION_CAP messages.
        for i in 0..CONVERSATION_CAP * 2 {
            app.record_user_turn(&format!("msg {i}"));
        }
        assert!(app.conversation.len() <= CONVERSATION_CAP);
        assert_eq!(
            app.conversation.last().unwrap().content,
            format!("msg {}", CONVERSATION_CAP * 2 - 1)
        );
    }

    #[test]
    fn chat_session_id_is_stable_then_resets() {
        let mut app = fresh_app(Some("claude-code"));
        assert!(app.chat_session_id.is_none(), "no id until first host turn");

        let id1 = app.ensure_chat_session_id();
        let id2 = app.ensure_chat_session_id();
        assert_eq!(id1, id2, "the id is minted once and stays stable");

        // Looks like a v4 UUID: 8-4-4-4-12 hex, version nibble = 4.
        let groups: Vec<&str> = id1.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(id1.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert!(groups[2].starts_with('4'), "version nibble must be 4");

        // /clear drops the id so the next turn starts a brand-new session.
        let _ = app.try_slash_command("/clear");
        assert!(app.chat_session_id.is_none());
        assert_ne!(
            app.ensure_chat_session_id(),
            id1,
            "a fresh session gets a fresh id"
        );
    }

    #[test]
    fn new_chat_session_ids_are_unique() {
        let a = new_chat_session_id();
        let b = new_chat_session_id();
        assert_ne!(a, b, "back-to-back ids must differ");
    }

    #[test]
    fn parse_notes_section_extracts_url() {
        let body = "# Frontend notes\n\n## Preview URL\n\nhttp://localhost:5173\n\n## Run command\n\ncd web && npm run dev\n";
        assert_eq!(
            parse_notes_section(body, "Preview URL"),
            Some("http://localhost:5173")
        );
        assert_eq!(
            parse_notes_section(body, "Run command"),
            Some("cd web && npm run dev")
        );
    }

    #[test]
    fn parse_notes_section_skips_placeholder() {
        let body = "## Preview URL\n\n_(worker fills this)_\n\nhttp://localhost:3000\n";
        // Skips the italic placeholder, returns the real URL.
        assert_eq!(
            parse_notes_section(body, "Preview URL"),
            Some("http://localhost:3000")
        );
    }

    #[test]
    fn parse_notes_section_missing_returns_none() {
        assert_eq!(parse_notes_section("no headings here", "Preview URL"), None);
    }

    #[test]
    fn parse_notes_section_stops_at_next_heading() {
        let body = "## Preview URL\n\nhttp://localhost:5173\n\n## Other\n\nhttp://wrong\n";
        assert_eq!(
            parse_notes_section(body, "Preview URL"),
            Some("http://localhost:5173")
        );
    }

    #[test]
    fn preview_url_from_notes_reads_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let slug = "demo";
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path()
                .join("output")
                .join(format!("{slug}-frontend-notes.md")),
            "# Notes\n\n## Preview URL\n\nhttp://localhost:4321\n\n## Run command\n\nnpm run dev\n",
        )
        .unwrap();
        let app = App::new(
            slug.to_string(),
            UserConfig {
                backend: Some("offline".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        assert_eq!(
            app.preview_url_from_notes().as_deref(),
            Some("http://localhost:4321")
        );
        assert_eq!(app.run_command_from_notes().as_deref(), Some("npm run dev"));
    }

    #[test]
    fn slash_preview_with_no_notes_gives_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "demo".to_string(),
            UserConfig {
                backend: Some("offline".into()),
                lang: Some("zh-CN".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        // No output dir / notes file → guidance message, no StartPreview.
        let action = app.slash_preview();
        assert!(matches!(action, Action::None));
        assert!(app.history.iter().any(|m| m.body.contains("还没有可预览")));
    }

    #[test]
    fn slash_preview_with_url_and_command_emits_start() {
        let tmp = tempfile::TempDir::new().unwrap();
        let slug = "demo";
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path()
                .join("output")
                .join(format!("{slug}-frontend-notes.md")),
            "## Preview URL\n\nhttp://localhost:5173\n\n## Run command\n\ncd web && npm run dev\n",
        )
        .unwrap();
        let mut app = App::new(
            slug.to_string(),
            UserConfig {
                backend: Some("offline".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        let action = app.slash_preview();
        match action {
            Action::StartPreview { url, command } => {
                assert_eq!(url, "http://localhost:5173");
                assert_eq!(command, "cd web && npm run dev");
            }
            other => panic!("expected StartPreview, got {other:?}"),
        }
    }

    #[test]
    fn no_backend_opens_picker() {
        let app = fresh_app(None);
        assert_eq!(app.mode, AppMode::Picker);
    }

    #[test]
    fn configured_backend_opens_chat_with_greeting() {
        let app = fresh_app(Some("claude-code"));
        assert_eq!(app.mode, AppMode::Chat);
        assert_eq!(app.backend_label, "claude-code");
        // Greeting is the very first message.
        let first = app.history.front().unwrap();
        assert_eq!(first.role, ChatRole::UmaDev);
        assert!(first.body.contains("claude-code"));
    }

    #[test]
    fn picker_arrow_keys_navigate() {
        let mut app = fresh_app(None);
        let last = app.picker_items.len() - 1;
        assert_eq!(app.picker_selected, 0);
        // Walk all the way down — should clamp at `last`.
        for _ in 0..(app.picker_items.len() + 2) {
            let _ = app.apply_key(KeyCode::Down);
        }
        assert_eq!(app.picker_selected, last);
        let _ = app.apply_key(KeyCode::Up);
        assert_eq!(app.picker_selected, last - 1);
    }

    #[test]
    fn picker_enter_on_unavailable_host_stays() {
        let mut app = fresh_app(None);
        // Base CLIs live in step 3; with no probes yet they're all unready.
        app.goto_picker_step(PickerStep::BaseCli);
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::None);
        assert_eq!(app.mode, AppMode::Picker);
        // The refusal is surfaced INLINE on the picker (visible to the user),
        // not pushed to the not-yet-visible chat screen.
        assert!(app.picker_notice.is_some());
        // …and navigating away clears it.
        let _ = app.apply_key(KeyCode::Down);
        assert!(app.picker_notice.is_none());
    }

    #[test]
    fn picker_refreshes_on_backend_probed() {
        let mut app = fresh_app(None);
        app.apply_engine(EngineEvent::BackendProbed {
            backend_id: "claude-code".into(),
            ready: true,
            detail: "claude 1.6.0".into(),
        });
        // Walk to the base-CLI step (language -> mode -> base) where the host
        // rows live; the probe just cached marks claude-code ready.
        app.goto_picker_step(PickerStep::BaseCli);
        let idx = app
            .picker_items
            .iter()
            .position(|i| i.backend_id.as_deref() == Some("claude-code"))
            .unwrap();
        app.picker_selected = idx;
        assert!(app.picker_items[idx].ready);
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::BackendChanged);
        assert_eq!(app.mode, AppMode::Chat);
        assert_eq!(app.backend_label, "claude-code");
    }

    #[test]
    fn chat_plain_text_routes_to_worker() {
        let mut app = fresh_app(Some("offline"));
        for c in "build a login".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Route("build a login".to_string()));
        // Input is cleared after submit.
        assert!(app.input.is_empty());
    }

    #[test]
    fn chat_empty_enter_is_noop() {
        let mut app = fresh_app(Some("offline"));
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::None);
    }

    #[test]
    fn slash_help_toggles_help_overlay() {
        let mut app = fresh_app(Some("offline"));
        for c in "/help".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::None);
        assert!(app.show_help);
    }

    #[test]
    fn slash_quit_returns_quit() {
        let mut app = fresh_app(Some("offline"));
        for c in "/quit".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn slash_clear_clears_history() {
        let mut app = fresh_app(Some("offline"));
        assert!(!app.history.is_empty()); // greeting present
        for c in "/clear".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let _ = app.apply_key(KeyCode::Enter);
        // After /clear: only the localized "history cleared" system note remains.
        assert_eq!(app.history.len(), 1);
        assert_eq!(
            app.history.front().unwrap().body,
            umadev_i18n::t(app.lang, "slash.history_cleared")
        );
    }

    #[test]
    fn slash_claude_switches_backend_and_saves() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        let cfg = UserConfig {
            backend: Some("offline".to_string()),
            model: None,
            ..Default::default()
        };
        let mut app = App::new(
            "demo",
            cfg,
            cfg_path.clone(),
            std::path::PathBuf::from("/tmp/sd-test-workspace"),
        );
        for c in "/claude".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::BackendChanged);
        assert_eq!(app.backend_label, "claude-code");
        // Config is persisted.
        let loaded = crate::config::load_from(&cfg_path);
        assert_eq!(loaded.backend.as_deref(), Some("claude-code"));
    }

    #[test]
    fn slash_continue_with_open_gate_returns_continue() {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        for c in "/continue".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Continue(Gate::DocsConfirm));
    }

    #[test]
    fn slash_continue_without_gate_is_noop_with_hint() {
        let mut app = fresh_app(Some("offline"));
        for c in "/continue".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::None);
        assert!(app
            .history
            .iter()
            .any(|m| m.body.contains("还没启动流水线") || m.body.contains("没有打开的 gate")));
    }

    #[test]
    fn slash_revise_at_gate_returns_revise_with_text() {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        for c in "/revise 把 OAuth 删掉".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Revise("把 OAuth 删掉".to_string()));
    }

    #[test]
    fn slash_revise_without_args_is_noop_with_usage_hint() {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        for c in "/revise".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::None);
        assert!(app.history.iter().any(|m| m.body.contains("/revise")));
    }

    #[test]
    fn slash_unknown_command_hints() {
        let mut app = fresh_app(Some("offline"));
        for c in "/foo".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let _ = app.apply_key(KeyCode::Enter);
        assert!(app
            .history
            .iter()
            .any(|m| m.body.contains("未知命令") && m.body.contains("/foo")));
    }

    #[test]
    fn plain_text_at_open_gate_routes_to_revise() {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        for c in "去掉 OAuth".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Revise("去掉 OAuth".to_string()));
    }

    #[test]
    fn plain_text_after_delivery_routes_to_worker() {
        let mut app = fresh_app(Some("offline"));
        app.run_started = true;
        app.apply_engine(EngineEvent::BlockCompleted {
            final_phase: Phase::Delivery,
            paused_at: None,
        });
        assert!(app.finished);
        for c in "make another tool".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Route("make another tool".to_string()));
        // Routing alone does not reset the delivered run; reset happens after
        // the worker returns a `run` decision.
        assert!(app.finished);
    }

    #[test]
    fn worker_routed_run_after_delivery_resets_phases() {
        let mut app = fresh_app(Some("offline"));
        app.run_started = true;
        app.apply_engine(EngineEvent::BlockCompleted {
            final_phase: Phase::Delivery,
            paused_at: None,
        });
        assert!(app.finished);
        app.prepare_worker_routed_run("make another tool");

        assert!(app.phases.iter().all(|r| r.status == PhaseStatus::Pending));
        assert!(!app.finished);
    }

    #[test]
    fn abort_sentinel_note_surfaces_explicit_terminal_state_not_idle() {
        // THE VISIBILITY BUG: a block that ended with an error used to emit a
        // bare, easily-missed note and leave the bar reading "ready / 0/9". A
        // terminal-abort note (carrying `ABORT_SENTINEL`) must instead flip the
        // run into an explicit aborted state and stop the live counters.
        let mut app = fresh_app(Some("offline"));
        app.run_started = true;
        app.run_started_at = Some(std::time::Instant::now());
        assert!(app.is_pipeline_active(), "run is active before the abort");

        app.apply_engine(EngineEvent::Note(format!(
            "{}本轮已中止:磁盘写入失败 — 释放空间后重试",
            crate::ABORT_SENTINEL
        )));

        assert!(app.aborted, "the sentinel note flips the run into aborted");
        assert!(
            !app.is_pipeline_active(),
            "an aborted run is NOT active — a retry must not be refused as busy"
        );
        assert!(
            app.run_started_at.is_none() && app.phase_started_at.is_none(),
            "live elapsed counters stop on abort so the bar isn't a fake idle"
        );
        // The user sees the cause, and the sentinel marker is stripped.
        let last = app.history.back().unwrap();
        assert!(last.body.contains("本轮已中止"));
        assert!(
            !last.body.contains(crate::ABORT_SENTINEL),
            "the internal sentinel marker must never be shown to the user"
        );
        // The status bar carries the explicit aborted label, not an idle look.
        app.refresh_status();
        assert!(app.status.contains("aborted"));
    }

    #[test]
    fn a_new_pipeline_start_clears_a_prior_aborted_state() {
        // Retrying after an abort: `PipelineStarted` must clear `aborted` so the
        // fresh run reads as live, not stuck in the previous terminal state.
        let mut app = fresh_app(Some("offline"));
        app.aborted = true;
        app.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "retry it".into(),
        });
        assert!(!app.aborted, "a fresh block clears the prior aborted state");
        assert!(app.is_pipeline_active(), "the retried run is active again");
    }

    #[test]
    fn ordinary_progress_note_does_not_abort() {
        // A normal progress note (no sentinel) must keep the run active — only
        // the explicit terminal-abort marker flips it.
        let mut app = fresh_app(Some("offline"));
        app.run_started = true;
        app.apply_engine(EngineEvent::Note("[plan] 动态规划:greenfield".into()));
        assert!(!app.aborted, "a plain progress note never aborts");
        assert!(app.is_pipeline_active());
    }

    #[test]
    fn host_output_lands_in_history_as_host_role() {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::HostOutput {
            phase: Phase::Research,
            line: "## Similar products".into(),
        });
        let last = app.history.back().unwrap();
        assert_eq!(last.role, ChatRole::Host);
        assert!(last.body.contains("Similar products"));
    }

    #[test]
    fn history_is_bounded() {
        let mut app = fresh_app(Some("offline"));
        for i in 0..(HISTORY_CAP + 50) {
            app.apply_engine(EngineEvent::Note(format!("line {i}")));
        }
        assert!(app.history.len() <= HISTORY_CAP);
    }

    #[test]
    fn f1_toggles_help_in_both_modes() {
        let mut a = fresh_app(None);
        assert!(!a.show_help);
        let _ = a.apply_key(KeyCode::F(1));
        assert!(a.show_help);
        let mut b = fresh_app(Some("offline"));
        let _ = b.apply_key(KeyCode::F(1));
        assert!(b.show_help);
    }

    #[test]
    fn slash_spec_opens_overlay() {
        let mut a = fresh_app(Some("offline"));
        for c in "/spec".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        let ov = a.overlay.as_ref().expect("overlay should open");
        assert!(ov.title.contains("UMADEV_HOST_SPEC_V1"));
        assert!(ov.lines.iter().any(|l| l.contains("UMADEV_HOST_SPEC_V1")));
    }

    #[test]
    fn slash_doctor_opens_overlay_with_binary_line() {
        let mut a = fresh_app(Some("offline"));
        for c in "/doctor".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        let ov = a.overlay.as_ref().expect("doctor overlay");
        // Locale-independent: the binary line carries the crate version, and the
        // worker-availability section header is always present. (The labels
        // themselves are localized, so we assert on the language-neutral parts.)
        assert!(
            ov.lines
                .iter()
                .any(|l| l.contains(env!("CARGO_PKG_VERSION"))),
            "doctor overlay should show the binary version line"
        );
        let avail = umadev_i18n::t(a.lang, "doctor.worker_availability");
        assert!(
            ov.lines.iter().any(|l| l.contains(avail.trim())),
            "doctor overlay should show the worker-availability section"
        );
    }

    #[test]
    fn slash_verify_opens_overlay_with_sections() {
        let mut a = fresh_app(Some("offline"));
        for c in "/verify".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        let ov = a.overlay.as_ref().unwrap();
        let joined = ov.lines.join("\n");
        assert!(joined.contains("## Spec manifest"));
        assert!(joined.contains("## Workflow state"));
        assert!(joined.contains("## Artifacts"));
    }

    #[test]
    fn slash_diff_missing_artifact_shows_available_list() {
        let mut a = fresh_app(Some("offline"));
        for c in "/diff".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        let ov = a.overlay.as_ref().unwrap();
        // Empty workspace → fallback message kicks in.
        assert!(ov
            .lines
            .iter()
            .any(|l| l.contains("找不到") || l.contains("还不存在")));
    }

    #[test]
    fn slash_init_writes_umadev_yaml() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = UserConfig {
            backend: Some("offline".into()),
            model: None,
            ..Default::default()
        };
        let mut app = App::new(
            "demo",
            cfg,
            std::path::PathBuf::from("/tmp/sd-test-config.toml"),
            tmp.path().to_path_buf(),
        );
        for c in "/init".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::None);
        // Manifest must exist on disk after the slash command.
        assert!(tmp.path().join("umadev.yaml").is_file());
        // Confirmation message landed in the chat.
        assert!(app
            .history
            .iter()
            .any(|m| m.role == ChatRole::UmaDev && m.body.contains("umadev.yaml")));
    }

    #[test]
    fn esc_during_active_pipeline_interrupts_the_run() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "build".into(),
        });
        assert!(a.is_pipeline_active());
        // Esc INTERRUPTS the running pipeline (like Claude Code) — a single
        // press, and it does NOT quit the app.
        let action = a.apply_key(KeyCode::Esc);
        assert_eq!(action, Action::Cancel);
        assert!(!a.should_quit);
    }

    #[test]
    fn typing_clears_pending_quit_confirm() {
        let mut a = fresh_app(Some("offline"));
        // Idle Esc arms the quit confirmation (no pipeline running).
        let _ = a.apply_key(KeyCode::Esc);
        assert!(a.pending_quit_confirm);
        // Any typing — even one char — clears the pending confirmation.
        let _ = a.apply_key(KeyCode::Char('x'));
        assert!(!a.pending_quit_confirm);
    }

    #[test]
    fn typing_mid_phase_queues_and_fires_at_the_next_gate() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "build".into(),
        });
        assert!(a.is_pipeline_active() && a.active_gate.is_none());
        // Typing while a phase runs (no gate open) QUEUES the message instead of
        // dropping it.
        for c in "make it dark mode".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::None);
        assert_eq!(
            a.queued_steer
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["make it dark mode"]
        );
        // At the next gate (the gap), the queued message is promoted to a
        // pending steer — fired as a revision — instead of auto-approving.
        a.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        assert!(a.queued_steer.is_empty());
        assert_eq!(a.pending_steer.as_deref(), Some("make it dark mode"));
        assert!(a.pending_auto_continue.is_none());
    }

    #[test]
    fn multiple_mid_phase_steers_queue_without_loss_and_count_correctly() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "build".into(),
        });
        assert!(a.is_pipeline_active() && a.active_gate.is_none());
        // Three separate mid-phase turns. The old `Option<String>` overwrote all
        // but the last; a `VecDeque` keeps every one, in order.
        for turn in ["first steer", "second steer", "third steer"] {
            for c in turn.chars() {
                let _ = a.apply_key(KeyCode::Char(c));
            }
            let action = a.apply_key(KeyCode::Enter);
            assert_eq!(action, Action::None);
        }
        assert_eq!(
            a.queued_steer
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["first steer", "second steer", "third steer"],
            "every steer is retained in FIFO order — none overwritten"
        );
        // The `queued N` chip reflects all three, not a stuck 1.
        assert_eq!(a.queued_count(), 3, "count is the real queue depth");
        // At the next gate, ALL of them fold into one pending revision (in order).
        a.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        assert!(a.queued_steer.is_empty(), "queue drained at the gate");
        assert_eq!(
            a.pending_steer.as_deref(),
            Some("first steer\nsecond steer\nthird steer")
        );
        assert_eq!(a.queued_count(), 0);
    }

    #[test]
    fn esc_during_agentic_turn_interrupts_not_quits() {
        let mut a = fresh_app(Some("offline"));
        // An agentic chat turn is streaming in a base subprocess — note this is
        // NOT a pipeline run (`run_started` stays false), so the only thing that
        // can interrupt it is the `agentic_in_flight` branch.
        a.agentic_in_flight = true;
        assert!(!a.is_pipeline_active());
        // Esc INTERRUPTS the agentic subprocess (parity with Ctrl-C) and does NOT
        // arm quit-confirm or drop the app.
        let action = a.apply_key(KeyCode::Esc);
        assert_eq!(action, Action::Cancel);
        assert!(!a.should_quit);
        assert!(
            !a.pending_quit_confirm,
            "Esc on an agentic turn interrupts, it does not arm quit-confirm"
        );
    }

    // ---- resume hint on chat init ----

    #[test]
    fn resume_hint_appears_when_workflow_state_paused_at_gate() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Seed a workflow-state.json that looks like "paused at docs_confirm".
        let state_dir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state_json = r#"{
            "phase": "docs_confirm",
            "active_gate": "docs_confirm",
            "slug": "demo",
            "requirement": "做一个登录系统",
            "last_transition_at": "2026-05-23T10:00:00Z",
            "note": "",
            "spec_version": "UMADEV_HOST_SPEC_V1"
        }"#;
        std::fs::write(state_dir.join("workflow-state.json"), state_json).unwrap();

        let cfg = UserConfig {
            backend: Some("offline".into()),
            model: None,
            ..Default::default()
        };
        let app = App::new(
            "demo",
            cfg,
            std::path::PathBuf::from("/tmp/sd-test-config.toml"),
            tmp.path().to_path_buf(),
        );

        // Greeting + resume hint both land in history.
        let resume_msg = app
            .history
            .iter()
            .find(|m| m.body.contains("docs_confirm"))
            .expect("resume hint should mention the paused gate");
        assert_eq!(resume_msg.role, ChatRole::System);
        assert!(resume_msg.body.contains("做一个登录系统"));
        assert!(resume_msg.body.contains("/continue"));
    }

    #[test]
    fn resume_hint_marks_completed_runs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state_json = r#"{
            "phase": "delivery",
            "active_gate": "",
            "slug": "demo",
            "requirement": "做个 todo",
            "last_transition_at": "2026-05-23T10:00:00Z",
            "note": "Pipeline complete.",
            "spec_version": "UMADEV_HOST_SPEC_V1"
        }"#;
        std::fs::write(state_dir.join("workflow-state.json"), state_json).unwrap();

        let cfg = UserConfig {
            backend: Some("offline".into()),
            model: None,
            lang: Some("zh-CN".into()),
            ..Default::default()
        };
        let app = App::new(
            "demo",
            cfg,
            std::path::PathBuf::from("/tmp/sd-test-config.toml"),
            tmp.path().to_path_buf(),
        );
        let msg = app
            .history
            .iter()
            .find(|m| m.body.contains("上次跑完了") || m.body.contains("上次会话"))
            .expect("delivery-state should produce a chat hint");
        assert!(msg.body.contains("做个 todo"));
    }

    #[test]
    fn no_resume_hint_for_clean_workspace() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = UserConfig {
            backend: Some("offline".into()),
            model: None,
            ..Default::default()
        };
        let app = App::new(
            "demo",
            cfg,
            std::path::PathBuf::from("/tmp/sd-test-config.toml"),
            tmp.path().to_path_buf(),
        );
        // Greeting still present (always), but no resume hint.
        assert!(!app
            .history
            .iter()
            .any(|m| m.body.contains("docs_confirm") || m.body.contains("上次")));
    }

    // ---- /model + /version + /changelog + typo did-you-mean ----

    #[test]
    fn slash_cancel_returns_cancel_action_only_while_running() {
        let mut a = fresh_app(Some("offline"));
        // Not running → /cancel is a no-op with a hint.
        for c in "/cancel".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        assert!(matches!(a.apply_key(KeyCode::Enter), Action::None));
        assert!(a.history.iter().any(|m| m.body.contains("没有正在运行")));
        // Running → /cancel returns Action::Cancel (event loop aborts the task).
        a.run_started = true;
        a.finished = false;
        for c in "/cancel".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        assert!(matches!(a.apply_key(KeyCode::Enter), Action::Cancel));
        // cancel_run resets state back to a clean prompt.
        a.cancel_run();
        assert!(!a.is_pipeline_active());
        assert!(a.history.iter().any(|m| m.body.contains("已取消")));
    }

    #[test]
    fn enter_on_partial_slash_runs_highlighted_palette_command() {
        let mut a = fresh_app(Some("offline"));
        // "/usag" is a partial that uniquely prefixes "usage".
        for c in "/usag".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        // It RAN /usage (usage summary), not "未知命令 /usag".
        assert!(
            a.history
                .iter()
                .any(|m| m.body.contains("使用统计") || m.body.contains("还没有使用记录")),
            "partial /usag + Enter should run /usage"
        );
        assert!(
            !a.history.iter().any(|m| m.body.contains("未知命令")),
            "should not report unknown command for a resolvable partial"
        );
    }

    #[test]
    fn slash_model_without_arg_prints_usage() {
        let mut a = fresh_app(Some("offline"));
        for c in "/model".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        assert!(a
            .history
            .iter()
            .any(|m| m.body.contains("切换:/model") && m.body.contains("当前 model")));
        // config.model still None.
        assert!(a.config.model.is_none());
    }

    #[test]
    fn slash_model_with_arg_saves_to_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        let cfg = UserConfig {
            backend: Some("offline".into()),
            model: None,
            ..Default::default()
        };
        let mut app = App::new(
            "demo",
            cfg,
            cfg_path.clone(),
            std::path::PathBuf::from("/tmp/sd-test-workspace"),
        );
        for c in "/model claude-opus-4-7".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let _ = app.apply_key(KeyCode::Enter);
        assert_eq!(app.config.model.as_deref(), Some("claude-opus-4-7"));
        // Persisted.
        let loaded = crate::config::load_from(&cfg_path);
        assert_eq!(loaded.model.as_deref(), Some("claude-opus-4-7"));
    }
    // ---- backend / brain-spec selection ----

    #[test]
    fn brain_spec_host_cli_when_no_provider() {
        let app = fresh_app(Some("codex"));
        assert!(matches!(app.brain_spec(), crate::BrainSpec::HostCli(_)));
    }

    #[test]
    fn clarify_answer_appended_to_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "demo".to_string(),
            UserConfig {
                backend: Some("offline".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        // Simulate ClarifyGate open.
        app.active_gate = Some(Gate::ClarifyGate);
        // User types an answer.
        let action = app.submit_text("面向个人开发者".into());
        assert!(matches!(action, Action::None), "answer should not continue");
        // File must exist with the answer.
        let answers =
            std::fs::read_to_string(tmp.path().join("output").join("demo-clarify-answers.md"))
                .unwrap();
        assert!(answers.contains("面向个人开发者"));
    }

    #[test]
    fn clarify_answer_multiple_appends() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "demo".to_string(),
            UserConfig {
                backend: Some("offline".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        app.active_gate = Some(Gate::ClarifyGate);
        app.submit_text("answer 1".into());
        app.submit_text("answer 2".into());
        let answers =
            std::fs::read_to_string(tmp.path().join("output").join("demo-clarify-answers.md"))
                .unwrap();
        assert!(answers.contains("answer 1"));
        assert!(answers.contains("answer 2"));
    }

    #[test]
    fn clarify_c_submits_and_continues() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "demo".to_string(),
            UserConfig {
                backend: Some("offline".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        app.active_gate = Some(Gate::ClarifyGate);
        app.submit_text("my answer".into());
        let action = app.submit_text("c".into());
        assert!(matches!(action, Action::Continue(Gate::ClarifyGate)));
        assert!(app.active_gate.is_none(), "gate must clear on continue");
    }

    #[test]
    fn brain_spec_offline_when_backend_offline() {
        let app = fresh_app(Some("offline"));
        assert!(matches!(app.brain_spec(), crate::BrainSpec::Offline));
    }

    #[test]
    fn deploy_command_reads_delivery_notes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let slug = "demo";
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path().join("output").join(format!("{slug}-delivery-notes.md")),
            "# Delivery\n\n## Deploy command\n\nnpx vercel --prod\n\n## Frontend URL\n\n(not yet deployed)\n",
        ).unwrap();
        let app = App::new(
            slug.to_string(),
            UserConfig {
                backend: Some("offline".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        assert_eq!(
            app.deploy_command_from_notes().as_deref(),
            Some("npx vercel --prod")
        );
        // "(not yet deployed)" is filtered out (not http).
        assert!(app.deploy_url_from_notes().is_none());
    }

    #[test]
    fn deploy_url_reads_live_url() {
        let tmp = tempfile::TempDir::new().unwrap();
        let slug = "demo";
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path()
                .join("output")
                .join(format!("{slug}-delivery-notes.md")),
            "## Frontend URL\n\nhttps://my-app.vercel.app\n",
        )
        .unwrap();
        let app = App::new(
            slug.to_string(),
            UserConfig {
                backend: Some("offline".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        assert_eq!(
            app.deploy_url_from_notes().as_deref(),
            Some("https://my-app.vercel.app")
        );
    }

    #[test]
    fn slash_deploy_without_notes_gives_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "demo".to_string(),
            UserConfig {
                backend: Some("offline".into()),
                lang: Some("zh-CN".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        let action = app.slash_deploy("");
        assert!(matches!(action, Action::None));
        assert!(app
            .history
            .iter()
            .any(|m| m.body.contains("还没有部署指令")));
    }

    #[test]
    fn slash_deploy_with_command_emits_run_deploy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let slug = "demo";
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path()
                .join("output")
                .join(format!("{slug}-delivery-notes.md")),
            "## Deploy command\n\nnpx vercel --prod\n",
        )
        .unwrap();
        let mut app = App::new(
            slug.to_string(),
            UserConfig {
                backend: Some("offline".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        // Bare /deploy only PREVIEWS — it must not deploy without confirmation.
        let preview = app.slash_deploy("");
        assert!(
            matches!(preview, Action::None),
            "bare /deploy is preview-only"
        );
        // Assert on the locale-independent command — the "not yet run" note is
        // i18n'd, so it differs by resolved locale (zh-CN on dev, English on CI).
        assert!(app
            .history
            .iter()
            .any(|m| m.body.contains("npx vercel --prod")));
        // /deploy confirm actually runs it.
        let action = app.slash_deploy("confirm");
        match action {
            Action::RunDeploy { command } => assert_eq!(command, "npx vercel --prod"),
            other => panic!("expected RunDeploy, got {other:?}"),
        }
    }

    #[test]
    fn slash_deploy_floor_requires_confirm_even_in_auto_mode() {
        // Gap 3 reversibility floor: a deploy is an irreversible network action,
        // so even in the AUTO trust tier bare /deploy must NOT fire — it
        // previews and waits for an explicit confirm. `auto` does not get to
        // skip the floor.
        let tmp = tempfile::TempDir::new().unwrap();
        let slug = "demo";
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path()
                .join("output")
                .join(format!("{slug}-delivery-notes.md")),
            "## Deploy command\n\nnpx vercel --prod\n",
        )
        .unwrap();
        let mut app = App::new(
            slug.to_string(),
            UserConfig {
                backend: Some("offline".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        // Force the strictest-skipping tier; the floor must still gate.
        app.trust_mode_override = Some(umadev_agent::TrustMode::Auto);
        assert!(app.effective_trust_mode() == umadev_agent::TrustMode::Auto);
        let preview = app.slash_deploy("");
        assert!(
            matches!(preview, Action::None),
            "auto mode must NOT skip the deploy confirmation floor"
        );
        // Explicit confirm still works.
        match app.slash_deploy("confirm") {
            Action::RunDeploy { command } => assert_eq!(command, "npx vercel --prod"),
            other => panic!("expected RunDeploy after confirm, got {other:?}"),
        }
    }

    #[test]
    fn slash_version_opens_overlay_with_binary_info() {
        let mut a = fresh_app(Some("offline"));
        for c in "/version".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        let ov = a.overlay.as_ref().expect("version overlay");
        let joined = ov.lines.join("\n");
        assert!(joined.contains("umadev"));
        assert!(joined.contains(env!("CARGO_PKG_VERSION")));
        assert!(joined.contains("UMADEV_HOST_SPEC_V1"));
    }

    #[test]
    fn slash_changelog_opens_overlay_with_header() {
        let mut a = fresh_app(Some("offline"));
        for c in "/changelog".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        let ov = a.overlay.as_ref().expect("changelog overlay");
        assert!(ov.lines.iter().any(|l| l.contains("Changelog")));
    }

    #[test]
    fn did_you_mean_suggests_for_typo() {
        // "/quitz" → suggest /quit
        let suggestion = App::did_you_mean("quitz");
        assert_eq!(suggestion, Some("quit"));
    }

    #[test]
    fn did_you_mean_suggests_via_prefix() {
        // "/rev" → /revise (prefix wins)
        let suggestion = App::did_you_mean("rev");
        assert_eq!(suggestion, Some("revise"));
    }

    #[test]
    fn did_you_mean_returns_none_for_garbage() {
        assert_eq!(App::did_you_mean("xxxxxxxxxx"), None);
    }

    #[test]
    fn unknown_slash_command_includes_did_you_mean_hint() {
        let mut a = fresh_app(Some("offline"));
        for c in "/quitz".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        let last = a.history.back().unwrap();
        assert!(last.body.contains("/quitz"));
        assert!(last.body.contains("/quit"));
        assert!(last.body.contains("是想用"));
    }

    #[test]
    fn extract_json_number_pulls_score() {
        let json = r#"{"score": 95, "passed": true, "notes": "ok"}"#;
        assert_eq!(extract_json_number(json, "score"), Some(95));
        assert_eq!(extract_json_number(json, "missing"), None);
    }

    #[test]
    fn extract_json_bool_pulls_passed() {
        let json = r#"{"score": 70, "passed": false}"#;
        assert_eq!(extract_json_bool(json, "passed"), Some(false));
        assert_eq!(extract_json_bool(json, "score"), None);
    }

    #[test]
    fn verify_overlay_surfaces_quality_gate_when_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let out_dir = root.join("output");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(
            out_dir.join("demo-quality-gate.json"),
            r#"{"score": 88, "passed": true}"#,
        )
        .unwrap();

        let mut app = App::new(
            "demo",
            UserConfig {
                backend: Some("offline".into()),
                model: None,
                ..Default::default()
            },
            std::path::PathBuf::from("/tmp/cfg.toml"),
            root.to_path_buf(),
        );
        for c in "/verify".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let _ = app.apply_key(KeyCode::Enter);
        let ov = app.overlay.as_ref().expect("verify overlay");
        let joined = ov.lines.join("\n");
        assert!(joined.contains("Quality gate"));
        assert!(joined.contains("88/100"));
        assert!(joined.contains("PASSED"));
    }

    #[test]
    fn gate_card_lists_artifacts_and_next_steps() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "x".into(),
        });
        a.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        let card = a
            .history
            .iter()
            .find(|m| m.role == ChatRole::Gate)
            .expect("gate card must land in chat");
        // Lists the three core docs by slug.
        assert!(card.body.contains("output/demo-prd.md"));
        assert!(card.body.contains("output/demo-architecture.md"));
        assert!(card.body.contains("output/demo-uiux.md"));
        // Lists next-step verbs.
        assert!(card.body.contains("/continue"));
        assert!(card.body.contains("/revise"));
        assert!(card.body.contains("/diff"));
    }

    #[test]
    fn gate_card_for_preview_confirm_lists_frontend_artifacts() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "shop".into(),
            requirement: "x".into(),
        });
        a.apply_engine(EngineEvent::GateOpened {
            gate: Gate::PreviewConfirm,
        });
        let card = a
            .history
            .iter()
            .find(|m| m.role == ChatRole::Gate)
            .expect("gate card must land in chat");
        assert!(card.body.contains("output/shop-frontend-notes.md"));
        assert!(card.body.contains("output/shop-execution-plan.md"));
    }

    #[test]
    fn gate_card_includes_approval_checklist() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "x".into(),
        });
        a.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        let card = a
            .history
            .iter()
            .find(|m| m.role == ChatRole::Gate)
            .expect("gate card must land in chat");
        // The checklist tells the user WHAT to verify before approving.
        assert!(card.body.contains("审批清单"));
        assert!(card.body.contains("验收标准") || card.body.contains("验收"));
    }

    #[test]
    fn fmt_elapsed_formats_seconds_and_minutes() {
        assert_eq!(fmt_elapsed(5), "5s");
        assert_eq!(fmt_elapsed(59), "59s");
        assert_eq!(fmt_elapsed(60), "1:00");
        assert_eq!(fmt_elapsed(125), "2:05");
        assert_eq!(fmt_elapsed(3661), "61:01");
    }

    #[test]
    fn pipeline_started_sets_run_timer() {
        let mut a = fresh_app(Some("offline"));
        assert!(a.run_started_at.is_none());
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "x".into(),
        });
        assert!(a.run_started_at.is_some(), "run timer must start");
    }

    #[test]
    fn gate_open_stops_run_timer() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "x".into(),
        });
        a.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        // Timer stops while waiting on the user — status bar shouldn't keep
        // ticking during an approval pause.
        assert!(a.run_started_at.is_none());
        assert!(a.phase_started_at.is_none());
    }

    #[test]
    fn verify_failed_appends_actionable_hint() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::VerifyFailed {
            phase: Phase::Frontend,
            exit_code: 1,
            stderr: "error: cannot find module 'react'".into(),
        });
        // The verify-failed line is now localized (the word "verify" itself is
        // translated), so find it by its language-neutral [fail] tag instead.
        let msg = a
            .history
            .iter()
            .find(|m| m.body.contains("[fail]"))
            .expect("verify failure message");
        assert!(msg.body.contains("依赖未安装"), "got: {}", msg.body);
    }

    #[test]
    fn bare_c_at_gate_is_treated_as_continue_shortcut() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        let _ = a.apply_key(KeyCode::Char('c'));
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Continue(Gate::DocsConfirm));
        assert!(a.active_gate.is_none());
    }

    #[test]
    fn bare_c_without_gate_is_plain_chat() {
        let mut a = fresh_app(Some("offline"));
        let _ = a.apply_key(KeyCode::Char('c'));
        let action = a.apply_key(KeyCode::Enter);
        // Outside a gate, "c" is neither approval nor a real requirement.
        assert_eq!(action, Action::Route("c".to_string()));
        assert!(!a.history.iter().any(|m| m.body.contains("直接描述需求")));
    }

    #[test]
    fn chinese_greeting_is_plain_chat_not_pipeline() {
        let mut a = fresh_app(Some("offline"));
        for c in "你好".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Route("你好".to_string()));
        assert!(!a.history.iter().any(|m| m.body.contains("收到需求")));
    }

    #[test]
    fn how_are_you_is_plain_chat_not_pipeline() {
        let mut a = fresh_app(Some("offline"));
        for c in "你好吗？我很好啊".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Route("你好吗？我很好啊".to_string()));
        assert!(!a.history.iter().any(|m| m.body.contains("流水线启动")));
    }

    #[test]
    fn slash_continue_no_run_hint_redirects_to_typing_a_requirement() {
        let mut a = fresh_app(Some("offline"));
        for c in "/continue".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        let last = a.history.back().unwrap();
        assert!(
            last.body.contains("还没启动流水线"),
            "expected redirect hint, got: {}",
            last.body
        );
    }

    #[test]
    fn preflight_message_lands_when_starting_run() {
        let mut a = fresh_app(Some("offline"));
        a.prepare_worker_routed_run("build me a thing");
        // The UmaDev preflight message includes the 9-phase plan.
        assert!(a.history.iter().any(|m| m.role == ChatRole::UmaDev
            && m.body.contains("9 阶段")
            && m.body.contains("docs_confirm")
            && m.body.contains("preview_confirm")));
    }

    // ---- cursor + editing ----

    #[test]
    fn left_arrow_moves_cursor_back_one_char() {
        let mut a = fresh_app(Some("offline"));
        for c in "abc".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        assert_eq!(a.input_cursor, 3);
        let _ = a.apply_key(KeyCode::Left);
        assert_eq!(a.input_cursor, 2);
    }

    #[test]
    fn home_and_end_jump_cursor() {
        let mut a = fresh_app(Some("offline"));
        for c in "abc".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Home);
        assert_eq!(a.input_cursor, 0);
        let _ = a.apply_key(KeyCode::End);
        assert_eq!(a.input_cursor, 3);
    }

    #[test]
    fn forward_delete_removes_char_at_cursor() {
        let mut a = fresh_app(Some("offline"));
        for c in "abc".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Home);
        let _ = a.apply_key(KeyCode::Delete);
        assert_eq!(a.input, "bc");
        assert_eq!(a.input_cursor, 0);
    }

    #[test]
    fn insertion_in_middle_preserves_surrounding_chars() {
        let mut a = fresh_app(Some("offline"));
        for c in "ac".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Left);
        let _ = a.apply_key(KeyCode::Char('b'));
        assert_eq!(a.input, "abc");
        assert_eq!(a.input_cursor, 2);
    }

    #[test]
    fn backspace_respects_cjk_boundary() {
        let mut a = fresh_app(Some("offline"));
        for c in "做个".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        assert_eq!(a.input, "做个");
        // Backspace once → just one CJK char gone, no panic.
        let _ = a.apply_key(KeyCode::Backspace);
        assert_eq!(a.input, "做");
    }

    // ---- Shift+Enter multi-line ----

    #[test]
    fn shift_enter_inserts_newline_and_does_not_submit() {
        let mut a = fresh_app(Some("offline"));
        for c in "line1".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key_with_mods(KeyCode::Enter, crossterm::event::KeyModifiers::SHIFT);
        assert_eq!(action, Action::None);
        assert!(a.input.contains("line1\n"));
        // Cursor advances past the newline.
        assert!(a.input_cursor >= 6);
    }

    #[test]
    fn plain_enter_after_shift_enter_keeps_short_multiline_as_chat() {
        let mut a = fresh_app(Some("offline"));
        for c in "line1".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key_with_mods(KeyCode::Enter, crossterm::event::KeyModifiers::SHIFT);
        for c in "line2".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Route("line1\nline2".to_string()));
    }

    #[test]
    fn plain_enter_after_shift_enter_submits_multiline_requirement() {
        let mut a = fresh_app(Some("offline"));
        for c in "build a login app".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key_with_mods(KeyCode::Enter, crossterm::event::KeyModifiers::SHIFT);
        for c in "with email authentication".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(
            action,
            Action::Route("build a login app\nwith email authentication".to_string())
        );
    }

    // ---- palette ----

    #[test]
    fn palette_matches_filter_by_prefix() {
        let mut a = fresh_app(Some("offline"));
        for c in "/cl".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let matches = a.palette_matches();
        // /claude /clear → 2 matches.
        let verbs: Vec<&str> = matches.iter().map(|(v, _)| *v).collect();
        assert!(verbs.contains(&"claude"));
        assert!(verbs.contains(&"clear"));
    }

    #[test]
    fn arrow_down_navigates_palette_when_active() {
        let mut a = fresh_app(Some("offline"));
        for c in "/c".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let before = a.palette_selected;
        let _ = a.apply_key(KeyCode::Down);
        assert_ne!(a.palette_selected, before);
    }

    #[test]
    fn tab_autocompletes_selected_palette_match() {
        let mut a = fresh_app(Some("offline"));
        for c in "/cla".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Tab);
        assert_eq!(a.input, "/claude ");
    }

    #[test]
    fn arrow_up_with_input_not_in_palette_recalls_history() {
        let mut a = fresh_app(Some("offline"));
        // Submit a prompt to populate history.
        for c in "first request".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        // After submit, input is empty. ↑ should recall it.
        assert!(a.input.is_empty());
        let _ = a.apply_key(KeyCode::Up);
        assert_eq!(a.input, "first request");
    }

    #[test]
    fn arrow_down_at_newest_history_returns_to_fresh_draft() {
        let mut a = fresh_app(Some("offline"));
        for c in "request".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        let _ = a.apply_key(KeyCode::Up);
        assert_eq!(a.input, "request");
        let _ = a.apply_key(KeyCode::Down);
        assert!(a.input.is_empty());
        assert!(a.input_history_idx.is_none());
    }

    #[test]
    fn submit_dedups_consecutive_identical_recalls() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = UserConfig {
            backend: Some("offline".to_string()),
            model: None,
            ..Default::default()
        };
        let mut a = App::new(
            "demo",
            cfg,
            tmp.path().join("config.toml"),
            tmp.path().join("workspace"),
        );
        for c in "same".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        a.finished = true;
        a.run_started = false;
        for c in "same".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        assert_eq!(
            a.input_history
                .iter()
                .filter(|s| s.as_str() == "same")
                .count(),
            1
        );
    }

    #[test]
    fn esc_when_idle_needs_a_second_press_to_quit() {
        let mut a = fresh_app(Some("offline"));
        // First idle Esc arms the confirmation (guards against an accidental
        // quit — including the Esc that just interrupted a run).
        let action = a.apply_key(KeyCode::Esc);
        assert_eq!(action, Action::None);
        assert!(a.pending_quit_confirm);
        // Second Esc actually quits.
        let action = a.apply_key(KeyCode::Esc);
        assert_eq!(action, Action::Quit);
    }

    #[test]
    fn slash_history_opens_overlay_with_messages() {
        let mut a = fresh_app(Some("offline"));
        for c in "/history".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        let ov = a.overlay.as_ref().unwrap();
        assert!(ov
            .lines
            .iter()
            .any(|l| l.contains("[umadev]") || l.contains("[system]")));
    }

    #[test]
    fn overlay_esc_closes() {
        let mut a = fresh_app(Some("offline"));
        for c in "/spec".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        assert!(a.overlay.is_some());
        // Esc should close, NOT quit, when an overlay is open.
        let action = a.apply_key(KeyCode::Esc);
        assert_eq!(action, Action::None);
        assert!(a.overlay.is_none());
        assert!(!a.should_quit);
    }

    #[test]
    fn overlay_scroll_keys() {
        let mut a = fresh_app(Some("offline"));
        for c in "/spec".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        let initial = a.overlay.as_ref().unwrap().scroll;
        // Down + PageDown advance.
        let _ = a.apply_key(KeyCode::Down);
        assert!(a.overlay.as_ref().unwrap().scroll > initial);
        let after_j = a.overlay.as_ref().unwrap().scroll;
        let _ = a.apply_key(KeyCode::PageDown);
        assert!(a.overlay.as_ref().unwrap().scroll > after_j);
        // Up rewinds.
        let _ = a.apply_key(KeyCode::Up);
        // Home resets to 0.
        let _ = a.apply_key(KeyCode::Home);
        assert_eq!(a.overlay.as_ref().unwrap().scroll, 0);
    }

    #[test]
    fn host_output_groups_into_single_bubble() {
        let mut a = fresh_app(Some("offline"));
        let before = a.history.len();
        a.apply_engine(EngineEvent::HostOutput {
            phase: Phase::Research,
            line: "# header".into(),
        });
        a.apply_engine(EngineEvent::HostOutput {
            phase: Phase::Research,
            line: "## section".into(),
        });
        a.apply_engine(EngineEvent::HostOutput {
            phase: Phase::Research,
            line: "body line".into(),
        });
        // All three lines collapse into one Host message.
        let host_msgs: Vec<_> = a
            .history
            .iter()
            .skip(before)
            .filter(|m| m.role == ChatRole::Host)
            .collect();
        assert_eq!(host_msgs.len(), 1);
        let body = &host_msgs[0].body;
        assert!(body.contains("# header"));
        assert!(body.contains("## section"));
        assert!(body.contains("body line"));
    }

    #[test]
    fn host_output_starts_new_bubble_after_umadev_break() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::HostOutput {
            phase: Phase::Research,
            line: "research line".into(),
        });
        // A UmaDev message between the two streams must break the group.
        a.apply_engine(EngineEvent::PhaseCompleted {
            phase: Phase::Research,
        });
        a.apply_engine(EngineEvent::HostOutput {
            phase: Phase::Docs,
            line: "docs line".into(),
        });
        let host_msgs: Vec<_> = a
            .history
            .iter()
            .filter(|m| m.role == ChatRole::Host)
            .collect();
        assert_eq!(host_msgs.len(), 2);
    }

    #[test]
    fn status_bar_contains_phase_dots() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PhaseStarted {
            phase: Phase::Research,
        });
        // Phase progress is a compact geometric bar after the backend label.
        // With research running (first of 9): ◐○○○○○○○○ 0/9.
        assert!(a.status.contains("◐○○○○○○○○"));
        assert!(a.status.contains("0/9"));
    }

    #[test]
    fn status_bar_dots_advance_as_phases_complete() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PhaseStarted {
            phase: Phase::Research,
        });
        a.apply_engine(EngineEvent::PhaseCompleted {
            phase: Phase::Research,
        });
        a.apply_engine(EngineEvent::PhaseStarted { phase: Phase::Docs });
        // After research done + docs running: ●◐○○○○○○○ 1/9.
        assert!(a.status.contains("●◐○○○○○○○"));
        assert!(a.status.contains("1/9"));
    }

    #[test]
    fn spinner_cycles() {
        let mut a = fresh_app(Some("offline"));
        let first = a.spinner();
        // 10 braille frames × 2 ticks each = 20 ticks per cycle.
        for _ in 0..20 {
            a.tick();
        }
        assert_eq!(a.spinner(), first);
    }

    #[test]
    fn running_circle_animates_through_its_frames() {
        // The in-progress phase circle must ROTATE (◐◓◑◒) as the tick advances,
        // not sit static — that rotation is what proves the bar is alive even
        // when the bottom-bar spinner is off-attention. One frame per 2 ticks.
        let mut a = fresh_app(Some("offline"));
        assert_eq!(a.running_circle(), '◐', "frame 0 at tick 0");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            seen.insert(a.running_circle());
            a.tick();
            a.tick(); // advance one full circle frame (~160ms)
        }
        // All four quarter-circle glyphs must appear as it rotates.
        for g in ['◐', '◓', '◑', '◒'] {
            assert!(seen.contains(&g), "running circle must show {g}: {seen:?}");
        }
    }

    #[test]
    fn running_phase_circle_in_status_bar_rotates() {
        // The progress bar (in app.status) shows the rotating circle for the
        // running phase, not a frozen ◐.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PhaseStarted {
            phase: Phase::Research,
        });
        // tick=0 → ◐ (frame 0).
        assert!(a.status.contains('◐'), "tick 0 shows ◐: {}", a.status);
        // Advance two ticks (one circle frame) → the running glyph becomes ◓.
        a.tick();
        a.tick();
        assert!(
            a.status.contains('◓'),
            "after 2 ticks the running circle rotates to ◓: {}",
            a.status
        );
    }

    #[test]
    fn stall_after_three_seconds_then_clears_on_output() {
        // Honest stall signal: a running phase with no output for >3s reads as
        // stalled (status painted red by the UI); any fresh output clears it.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PhaseStarted {
            phase: Phase::Research,
        });
        // Just started → not stalled (spin-up grace).
        assert!(!a.is_stalled(), "a just-started phase is not stalled");
        // Backdate the last-output clock past the 3s threshold.
        a.last_output_at = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(4));
        assert!(a.is_stalled(), "no output for >3s must read as stalled");
        // A worker stream event is a sign of life → stall clears immediately.
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: "back to work".into(),
            },
        });
        assert!(!a.is_stalled(), "fresh output must clear the stall signal");
    }

    #[test]
    fn tool_call_in_flight_is_not_a_stall() {
        // A long tool call (e.g. a 40s npm install) is WORK, not a stall — the
        // red signal must stay suppressed while a ToolUse has no ToolResult yet,
        // even past the 3s threshold; the ToolResult re-arms the stall clock.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PhaseStarted {
            phase: Phase::Backend,
        });
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Bash".into(),
                detail: "npm install".into(),
            },
        });
        assert!(a.tool_in_progress, "ToolUse marks a tool in flight");
        // Even with a stale clock, an in-flight tool is not a stall.
        a.last_output_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(10));
        assert!(
            !a.is_stalled(),
            "an in-flight tool call must NOT read as stalled"
        );
        // The result returns → tool no longer in flight; the stall clock applies.
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolResult {
                ok: true,
                summary: "added 200 packages".into(),
            },
        });
        assert!(!a.tool_in_progress, "ToolResult clears the in-flight flag");
        // (The result itself just marked output, so still not stalled now.)
        assert!(!a.is_stalled());
    }

    #[test]
    fn not_stalled_at_a_gate_or_when_idle() {
        // At a gate (paused for the user) phase_started_at is cleared, so the
        // status must never falsely flash red while waiting on a human.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PhaseStarted { phase: Phase::Docs });
        a.last_output_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(30));
        a.apply_engine(EngineEvent::GateOpened {
            gate: umadev_agent::gates::Gate::DocsConfirm,
        });
        assert!(
            !a.is_stalled(),
            "a gate pause (no running phase) is not a stall"
        );
        // A brand-new app with nothing running is never stalled either.
        let idle = fresh_app(Some("offline"));
        assert!(!idle.is_stalled());
    }

    // ---- WorkerStream rendering tests ----

    #[test]
    fn text_delta_creates_host_message() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: "Hello world".into(),
            },
        });
        let last = a.history.back().unwrap();
        assert_eq!(last.role, ChatRole::Host);
        assert!(last.body.contains("Hello world"));
        assert!(
            a.stream_text_active,
            "stream_text_active should be true after first text"
        );
    }

    #[test]
    fn consecutive_text_deltas_append_not_push() {
        let mut a = fresh_app(Some("offline"));
        // First delta → new message
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: "Part 1".into(),
            },
        });
        // Second delta → append to same message (typewriter)
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: " Part 2".into(),
            },
        });
        let host_msgs: Vec<_> = a
            .history
            .iter()
            .filter(|m| m.role == ChatRole::Host)
            .collect();
        assert_eq!(
            host_msgs.len(),
            1,
            "two consecutive text deltas should be one message"
        );
        assert_eq!(host_msgs[0].body, "Part 1 Part 2");
    }

    #[test]
    fn tool_use_resets_text_append() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: "Some text".into(),
            },
        });
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Read".into(),
                detail: "Cargo.toml".into(),
            },
        });
        // Text after tool should be a NEW message, not appended to tool line
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: "New text".into(),
            },
        });
        assert!(!a.stream_text_active || a.history.back().unwrap().body == "New text");
    }

    #[test]
    fn same_tool_type_batches() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Read".into(),
                detail: "file1".into(),
            },
        });
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Read".into(),
                detail: "file2".into(),
            },
        });
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Read".into(),
                detail: "file3".into(),
            },
        });
        let host_msgs: Vec<_> = a
            .history
            .iter()
            .filter(|m| m.role == ChatRole::Host)
            .collect();
        assert_eq!(
            host_msgs.len(),
            1,
            "3 same-type tool calls should batch to 1 message"
        );
        assert!(
            host_msgs[0].body.contains("(3)"),
            "should show count: {}",
            host_msgs[0].body
        );
        assert!(
            host_msgs[0].body.contains("file3"),
            "should show last detail"
        );
    }

    #[test]
    fn different_tool_type_resets_batch() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Read".into(),
                detail: "file1".into(),
            },
        });
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Bash".into(),
                detail: "npm test".into(),
            },
        });
        let host_msgs: Vec<_> = a
            .history
            .iter()
            .filter(|m| m.role == ChatRole::Host)
            .collect();
        assert_eq!(
            host_msgs.len(),
            2,
            "different tool types should be separate messages"
        );
    }

    #[test]
    fn thinking_indicator_shows() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Thinking,
        });
        let last = a.history.back().unwrap();
        assert_eq!(last.role, ChatRole::System);
        assert!(
            last.body.contains("thinking"),
            "should show thinking indicator: {}",
            last.body
        );
    }

    #[test]
    fn tool_result_shows_checkmark() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolResult {
                ok: true,
                summary: "version = 4.6.0".into(),
            },
        });
        let last = a.history.back().unwrap();
        assert!(last.body.contains("[ok]"), "success should show checkmark");
        assert!(last.body.contains("4.6.0"));
    }

    #[test]
    fn tool_result_error_shows_cross() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolResult {
                ok: false,
                summary: "file not found".into(),
            },
        });
        let last = a.history.back().unwrap();
        assert!(last.body.contains("[fail]"), "error should show cross");
    }

    #[test]
    fn empty_text_delta_ignored() {
        let mut a = fresh_app(Some("offline"));
        let before = a.history.len();
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: "   ".into(),
            },
        });
        assert_eq!(
            a.history.len(),
            before,
            "empty/whitespace text delta should not push"
        );
    }

    #[test]
    fn warning_shows_in_chat() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Warning {
                message: "rate limited".into(),
            },
        });
        let last = a.history.back().unwrap();
        assert!(last.body.contains("rate limited"));
    }

    #[test]
    fn default_trust_mode_is_guarded() {
        // fresh_app writes `.umadevrc` with auto_approve_gates = false, so the
        // default tier is the existing human-in-the-loop behaviour.
        let a = fresh_app(Some("offline"));
        assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);
        assert!(!a.auto_approve_on());
    }

    #[test]
    fn slash_mode_switches_tier_and_keeps_legacy_toggle_consistent() {
        let mut a = fresh_app(Some("offline"));
        a.slash_mode("auto");
        assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Auto);
        assert!(a.auto_approve_on(), "legacy toggle tracks the tier");

        a.slash_mode("plan");
        assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Plan);
        // plan is read-only → gates do NOT auto-approve.
        assert!(!a.auto_approve_on());

        a.slash_mode("guarded");
        assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);

        // Unknown arg is rejected without changing the tier.
        a.slash_mode("nonsense");
        assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);
        assert!(a
            .history
            .iter()
            .any(|m| m.body.contains("nonsense") || m.body.contains("未知")));
    }

    #[test]
    fn plan_mode_does_not_auto_continue_at_gate() {
        let mut a = fresh_app(Some("offline"));
        a.run_started = true;
        a.slash_mode("plan");
        a.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        // Plan is read-only: the gate pauses, never auto-continues.
        assert!(
            a.pending_auto_continue.is_none(),
            "plan mode must not auto-advance the gate"
        );
        assert_eq!(a.active_gate, Some(Gate::DocsConfirm));
    }

    /// Reset any persisted trust state so a leftover `.umadev/trust.json` from a
    /// previous run of the (reused) test workspace can't skew the counters.
    fn reset_trust(a: &mut App) {
        let _ = std::fs::remove_file(a.project_root.join(".umadev").join("trust.json"));
        a.trust_ledger = umadev_agent::TrustLedger::default();
    }

    #[test]
    fn auto_mode_auto_continues_and_records_trust() {
        let mut a = fresh_app(Some("offline"));
        reset_trust(&mut a);
        a.run_started = true;
        a.slash_mode("auto");
        a.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        // Auto tier auto-advances AND books a trust pass for the gate.
        assert_eq!(a.pending_auto_continue, Some(Gate::DocsConfirm));
        assert_eq!(a.trust_ledger.consecutive("docs_confirm"), 1);
    }

    #[test]
    fn manual_approval_builds_trust_and_suggests_at_threshold() {
        let mut a = fresh_app(Some("offline"));
        reset_trust(&mut a);
        // Guarded default: manually approve the docs gate enough times in a row
        // that the ledger surfaces a one-time auto-advance suggestion.
        for _ in 0..umadev_agent::trust::SUGGEST_THRESHOLD {
            a.active_gate = Some(Gate::DocsConfirm);
            let action = a.submit_text("c".to_string());
            assert_eq!(action, Action::Continue(Gate::DocsConfirm));
        }
        assert_eq!(
            a.trust_ledger.consecutive("docs_confirm"),
            umadev_agent::trust::SUGGEST_THRESHOLD
        );
        assert!(
            a.history.iter().any(|m| m.body.contains("[trust]")),
            "a trust suggestion should have fired once at the threshold"
        );
    }

    #[test]
    fn revision_resets_trust_streak() {
        let mut a = fresh_app(Some("offline"));
        reset_trust(&mut a);
        a.active_gate = Some(Gate::PreviewConfirm);
        let _ = a.submit_text("c".to_string());
        assert_eq!(a.trust_ledger.consecutive("preview_confirm"), 1);
        // A revision at the gate walks back the accumulated trust.
        a.active_gate = Some(Gate::PreviewConfirm);
        let _ = a.submit_text("把图标换成 lucide".to_string());
        assert_eq!(a.trust_ledger.consecutive("preview_confirm"), 0);
    }

    // ---- input-correctness hardening (wave 3) ----------------------------

    #[test]
    fn unrelated_note_does_not_clear_thinking_but_route_result_does() {
        let mut a = fresh_app(Some("offline"));
        // A routed chat turn is in flight.
        a.thinking = true;
        a.thinking_started = Some(std::time::Instant::now());
        // An UNRELATED progress note (heartbeat / resume-retry / governance)
        // must NOT extinguish the animation — the route is still running.
        a.apply_engine(EngineEvent::Note("route.resume_retry: retrying".into()));
        assert!(
            a.thinking,
            "a bare progress Note must not clear thinking while a route is in flight"
        );
        // A TERMINAL route outcome DOES clear it: first the failure path…
        a.record_route_failed("route failed: boom".into());
        assert!(!a.thinking, "a failed route result clears thinking");
        assert!(a.thinking_started.is_none());
        // …and the normal reply path too.
        a.thinking = true;
        a.thinking_started = Some(std::time::Instant::now());
        a.record_chat_reply("hello back".into());
        assert!(!a.thinking, "a chat reply clears thinking");
        assert!(a.thinking_started.is_none());
    }

    #[test]
    fn submitting_while_thinking_queues_instead_of_routing_concurrently() {
        let mut a = fresh_app(Some("offline"));
        // First turn: nothing running → routes, and marks thinking.
        let first = a.submit_text("first message".to_string());
        assert!(matches!(first, Action::Route(_)), "first turn routes");
        assert!(a.thinking, "first routed turn marks thinking");
        assert!(a.queued_chat.is_empty());
        // Second turn WHILE thinking: must NOT spawn a second route — it parks.
        let second = a.submit_text("second message".to_string());
        assert_eq!(
            second,
            Action::None,
            "a turn submitted while thinking must not route concurrently"
        );
        assert_eq!(a.queued_chat.len(), 1, "the extra turn is queued");
        assert_eq!(
            a.queued_chat.front().map(String::as_str),
            Some("second message")
        );
        // A third also queues (FIFO order preserved).
        let _ = a.submit_text("third message".to_string());
        assert_eq!(a.queued_chat.len(), 2);
        assert_eq!(a.take_next_queued_chat().as_deref(), Some("second message"));
        assert_eq!(a.take_next_queued_chat().as_deref(), Some("third message"));
    }

    #[test]
    fn ctrl_c_interrupts_a_running_pipeline_even_with_nonempty_input() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "build".into(),
        });
        assert!(a.is_pipeline_active());
        // Half-typed next message in the box.
        for c in "half typed".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        assert!(!a.input.is_empty());
        // Ctrl-C while running → INTERRUPT immediately (Claude Code parity),
        // not just clear the input.
        let action =
            a.apply_key_with_mods(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL);
        assert_eq!(
            action,
            Action::Cancel,
            "Ctrl-C interrupts a running pipeline"
        );
        assert!(
            a.input.is_empty(),
            "the half-typed input is dropped on interrupt"
        );
    }

    #[test]
    fn ctrl_c_while_thinking_stops_the_spinner_and_drops_the_queue() {
        let mut a = fresh_app(Some("offline"));
        // A route in flight, with extra turns parked behind it.
        a.thinking = true;
        a.thinking_started = Some(std::time::Instant::now());
        a.queued_chat.push_back("parked".into());
        for c in "typing".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action =
            a.apply_key_with_mods(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL);
        assert_eq!(action, Action::None);
        assert!(!a.thinking, "Ctrl-C while thinking stops the animation");
        assert!(a.thinking_started.is_none());
        assert!(
            a.queued_chat.is_empty(),
            "parked turns are cleared on interrupt"
        );
        assert!(a.input.is_empty());
    }

    #[test]
    fn ctrl_c_on_empty_idle_input_arms_quit_confirm() {
        // Regression guard for the idle path: with nothing running and an empty
        // box, Ctrl-C still falls through to the Esc (quit-confirm) semantics.
        let mut a = fresh_app(Some("offline"));
        let action =
            a.apply_key_with_mods(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL);
        assert_eq!(action, Action::None);
        assert!(
            a.pending_quit_confirm,
            "idle empty Ctrl-C arms quit confirm"
        );
    }

    #[test]
    fn queued_count_reflects_chat_queue_and_steer() {
        let mut a = fresh_app(Some("offline"));
        assert_eq!(a.queued_count(), 0, "nothing queued initially");
        a.queued_chat.push_back("a".into());
        a.queued_chat.push_back("b".into());
        assert_eq!(a.queued_count(), 2, "chat queue counts");
        a.queued_steer.push_back("steer".into());
        assert_eq!(a.queued_count(), 3, "a pending steer adds to the count");
        a.queued_chat.clear();
        a.queued_steer.clear();
        assert_eq!(a.queued_count(), 0, "clears back to zero");
    }
}
