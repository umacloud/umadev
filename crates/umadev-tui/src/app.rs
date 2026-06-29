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
/// A Host/UmaDev text body (or a tool result) past this many SOURCE lines is
/// foldable: the renderer shows a head-N preview + a `… N more lines` summary
/// until the user expands it (Ctrl+R). This is what stops a single 998-line base
/// reply from flooding the whole transcript. Counted on raw `\n`-split lines
/// (cheap, pre-wrap); a borderline message that wraps to more visual rows is
/// fine — the head-N is generous.
pub(crate) const FOLD_THRESHOLD: usize = 20;
/// Head lines kept when a long GENERAL (text / non-shell) body is folded.
pub(crate) const FOLD_HEAD_GENERAL: usize = 3;
/// Head lines kept when a long SHELL (Bash) tool result is folded — shell output
/// gets a deeper preview (the tail of a build log is usually the signal).
pub(crate) const FOLD_HEAD_SHELL: usize = 10;
/// Max conversation-memory messages handed to the base per routed turn.
/// Bounds prompt growth (≈ the last 8 user/assistant exchanges) while keeping
/// enough context for the base to follow a multi-turn dialogue.
const CONVERSATION_CAP: usize = 16;
/// Max chars in the input box.
const INPUT_CAP: usize = 8192;

/// Marker prefix on the live `Thinking` placeholder System row (P5c). Used to
/// re-validate the row before collapsing it to a summary, so a shifted/rolled-off
/// history index can never rewrite an unrelated row. Also the structural sentinel
/// the renderer keys off to fold a reasoning block: a System row whose first line
/// starts with this tag and that has reasoning lines below is a collapsible
/// `[thinking]` block (the base's extended-thinking text, default-collapsed).
pub(crate) const THINKING_PLACEHOLDER_TAG: &str = "[thinking]";

/// Hard cap (bytes) on the reasoning text accumulated into ONE `[thinking]` block,
/// so a long extended-thinking stream can never blow up the transcript / memory.
/// Past this the block stops growing (the early reasoning is the useful part).
pub(crate) const THINKING_REASONING_MAX: usize = 16_000;

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
    /// `/goal <objective>` — start a goal-driven director build: the base keeps
    /// working toward the objective until it's met (Claude Code's native persistent
    /// `/goal` framing on a capable base; a "don't stop early" prompt fallback on
    /// the rest). Rides the SAME director-build path as [`Self::StartRun`] with the
    /// full system (design / team / knowledge / evolution); the only difference is
    /// the goal-mode flag the event loop forwards into the director loop.
    StartGoal(String),
    /// `/continue` on a FRESH session (no in-memory gate) with a persisted,
    /// resumable director-loop run on disk — re-attach to the saved plan and drive
    /// only the remaining steps instead of restarting the whole pipeline. Carries
    /// the requirement (read back from `.umadev/workflow-state.json` when the
    /// in-memory one is empty). Rides the SAME director-build path as
    /// [`Self::StartRun`], only with the loop's resume entry selected.
    ResumeRun(String),
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
    /// `Ctrl+L` / `/redraw` — clear the screen and force a full repaint on the
    /// next frame. The escape hatch that recovers from any accumulated
    /// incremental-diff desync (stale cells, leftover prefixes, bled long lines)
    /// — worse on flaky Windows consoles. The event loop owns `terminal`, so the
    /// actual `terminal.clear()` is issued there, not from the app model.
    ForceRedraw,
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

/// Honest first-run readiness of a base, surfaced in the picker (Wave 1
/// deliverable 5 / gap G10). Three states the user can act on, plus an
/// indeterminate `Unknown` that conservatively reads as "login may be required"
/// — the picker NEVER shows a false green.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AuthMark {
    /// Installed AND confidently logged in — the only honest green "ready".
    LoggedIn,
    /// Installed but NOT logged in — show the base's login command, block commit.
    NotLoggedIn,
    /// Not installed — show the install command, block commit.
    NotInstalled,
    /// Could not determine login state (probe errored / timed out). Conservative:
    /// reads as "login may be required", does NOT block commit (the base IS
    /// installed; a false-block would be worse than an honest "may need login").
    Unknown,
}

impl AuthMark {
    /// Parse the structured tag `spawn_probe` prefixes onto the probe `detail`
    /// (`auth=logged_in|not_logged_in|not_installed|unknown;…`). Fail-open: an
    /// unrecognised / absent tag is [`Self::Unknown`] (never a false green).
    #[must_use]
    pub fn from_tag(s: &str) -> Self {
        match s {
            "logged_in" => Self::LoggedIn,
            "not_logged_in" => Self::NotLoggedIn,
            "not_installed" => Self::NotInstalled,
            _ => Self::Unknown,
        }
    }
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
    /// Honest auth/install state (gap G10) — drives the three-state picker mark
    /// and the not-ready commit block. Defaults to [`AuthMark::Unknown`].
    pub auth: AuthMark,
    /// The base's login command (e.g. `claude auth login`), shown when
    /// [`Self::auth`] is [`AuthMark::NotLoggedIn`]. Empty when none / unknown.
    pub login_cmd: String,
    /// The base's install command (e.g. `npm install -g …`), shown when
    /// [`Self::auth`] is [`AuthMark::NotInstalled`]. Empty when none / unknown.
    pub install_cmd: String,
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
    /// Honest auth state (gap G10) — drives the three-state readiness mark and
    /// the not-ready commit block. [`AuthMark::Unknown`] until probed.
    pub auth: AuthMark,
    /// Login command shown on [`AuthMark::NotLoggedIn`] (empty when none).
    pub login_cmd: String,
    /// Install command shown on [`AuthMark::NotInstalled`] (empty when none).
    pub install_cmd: String,
}

/// One step in the live, UmaDev-owned plan checklist (Wave 1 deliverable 2/3).
/// Mirrors a `umadev_agent::plan_state::PlanStep` flattened to the cheap summary
/// the TUI renders: an id (matches a `PlanStepStatus` event's `id`), a title, and
/// a status string (`pending` / `active` / `done` / `blocked`). Kept as plain
/// strings so the panel renders without re-importing the agent's typed plan.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PlanStepRow {
    /// Stable step id — matches the leading id of a `PlanPosted` summary and the
    /// `id` field of a `PlanStepStatus` transition.
    pub id: String,
    /// Human-readable step title (the checklist label).
    pub title: String,
    /// Current status id: `pending` / `active` / `done` / `blocked`. Any other
    /// value renders as a neutral pending dot (fail-open).
    pub status: String,
}

/// One reviewing seat's verdict in the collapsible team-review panel (Wave 1
/// deliverable 3). Flattened from a `umadev_agent::critics::RoleVerdict`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CriticRow {
    /// The reviewing seat's role id (e.g. `architect`, `qa`).
    pub seat: String,
    /// Whether the seat accepts the artifacts as-is.
    pub accepts: bool,
    /// Must-fix findings (may be empty).
    pub blocking: Vec<String>,
    /// Nice-to-have notes (may be empty).
    pub advisory: Vec<String>,
}

/// The standing development team rendered by `/team` — the eight specialist
/// seats plus the coordinator, in delivery order. Each entry is one i18n key
/// whose string names the role AND the artifact it produces (the
/// role→deliverable model surfaced by Wave C of the development-team
/// repositioning). Kept module-level so the roster is one source of truth shared
/// by `slash_team` and its tests.
const TEAM_ROSTER: &[&str] = &[
    "team.roster.pm",
    "team.roster.architect",
    "team.roster.designer",
    "team.roster.frontend",
    "team.roster.backend",
    "team.roster.qa",
    "team.roster.security",
    "team.roster.devops",
    "team.roster.coordinator",
];

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
    /// A LOUD, high-risk warning — rendered bold in the theme's error red (the
    /// same red as a failed tool / blocked review row). Reserved for warnings
    /// the user must not miss, e.g. the codex `danger-full-access` sandbox
    /// notice at startup.
    Error,
}

/// Lifecycle of a structured tool call shown in the transcript. Drives the
/// status glyph (queued = dim, running = spinner, ok = green, fail = red) and
/// the auto-collapse policy (a finished OK call collapses; running / failed
/// always stay expanded).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ToolStatus {
    /// Announced but not yet started (rare on the stream path; reserved).
    Queued,
    /// In flight — the base is executing the tool right now.
    Running,
    /// Completed successfully.
    Ok,
    /// Completed with an error.
    Fail,
    /// Settled by an interrupt: the run/turn ended (idle settle, Cancel, base
    /// error) before the tool's matching result arrived, so the call never
    /// reported Ok/Fail. Terminal + NEUTRAL — rendered dim with an `[aborted]`
    /// tag, never a fake success — so a stack of base tool rows can't keep
    /// spinning forever after the run is over.
    Aborted,
}

impl ToolStatus {
    /// `true` once the call has reached a terminal state (used by the
    /// auto-collapse policy: only a finished call may collapse). `Aborted` is
    /// terminal — an interrupted call is settled, just without a result.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ToolStatus::Ok | ToolStatus::Fail | ToolStatus::Aborted
        )
    }
}

/// A structured tool invocation rendered as a single status line (a status
/// glyph, the bold name, then the dim primary argument) with its result folded
/// into a dim gutter line below. Replaces the old path that flattened a tool
/// call into a sentence-like string, so a write/edit no longer reads like prose.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ToolCall {
    /// The tool's name as the base reports it (`Read` / `Edit` / `Bash` …).
    pub name: String,
    /// The primary argument (a path, a query, a command) — already truncated
    /// to a sane width; rendered dim in parentheses after the name.
    pub arg: String,
    /// Current lifecycle state — drives the status glyph + auto-collapse.
    pub status: ToolStatus,
    /// The result summary once the call returns (`None` while in flight).
    pub result: Option<String>,
    /// `true` when a low-signal read/grep/glob batch was merged into this one
    /// line; `count` then carries how many calls folded together.
    pub merged: bool,
    /// How many calls this row represents (1 unless `merged`). Stored as the
    /// "greatest seen" so a streaming count never visibly jumps backwards.
    /// `u32` to line up with the streaming batch counter (`stream_tool_batch`).
    pub count: u32,
    /// Whether the result body is currently folded to a summary line. A
    /// finished OK call defaults to collapsed; a running / failed call is
    /// always shown expanded.
    pub collapsed: bool,
}

/// One UI command-palette section. Help groups render in this declared order,
/// and every [`SlashCommand`] names exactly one group, so the help overlay can
/// be GENERATED from [`App::COMMANDS`] alone — there are no hand-curated help
/// rows left to drift out of sync with the palette / dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdGroup {
    /// Pick / switch the worker (base CLI) that supplies the brain.
    Worker,
    /// Drive the delivery pipeline and its gates.
    Pipeline,
    /// Take the finished work live.
    Ship,
    /// Read-only inspection of state, artifacts, and history.
    Inspect,
    /// UI / onboarding settings (language, animations, redraw, …).
    System,
    /// Conversation lifecycle and exit.
    Session,
}

impl CmdGroup {
    /// All groups in help-overlay render order.
    pub const ALL: &'static [CmdGroup] = &[
        CmdGroup::Worker,
        CmdGroup::Pipeline,
        CmdGroup::Ship,
        CmdGroup::Inspect,
        CmdGroup::System,
        CmdGroup::Session,
    ];

    /// i18n key for this group's help-overlay heading.
    #[must_use]
    pub fn title_key(self) -> &'static str {
        match self {
            CmdGroup::Worker => "tui.help.group.worker",
            CmdGroup::Pipeline => "tui.help.group.pipeline",
            CmdGroup::Ship => "tui.help.group.ship",
            CmdGroup::Inspect => "tui.help.group.inspect",
            CmdGroup::System => "tui.help.group.system",
            CmdGroup::Session => "tui.help.group.session",
        }
    }
}

/// One slash command — the SINGLE source of truth shared by the palette
/// autocomplete ([`App::palette_matches`]), the help overlay
/// (`render_help_overlay`), and the dispatch resolver ([`App::resolve_command`]
/// → `try_slash_command`).
///
/// Before this registry, those three surfaces were three hand-kept tables that
/// had already drifted: `/model` was dispatchable yet absent from the palette;
/// roughly a dozen verbs never appeared in help; many aliases (`q`/`exit`,
/// `?`/`commands`, `abort`, `snapshot`, …) lived only in the dispatch `match`. A
/// parity test (`commands_and_dispatch_are_in_lockstep`) now locks the registry
/// against the dispatcher so the surfaces can never diverge again.
#[derive(Debug, Clone, Copy)]
pub struct SlashCommand {
    /// Canonical verb (typed after `/`, no slash). Also the dispatch-arm key.
    pub name: &'static str,
    /// Alternative spellings that resolve to `name` (e.g. `q`/`exit` → `quit`).
    pub aliases: &'static [&'static str],
    /// Argument hint shown as dim ghost text in the palette / help (`<id>`,
    /// `[slug]`). `None` for a verb that takes no arguments.
    pub arg_hint: Option<&'static str>,
    /// Which help-overlay section this command renders under.
    pub group: CmdGroup,
    /// i18n key for the one-line description shared by the palette and help.
    pub desc_key: &'static str,
    /// Hidden from the palette + help (still dispatchable). None today; kept so a
    /// future internal verb can be wired without advertising it.
    pub hidden: bool,
}

/// One palette autocomplete row: the verb, its localized description, and an
/// optional argument hint rendered as dim ghost text. Built per-keystroke by
/// [`App::palette_matches`] from [`App::COMMANDS`] (+ the dynamic per-backend
/// verbs) for the active language.
#[derive(Debug, Clone)]
pub struct PaletteEntry {
    /// The verb to insert on autocomplete (no leading slash).
    pub verb: &'static str,
    /// Localized one-line description (already resolved for the active language).
    pub desc: &'static str,
    /// Optional dim ghost-text argument hint.
    pub arg_hint: Option<&'static str>,
}

/// One rendered line of a diff card. The `tag` is the gutter marker; `line_no`
/// is the (1-based) line number in the AFTER file for an add/context line, or in
/// the BEFORE file for a deletion — whichever the row belongs to (so the number
/// column tracks the file you can actually open). `text` is the raw content
/// WITHOUT the +/-/space prefix (the gutter carries that), syntax-highlighted by
/// the renderer per the file extension.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DiffLine {
    /// Gutter marker: `'+'` add, `'-'` delete, `' '` unchanged context.
    pub tag: char,
    /// 1-based line number in the relevant side of the file (`None` is never
    /// used today but reserved for a marker row).
    pub line_no: Option<u32>,
    /// Raw line content (no leading +/-/space).
    pub text: String,
    /// Word-level changed regions WITHIN this `+`/`-` line, as **byte** ranges
    /// `(start, end)` into [`text`](Self::text) (already sorted, non-overlapping,
    /// on char boundaries). Populated by pairing each deletion run with the
    /// following insertion run and word-diffing the two sides
    /// ([`FileDiff::from_tool_edit`]). When **empty** the renderer falls back to
    /// whole-line emphasis (an unpaired line, a near-total rewrite that tripped
    /// the change-ratio threshold, or a context row) — so an empty vec is the
    /// safe default and the fail-open path.
    pub changed: Vec<(usize, usize)>,
}

/// A contiguous block of changed + surrounding-context lines (one `@@` hunk),
/// already trimmed to ±N context lines.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DiffHunk {
    /// The lines of this hunk in display order.
    pub lines: Vec<DiffLine>,
}

/// A structured file diff rendered as a Claude-Code-style diff card: a header
/// (`path (+N −M)`), a dashed top/bottom frame, and per-line gutter (marker +
/// right-aligned line number) with syntax-highlighted content. Built from a
/// [`umadev_runtime::ToolEdit`] (a `Write`/`Edit` the base actually performed),
/// so the user sees code being added / changed in real time instead of a bare
/// `Write src/app.tsx` row.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FileDiff {
    /// The edited file's path (rendered in the header + used for the language
    /// hint that drives syntax highlighting).
    pub path: String,
    /// Count of added (`+`) lines, for the header metric.
    pub added: u32,
    /// Count of removed (`-`) lines, for the header metric.
    pub removed: u32,
    /// The change hunks (each already ±context-trimmed).
    pub hunks: Vec<DiffHunk>,
    /// Whether the card is folded to its one-line header (a big diff defaults to
    /// collapsed; toggled by Ctrl+R, reusing the P6 fold lever).
    pub collapsed: bool,
}

/// Context lines kept on each side of a change run inside a hunk (Claude-Code's
/// diff card shows a few lines of surrounding code, not the whole file).
pub(crate) const DIFF_CONTEXT: usize = 3;

/// A diff whose total rendered line count exceeds this defaults to collapsed (it
/// shows just the `path (+N −M) · Ctrl+R 展开` header until expanded). Reuses the
/// P6 fold lever so a big rewrite doesn't bury the conversation.
pub(crate) const DIFF_FOLD_THRESHOLD: usize = 24;

impl FileDiff {
    /// Build a diff card from a structured [`umadev_runtime::ToolEdit`].
    ///
    /// Runs a line-level diff (`similar`, pure-Rust Myers) over `before → after`,
    /// groups the changes into hunks with ±[`DIFF_CONTEXT`] lines of surrounding
    /// context, counts the +/- lines for the header, and pre-folds a big diff to
    /// its header (`collapsed`). The line numbers track the AFTER file for
    /// add/context rows and the BEFORE file for deletions, so each number points
    /// at the side you can actually open.
    ///
    /// **Fail-open by construction:** this is pure data assembly over two
    /// strings — it never errors, never panics, and an empty/no-op edit simply
    /// yields a card with zero hunks (the caller can choose to skip it).
    #[must_use]
    pub fn from_tool_edit(edit: &umadev_runtime::ToolEdit) -> Self {
        use similar::{ChangeTag, TextDiff};

        let diff = TextDiff::from_lines(&edit.before, &edit.after);

        // First pass: a flat tagged stream with running line numbers per side
        // (reusing `DiffLine` directly — its shape already matches).
        let mut flat: Vec<DiffLine> = Vec::new();
        let (mut old_no, mut new_no) = (0u32, 0u32);
        let mut added = 0u32;
        let mut removed = 0u32;
        for change in diff.iter_all_changes() {
            // `similar` yields each line WITH its trailing '\n'; strip it so the
            // gutter/number columns line up and the renderer owns line breaks.
            let text = change
                .value()
                .strip_suffix('\n')
                .unwrap_or(change.value())
                .to_string();
            match change.tag() {
                ChangeTag::Equal => {
                    old_no += 1;
                    new_no += 1;
                    flat.push(DiffLine {
                        tag: ' ',
                        line_no: Some(new_no),
                        text,
                        changed: Vec::new(),
                    });
                }
                ChangeTag::Delete => {
                    old_no += 1;
                    removed += 1;
                    flat.push(DiffLine {
                        tag: '-',
                        line_no: Some(old_no),
                        text,
                        changed: Vec::new(),
                    });
                }
                ChangeTag::Insert => {
                    new_no += 1;
                    added += 1;
                    flat.push(DiffLine {
                        tag: '+',
                        line_no: Some(new_no),
                        text,
                        changed: Vec::new(),
                    });
                }
            }
        }

        // Word-level pass: pair each maximal run of consecutive `-` lines with
        // the run of `+` lines that immediately follows it, line-up the two runs
        // by position, and for each `(del_line, ins_line)` pair compute the
        // changed byte ranges on BOTH sides (`similar::TextDiff::from_words`).
        // The result is stored on `DiffLine.changed`, so the renderer can
        // emphasise only the changed words and syntax-highlight the rest. Lines
        // with no pair (a pure add / pure delete, or the ragged tail of an
        // uneven run) keep `changed == []` → whole-line emphasis fallback. Fully
        // local + fail-open: any anomaly leaves `changed` empty.
        {
            let mut i = 0;
            while i < flat.len() {
                if flat[i].tag != '-' {
                    i += 1;
                    continue;
                }
                // Maximal '-' run [i, del_end), then the immediately-following
                // '+' run [del_end, ins_end).
                let del_start = i;
                let mut del_end = i;
                while del_end < flat.len() && flat[del_end].tag == '-' {
                    del_end += 1;
                }
                let ins_start = del_end;
                let mut ins_end = ins_start;
                while ins_end < flat.len() && flat[ins_end].tag == '+' {
                    ins_end += 1;
                }
                // Pair the two runs position-by-position (the common prefix
                // length); a ragged tail on either side stays whole-line.
                let pairs = (del_end - del_start).min(ins_end - ins_start);
                for k in 0..pairs {
                    let (del_text, ins_text) = (
                        flat[del_start + k].text.clone(),
                        flat[ins_start + k].text.clone(),
                    );
                    let (del_ranges, ins_ranges) = word_diff_ranges(&del_text, &ins_text);
                    flat[del_start + k].changed = del_ranges;
                    flat[ins_start + k].changed = ins_ranges;
                }
                // Advance past whichever run(s) we consumed; if there was no
                // following '+' run, still step past the '-' run.
                i = ins_end.max(del_end);
            }
        }

        // Second pass: keep only changed lines + ±DIFF_CONTEXT around them, and
        // split into hunks wherever the gap between kept regions is larger than
        // 2×context (so distant edits don't merge into one giant block).
        let n = flat.len();
        let mut keep = vec![false; n];
        for (i, t) in flat.iter().enumerate() {
            if t.tag != ' ' {
                let lo = i.saturating_sub(DIFF_CONTEXT);
                let hi = (i + DIFF_CONTEXT + 1).min(n);
                for k in keep.iter_mut().take(hi).skip(lo) {
                    *k = true;
                }
            }
        }

        let mut hunks: Vec<DiffHunk> = Vec::new();
        let mut cur: Vec<DiffLine> = Vec::new();
        for (i, t) in flat.iter().enumerate() {
            if keep[i] {
                cur.push(t.clone());
            } else if !cur.is_empty() {
                hunks.push(DiffHunk {
                    lines: std::mem::take(&mut cur),
                });
            }
        }
        if !cur.is_empty() {
            hunks.push(DiffHunk { lines: cur });
        }

        // Default-collapse a big diff (total rendered rows over the threshold).
        let total_rows: usize = hunks.iter().map(|h| h.lines.len()).sum();
        let collapsed = total_rows > DIFF_FOLD_THRESHOLD;

        Self {
            path: edit.path.clone(),
            added,
            removed,
            hunks,
            collapsed,
        }
    }

    /// The greatest absolute line number appearing in the diff — used to size the
    /// fixed-width gutter number column (so all rows align). `0` for an empty
    /// diff (the renderer then uses a minimal column).
    #[must_use]
    pub fn max_line_no(&self) -> u32 {
        self.hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .filter_map(|l| l.line_no)
            .max()
            .unwrap_or(0)
    }

    /// Total rendered content rows (across all hunks), for the fold decision.
    #[must_use]
    pub fn total_rows(&self) -> usize {
        self.hunks.iter().map(|h| h.lines.len()).sum()
    }
}

/// Changed **byte** ranges `(start, end)` within one line — the word-level diff
/// signal carried on [`DiffLine::changed`] and returned by [`word_diff_ranges`].
/// Sorted, non-overlapping, on char boundaries. A named alias keeps the paired
/// `(del, ins)` return type readable (and satisfies clippy's `type_complexity`).
type WordRanges = Vec<(usize, usize)>;

/// Above this change ratio a paired line is treated as a near-total rewrite:
/// the word-level ranges are discarded (returned empty) so the renderer falls
/// back to whole-line emphasis instead of confetti-highlighting almost every
/// token. `changed_len / total_len` is measured in **bytes** on the longer side
/// of the pair. `0.4` matches the task's noise-avoidance threshold.
pub(crate) const DIFF_WORD_REWRITE_RATIO: f32 = 0.4;

/// Word-level diff of a `(deleted_line, inserted_line)` pair.
///
/// Returns `(del_ranges, ins_ranges)` — the changed **byte** ranges on each
/// side (sorted, non-overlapping, char-boundary-aligned, into the respective
/// input string), suitable for slicing the line into "unchanged" vs "changed"
/// spans. Uses `similar::TextDiff::from_words`, walking the change stream with a
/// running byte cursor per side: a `Delete` advances + records on the old side,
/// an `Insert` on the new side, an `Equal` advances both.
///
/// **Threshold fallback:** if the changed bytes exceed
/// [`DIFF_WORD_REWRITE_RATIO`] of the longer side, BOTH range vectors come back
/// empty (the caller then whole-line-highlights — a near-rewrite has no useful
/// word signal). Two identical lines also yield `([], [])`.
///
/// **Fail-open + CJK-safe:** ranges are exact byte offsets accumulated from the
/// word slices `similar` returns (themselves cut on grapheme/word boundaries),
/// so they never split a UTF-8 sequence; the renderer width-measures the slices
/// with `unicode-width`. Pure data, never panics.
#[must_use]
fn word_diff_ranges(del: &str, ins: &str) -> (WordRanges, WordRanges) {
    use similar::{ChangeTag, TextDiff};

    if del == ins {
        return (Vec::new(), Vec::new());
    }

    let diff = TextDiff::from_words(del, ins);
    let mut del_ranges: WordRanges = Vec::new();
    let mut ins_ranges: WordRanges = Vec::new();
    let (mut del_cur, mut ins_cur) = (0usize, 0usize);
    let (mut del_changed, mut ins_changed) = (0usize, 0usize);

    for change in diff.iter_all_changes() {
        let len = change.value().len();
        match change.tag() {
            ChangeTag::Equal => {
                del_cur += len;
                ins_cur += len;
            }
            ChangeTag::Delete => {
                push_range(&mut del_ranges, del_cur, del_cur + len);
                del_cur += len;
                del_changed += len;
            }
            ChangeTag::Insert => {
                push_range(&mut ins_ranges, ins_cur, ins_cur + len);
                ins_cur += len;
                ins_changed += len;
            }
        }
    }

    // Near-total rewrite → drop the word signal, whole-line highlight instead.
    let longer = del.len().max(ins.len()).max(1);
    let changed = del_changed.max(ins_changed);
    #[allow(clippy::cast_precision_loss)]
    if (changed as f32) / (longer as f32) > DIFF_WORD_REWRITE_RATIO {
        return (Vec::new(), Vec::new());
    }

    (del_ranges, ins_ranges)
}

/// Append `[start, end)` to `ranges`, coalescing with the previous range when
/// they touch/overlap (so adjacent changed words render as one contiguous
/// emphasis span instead of many). Empty ranges are ignored.
fn push_range(ranges: &mut WordRanges, start: usize, end: usize) {
    if end <= start {
        return;
    }
    if let Some(last) = ranges.last_mut() {
        if start <= last.1 {
            last.1 = last.1.max(end);
            return;
        }
    }
    ranges.push((start, end));
}

/// True when every char of `needle` appears in `haystack` in order (a fuzzy
/// subsequence match, case-sensitive — callers lowercase first). Powers the
/// slash-command palette's fuzzy filter (`dpl` → `deploy`).
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars();
    needle.chars().all(|nc| hay.any(|hc| hc == nc))
}

/// True for a char allowed inside an `@`-file-mention token (`[\w./-]`, plus any
/// Unicode alphanumeric so a non-ASCII path component still matches): word chars,
/// underscore, dot, slash, dash. The `@`-typeahead reads the contiguous run of
/// these immediately before the cursor as the partial path being filtered.
fn is_mention_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '-')
}

/// Maximum number of repo files offered as `@`-mention candidates (bounds both
/// the one-time scan and the per-frame filter cost).
const MENTION_FILE_CAP: usize = 2000;
/// Maximum directory depth the `@`-mention scan descends (a guard against a
/// pathological tree; the user's files of interest are shallow).
const MENTION_SCAN_DEPTH: usize = 12;
/// Maximum number of ranked candidates shown in the `@`-mention popover at once.
const MENTION_MATCH_CAP: usize = 50;

/// Walk `root` and collect up to [`MENTION_FILE_CAP`] repo-relative file paths
/// (`/`-separated, sorted) for the `@`-mention typeahead. Skips the noisy
/// build / VCS / hidden directories (`target`, `node_modules`, and anything
/// whose name starts with `.` — which covers `.git`, `.umadev`, dotfiles).
/// Fail-open: an unreadable directory is skipped (never panics); a missing root
/// yields an empty list. Uses an explicit work stack so a deep tree can't
/// overflow the call stack, and `DirEntry::file_type` (no symlink follow) so a
/// symlink loop can't make the walk diverge.
fn collect_repo_files(root: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if out.len() >= MENTION_FILE_CAP {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // unreadable dir → skip (fail-open)
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if depth < MENTION_SCAN_DEPTH {
                    stack.push((entry.path(), depth + 1));
                }
            } else if ft.is_file() {
                if let Ok(rel) = entry.path().strip_prefix(root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                    if out.len() >= MENTION_FILE_CAP {
                        break;
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// True for a RECOVERABLE, mid-turn base hiccup (rate-limit / overloaded / retry)
/// that should surface as a transient live status line rather than a permanent
/// `[warn]` transcript row — so a flurry of retries doesn't spam the region next
/// to the still-running thinking timer and read like the turn is erroring (it
/// isn't; the turn keeps running, only a terminal ABORT settles it).
fn is_transient_warning(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("rate limit")
        || m.contains("rate-limit")
        || m.contains("429")
        || m.contains("529")
        || m.contains("overloaded")
        || m.contains("retry")
        || m.contains("retrying")
        || m.contains("too many requests")
        || m.contains("capacity")
        || m.contains("temporarily")
        || m.contains("server is busy")
}

/// True when `s` ends with a recognised raster-image extension (case-insensitive).
/// The set mirrors what the bases accept as an image (`png` / `jpe?g` / `gif` /
/// `webp`); a dragged-in image arrives as exactly such a path.
fn is_image_path(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|ext| l.ends_with(ext))
}

/// Normalise a pasted path token the way a terminal mangles a dragged file:
/// drop a `file://` scheme, strip one layer of matching outer quotes, and undo
/// shell backslash-escapes (`my\ pic.png` → `my pic.png`). Leaves a plain path
/// untouched.
fn unquote_unescape(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("file://").unwrap_or(s);
    let bytes = s.as_bytes();
    let s = if s.len() >= 2
        && ((bytes[0] == b'"' && bytes[s.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[s.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    };
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The payload of one chat row — free text (rendered via the shared
/// `markdown_to_lines` compiler), a structured tool call (a status line +
/// folded result), or a structured file diff (a diff card). Keeping these as a
/// typed enum is the P0 data-model foundation the tool-row beautification (P4),
/// the long-output folding (P6), and the diff card (P1) build on; everything
/// else stays plain `Text`, so the upgrade is backward-compatible by
/// construction.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum MessageBody {
    /// A free-text body (already cleaned of ANSI etc.). Goes through the shared
    /// markdown renderer (`markdown_to_lines`) on the assistant/host path.
    Text(String),
    /// A structured tool call — its own status line + foldable result.
    Tool(ToolCall),
    /// A structured file diff — a Write/Edit rendered as a diff card.
    Diff(FileDiff),
}

impl MessageBody {
    /// Borrow the flat text of this body — the `Text` string verbatim, or a
    /// deterministic one-line rendering of a `Tool` call. Used by every
    /// non-render consumer (history export, the resume preview, the brain
    /// transcript) so they keep working unchanged after the enum upgrade.
    #[must_use]
    pub fn as_text(&self) -> std::borrow::Cow<'_, str> {
        match self {
            MessageBody::Text(s) => std::borrow::Cow::Borrowed(s),
            MessageBody::Tool(t) => {
                // The flat text (export / brain transcript / history preview)
                // keeps the legacy bracket-tag look (`[read] Read`), with a
                // terminal-state mark so a failed call still reads as failed.
                let mark = match t.status {
                    ToolStatus::Queued | ToolStatus::Running => tool_tag(&t.name),
                    ToolStatus::Ok => "[ok]",
                    ToolStatus::Fail => "[fail]",
                    ToolStatus::Aborted => "[aborted]",
                };
                let count = if t.count > 1 {
                    format!(" ({})", t.count)
                } else {
                    String::new()
                };
                let arg = if t.arg.is_empty() {
                    String::new()
                } else {
                    format!(" {}", t.arg)
                };
                let result = t
                    .result
                    .as_deref()
                    .filter(|r| !r.trim().is_empty())
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default();
                std::borrow::Cow::Owned(format!("{mark} {}{count}{arg}{result}", t.name))
            }
            MessageBody::Diff(d) => {
                // Flat text (export / brain transcript / history preview): a
                // compact one-liner so the diff card never leaks raw +/- noise
                // into the brain transcript or the resume preview.
                std::borrow::Cow::Owned(format!("[edit] {} (+{} -{})", d.path, d.added, d.removed))
            }
        }
    }
}

/// One row in the chat history.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ChatMessage {
    /// Who "said" this.
    pub role: ChatRole,
    /// The structured payload — free text or a tool call.
    pub kind: MessageBody,
    /// Whether a long body (text or tool result) is currently folded to a
    /// summary line. Toggled by the focus + expand key (P6); defaults to
    /// `false` (everything starts expanded; the renderer applies the head-N
    /// truncation only when this is set).
    pub collapsed: bool,
}

impl ChatMessage {
    /// Borrow the flat text of this message (see [`MessageBody::as_text`]).
    /// Lets the many `msg.body`-style read sites become `msg.body()` with no
    /// behavioural change.
    #[must_use]
    pub fn body(&self) -> std::borrow::Cow<'_, str> {
        self.kind.as_text()
    }

    /// Mutable handle to the underlying text when this row is a `Text` body —
    /// the streaming typewriter path appends deltas through this. Returns
    /// `None` for a `Tool` row (tool rows are updated structurally, not by
    /// string append), keeping the streaming append fail-open.
    pub fn text_mut(&mut self) -> Option<&mut String> {
        match &mut self.kind {
            MessageBody::Text(s) => Some(s),
            // Tool / Diff rows are updated structurally, not by string append.
            MessageBody::Tool(_) | MessageBody::Diff(_) => None,
        }
    }
}

/// A scrollable full-screen overlay opened by `/spec` / `/verify` /
/// `/doctor` / `/diff`. Closed with Esc.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Overlay {
    /// Window title shown at the top of the overlay border.
    pub title: String,
    /// Pre-split lines for easy clipping (each may be longer than the
    /// visible width; the renderer pre-folds them to VISUAL rows).
    pub lines: Vec<String>,
    /// Top-of-window cursor — counted in **visual rows** (a long logical line
    /// that wraps occupies several), so scrolling reaches every wrapped row and
    /// the progress % is honest. `0` = first visual row.
    pub scroll: usize,
    /// Greatest legal `scroll` (top-most reachable visual row), published every
    /// frame by the renderer once it knows the wrapped row count and viewport
    /// height. Key handlers clamp `scroll` against this so End / scroll_down land
    /// on the last visual row rather than a logical-line guess. `0` until first
    /// render (fail-open: an un-rendered overlay simply doesn't scroll).
    pub max_scroll: std::cell::Cell<usize>,
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
            max_scroll: std::cell::Cell::new(0),
        }
    }

    /// Scroll down by `n` visual rows, clamped to the last reachable row
    /// (published by the renderer in [`Self::max_scroll`]).
    pub fn scroll_down(&mut self, n: usize) {
        let max = self.max_scroll.get();
        self.scroll = (self.scroll + n).min(max);
    }

    /// Scroll up by `n` visual rows, clamped at 0.
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    /// Jump to the last reachable visual row (End / `G`).
    pub fn scroll_to_end(&mut self) {
        self.scroll = self.max_scroll.get();
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
    /// Image attachments for the turn being composed. A dragged/pasted image path
    /// is stored here (absolute, verified) and shown in `input` as an `[图片 N]`
    /// chip (N = 1-based index into this Vec); on submit the chip is rewritten to
    /// an `@<abs-path>` mention the base ingests as an image. Cleared with the input.
    pub attachments: Vec<std::path::PathBuf>,
    /// Past submitted texts. ↑↓ in an empty input box recalls them.
    pub input_history: VecDeque<String>,
    /// Recall cursor into `input_history`; `None` = editing a fresh draft.
    pub input_history_idx: Option<usize>,
    /// The in-progress draft stashed when history recall BEGINS (the
    /// `None → Some(idx)` transition), so stepping forward past the newest
    /// entry restores what the user was typing instead of clearing it.
    pub input_history_draft: Option<String>,
    /// When `input` starts with `/` and matches command verbs, this is
    /// the highlight in the slash-command palette popover.
    pub palette_selected: usize,
    /// Highlight index in the `@`-file-mention typeahead popover (parallel to
    /// [`Self::palette_selected`]). Always clamped against the live match count.
    pub mention_selected: usize,
    /// `true` once the user pressed Esc to dismiss the open `@`-mention popover;
    /// it stays closed until the next input edit re-opens it. Lets Esc close the
    /// popover WITHOUT mutating the prompt text.
    pub mention_dismissed: bool,
    /// Lazily-built, cached list of repo-relative file paths (`/`-separated,
    /// sorted) — the `@`-mention candidate source. `None` until the first
    /// `@`-token is typed, then built once by [`Self::ensure_mention_files`] and
    /// reused (the filesystem scan is NOT re-run per keystroke). Interior-mutable
    /// so the pure `&App` renderer can populate it on first use.
    pub mention_files: std::cell::RefCell<Option<Vec<String>>>,

    /// Bounded scrolling chat history (older lines roll off).
    pub history: VecDeque<ChatMessage>,

    /// **Transcript scrollback** — how many wrapped rows the user has scrolled
    /// UP from the bottom. `0` = pinned to the bottom (the default, auto-sticky
    /// as new content arrives). Any positive value means the user is reviewing
    /// history, so the renderer STOPS auto-sticking to the bottom until they
    /// return (End / scroll back down to 0). Clamped each frame against
    /// [`Self::transcript_max_scroll`].
    ///
    /// **Interior-mutable (P5b)** so the pure `&App` renderer can *re-anchor* it
    /// when the user is scrolled up and new content lands below: without that the
    /// from-bottom offset would let fresh rows push the content the user is
    /// reading upward out of view (a partial auto-follow). When scrolled up, the
    /// renderer bumps this by exactly the number of rows that appeared below so
    /// the same rows stay put ("release the pin, hold the anchor"); at offset `0`
    /// the view stays pinned to the bottom and follows new tokens (sticky). Key
    /// handlers read/write it through [`Self::transcript_scroll`] /
    /// [`Self::set_transcript_scroll`].
    pub transcript_scroll: std::cell::Cell<usize>,
    /// Previous frame's `hidden_above` (rows hidden above the viewport), published
    /// by the renderer so the next frame can tell how many rows appeared below and
    /// re-anchor a scrolled-up view (P5b). `0` until the first render.
    pub transcript_prev_hidden: std::cell::Cell<usize>,
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
    /// Display columns available to the input TEXT (after the `>_ ` mode prefix),
    /// published by the renderer every frame. The Up/Down key handlers use it to
    /// move the caret by one *wrapped* visual row inside a multi-line / wrapped
    /// prompt (Claude Code parity) before falling through to history recall. `0`
    /// until the first render — the handlers treat that as "no wrap info yet" and
    /// fall back to the plain history-recall behavior.
    pub input_text_cols: std::cell::Cell<u16>,
    /// `true` when the mouse-wheel → transcript-scroll binding is active (and,
    /// with it, the in-app drag-to-select/copy layer). **Default ON**: mouse
    /// capture is enabled at startup so the wheel pages the transcript AND a
    /// left-drag selects text that we copy ourselves via OSC 52 — the Claude
    /// Code approach to getting BOTH on the alternate screen (which has no
    /// native scrollback). A `/mouse` toggle flips it OFF, which issues
    /// `DisableMouseCapture` and hands selection back to the terminal's native
    /// click-drag for users who prefer it. Read by the event loop; the value
    /// lives here so it survives across redraws.
    pub mouse_scroll: bool,

    /// **In-app text selection** over the transcript (the Claude-Code layer).
    /// `Some` while the user has a live / just-copied drag selection; `None`
    /// when there's nothing selected. Coordinates are `(content_row, col)` into
    /// [`Self::transcript_rows`]. Set on mouse-down, extended on drag, kept on
    /// mouse-up (so the copied span stays highlighted) and cleared on the next
    /// down outside the selection. See [`crate::selection`].
    pub selection: Option<crate::selection::Selection>,

    /// **Per-frame cache of the rendered transcript as plain text** — one
    /// `String` per wrapped visual row, in render order. Rebuilt every frame by
    /// `ui::render_transcript` from the same folded `Vec<Line>` it paints. The
    /// cache holds the LOGICAL row text only: the leading role-spine / hang-indent
    /// gutter is stripped and the trailing user-bubble background padding is
    /// trimmed, so a drag-copy yields clean content (no `▎` glyphs, no leading
    /// indent, no runs of trailing spaces). The dropped gutter width is recorded
    /// per row in [`Self::transcript_gutters`] so a screen column still maps to the
    /// right logical char index. This is what [`crate::selection::extract`] reads
    /// the copied text out of. In a `RefCell` because the renderer borrows `&App`
    /// (it can't take `&mut`).
    pub transcript_rows: std::cell::RefCell<Vec<String>>,

    /// **Per-row leading-gutter width** (display columns) stripped from each
    /// cached row in [`Self::transcript_rows`] — the role-spine glyph + hang
    /// indent that decorates the painted line but must NOT be copied. The mouse
    /// mapping subtracts it (a click in the gutter resolves to column 0 of the
    /// logical text) and the highlight adds it back (it paints the DECORATED line,
    /// whose char indices are shifted right by the gutter). One entry per row, in
    /// lockstep with `transcript_rows`. In a `RefCell` for the same borrow reason.
    pub transcript_gutters: std::cell::RefCell<Vec<usize>>,

    /// The transcript rectangle `(left, top, width, height)` from the last
    /// frame, published by `ui::render_transcript`. The event loop maps a mouse
    /// `(col, row)` against this to decide inside/outside the transcript and to
    /// compute the content row. `(0,0,0,0)` until the first render.
    pub transcript_area: std::cell::Cell<(u16, u16, u16, u16)>,

    /// Index into [`Self::transcript_rows`] of the row currently painted at the
    /// top of the transcript area (`hidden_above - user_offset`, the renderer's
    /// effective top scroll offset). Combined with [`Self::transcript_area`] this
    /// turns a screen row into a content row. Published every frame.
    pub transcript_first_visible: std::cell::Cell<usize>,

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

    /// One-shot signal that the **resident chat session** (the host-CLI base process
    /// the event loop keeps alive across the conversation — `lib.rs`'s
    /// `chat_session_holder`) must be CLOSED before the next turn: the conversation
    /// context broke (`/clear`) or the driving base changed (a `/backend` switch), so
    /// the live session — built against the OLD context / base — is no longer valid.
    /// Set here, consumed (drained) by the event loop, which `end()`s and clears the
    /// holder so the next chat message opens a FRESH resident session. Fail-open: a
    /// missed signal at worst keeps a stale-but-harmless session one extra turn.
    pub(crate) chat_session_dirty: bool,

    /// **Persistent chat id** (Wave 5 / G11) — the stable id of the on-disk
    /// `.umadev/chat/<id>.json` that mirrors [`Self::conversation`] so a restart
    /// reopens the same dialogue instead of amnesia. Distinct from
    /// [`Self::chat_session_id`] (the BASE session pin, which the base may rotate):
    /// this id names the SAVED transcript and survives `/clear` re-mints. Minted at
    /// construction; `/resume <id>` swaps it to an existing saved chat; `/clear`
    /// starts a fresh one. Fail-open: every persist/load is best-effort — a write
    /// failure or corrupt file degrades to the live in-memory buffer, never a crash.
    pub(crate) chat_id: String,

    /// `true` once the run loop has handed a finished `/run` director session back
    /// to chat (Wave 5 deliverable 2). The NEXT chat turn then resumes the base's
    /// most-recent session in this dir (`--continue`, i.e. `session_id = None` +
    /// `continue_session = true`) so "why did you build it that way?" continues the
    /// SAME session that did the build — not a disjoint cold one. Consumed (reset)
    /// once that first post-run chat turn fires. Fail-open: if the resume misses,
    /// the base starts fresh (today's behaviour).
    pub(crate) run_session_handed_to_chat: bool,

    /// `true` while an explicit `/run` **director build** is the in-flight agentic
    /// turn (Wave 5 deliverable 2). Set when the `/run` director loop is launched and
    /// cleared on any terminal turn.
    ///
    /// It NO LONGER decides the Wave-5 session hand-back: the chat surface classifies
    /// chat-vs-build INSIDE the spawned task (after the slow brain-router consult), so
    /// the event loop can't know the class before dispatch and can't set this flag
    /// truthfully pre-spawn. The build-ness now rides the terminal
    /// [`crate::RouteDecision::AgenticDone`]'s `director_build` field and is what
    /// [`Self::record_agentic_done`] keys the hand-back on. This flag is retained only
    /// as the explicit-`/run` in-flight marker.
    pub(crate) director_run_in_flight: bool,

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
    /// `true` while a cancelled run/turn's aborted task is winding down (the
    /// base subprocess is being killed + its session dropped). Set the instant
    /// Esc/Ctrl-C fires so the loop keeps redrawing a live "stopping…" state —
    /// the abort-drain runs OFF the render path, so the UI never freezes waiting
    /// for it. Cleared by the loop once the drain completes and `cancel_run`
    /// resets the rest of the state.
    pub cancelling: bool,

    /// **P5c — reasoning-block collapse.** When the base emits a `Thinking`
    /// stream event we push ONE placeholder System line (live spinner) and stamp
    /// the moment here; the moment the next real content (text / tool) arrives we
    /// rewrite that line in place to a one-line summary `思考 · 4.2s` instead of
    /// leaving a stack of orphan `[thinking]` rows. `None` when no reasoning block
    /// is open. Fail-open: a missing timestamp collapses to a plain "思考完成".
    pub(crate) thinking_block_start: Option<std::time::Instant>,
    /// History index of the live `Thinking` placeholder row to rewrite on
    /// collapse (P5c). `None` when no reasoning block is open. Re-validated
    /// against the row's content before rewrite so a rolled-off / shifted index
    /// can never clobber an unrelated row (fail-open).
    pub(crate) thinking_block_idx: Option<usize>,
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

    /// Process-local cache of the trust tier *derived from `.umadevrc`* (used
    /// only when no session override is set). [`effective_trust_mode`] runs in
    /// the render hot path (~12/s at the 80 ms tick); without this it would
    /// `load_project_config` — i.e. read `.umadevrc` off disk — on every frame,
    /// which stutters on a slow / network-mounted workspace. We read the config
    /// once, memoise the result here, and only refresh when the config could
    /// actually have changed (a `/mode` switch or an explicit reload). Interior
    /// mutability keeps `effective_trust_mode` a `&self` reader. Fail-open: a
    /// config read error resolves to `Guarded`, same as before.
    config_trust_cache: std::cell::Cell<Option<umadev_agent::TrustMode>>,

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

    /// First-Esc-to-arm, second-Esc-to-interrupt while a run is in flight, so a
    /// stray keypress can't nuke a long build. Set on the first Esc; a second Esc
    /// within a short window actually cancels. `None` = not armed.
    pub interrupt_armed_at: Option<std::time::Instant>,

    /// REAL token usage for THIS session (input+output), accumulated from the
    /// base's own per-turn reports (`EngineEvent::TurnUsage`, F3) — true
    /// consumption, the base's numbers, NOT an estimate or the all-time ledger.
    /// Starts at 0 each launch and grows per turn; shown on the waiting indicator.
    pub session_tokens: u64,

    /// One-line status shown in the top bar.
    pub status: String,
    /// Spinner animation tick.
    pub tick: u8,
    /// **P5d — animation master switch.** `true` (default) animates every spinner
    /// surface through the shared braille frames; `false` (accessibility / a
    /// non-TTY render target / `/animations off`) renders a single static glyph
    /// instead, so the UI never strobes. Read once at construction from
    /// `~/.umadev/settings.json` (`animations_enabled`) AND the real-stdout TTY
    /// probe, and flipped live by `/animations`. Fail-open: an unreadable setting
    /// or probe defaults to animated (today's behaviour).
    pub animations: bool,
    /// Global "expand everything" toggle (Ctrl+O). When `true`, EVERY collapsible
    /// renderer (tool results, diff cards, long replies) shows its full body at
    /// once — not just the most-recent row (which Ctrl+R folds). Flipping it back
    /// to `false` re-hides all of them to their per-row default. The single
    /// reveal-all gesture, so older collapsed output is never stranded with no
    /// way to expand it. Defaults to `false` (collapsed).
    pub verbose: bool,
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

    /// **P5a stable-prefix markdown cache** — interior-mutable so the pure
    /// `&App` renderer can reuse the closed-block render of the message
    /// currently being streamed, re-rendering only the unclosed tail per delta
    /// (kills the old O(n²) re-parse of the whole body each frame). Reset when a
    /// streamed turn settles ([`Self::record_agentic_done`] /
    /// [`Self::record_route_failed`]) or the context breaks (`/clear`), so the
    /// next frame falls back to a clean whole-body render. Fully fail-open: any
    /// precondition miss inside [`crate::ui::stream_markdown_lines`] discards the
    /// cache and renders the whole body (the prior behaviour).
    pub(crate) stream_md_cache: std::cell::RefCell<crate::ui::StreamMarkdownCache>,

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
    /// **Adaptive stall threshold** (Wave 6) — `true` while the in-flight tool
    /// call is a known *legitimately-long* operation (a dependency install or a
    /// full build/compile: `npm install`, `cargo build`, `pip install`, …). These
    /// routinely run for minutes with no intervening output, so the stall clock
    /// uses a much longer threshold while this is set — the red "about to hang"
    /// cue must not false-fire during a legitimate `npm install`. Set on the
    /// matching `ToolUse`, cleared on its `ToolResult` (alongside
    /// [`Self::tool_in_progress`]).
    pub long_op_in_progress: bool,

    /// **Transient heartbeat status** — a single in-place line for the
    /// long-phase heartbeat's periodic "still working (mm:ss)" beats. Set by
    /// [`EngineEvent::TransientStatus`] (overwritten each beat, NOT appended to
    /// the transcript) and cleared (`None`) when the slow op finishes or any
    /// real progress arrives (phase boundary, host output, worker stream). This
    /// is what stops the heartbeat from flooding the chat with a new row every
    /// few seconds — the status bar shows ONE live-updating reassurance instead.
    /// Never enters the scrollback history.
    pub transient_status: Option<String>,

    /// **Live plan checklist** (Wave 1 deliverable 2/3) — the UmaDev-owned plan,
    /// populated by [`EngineEvent::PlanPosted`] and updated in place by
    /// [`EngineEvent::PlanStepStatus`]. Rendered as a ticking checklist panel
    /// above the prompt (replacing the frozen 0/9 dot bar on the director path).
    /// Empty when no plan is live. Fail-open: a status for an unknown id is
    /// ignored rather than panicking.
    pub plan_steps: Vec<PlanStepRow>,
    /// `true` collapses the live plan checklist panel to a one-line summary
    /// (toggled by `/plan` with no args). Default expanded.
    pub plan_collapsed: bool,

    /// **Team-review verdicts** (Wave 1 deliverable 3) — each reviewing seat's
    /// structured verdict, pushed by [`EngineEvent::CriticVerdict`]. Rendered as
    /// a collapsible team-review panel. Bounded to the latest review round (a
    /// repeated seat id replaces its prior row so a re-review doesn't stack).
    pub critic_verdicts: Vec<CriticRow>,
    /// `true` collapses the team-review panel to a one-line accept/block tally.
    /// Default expanded so the first review is visible.
    pub critics_collapsed: bool,
    /// `true` while a review burst is OPEN — the seats of the *current* round are
    /// still arriving (one [`EngineEvent::CriticVerdict`] per seat, contiguous).
    /// A plan-step transition / phase start / fresh plan post **seals** it
    /// (`false`), so the NEXT verdict that arrives is recognised as a new round
    /// and clears the previous round's rows before appending — the panel shows
    /// the CURRENT round, never a stale mix. Default `false` (no round open yet).
    pub critic_round_open: bool,

    /// The last routed intent (Wave 1 deliverable 1) — the class id the router
    /// decided for the in-flight turn (`chat` / `build` / …). Drives the status
    /// chip so the user sees fast-vs-deliberate at a glance. `None` until the
    /// first route. Set deterministically (Tier-0) the instant a turn is
    /// submitted, then refined by the async Tier-1 consult.
    pub last_intent_class: Option<String>,
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
        // Publish the project's Codex launch-sandbox choice (`.umadevrc`
        // `[codex] sandbox_mode`) into `UMADEV_CODEX_SANDBOX` so the codex driver
        // honors it, mirroring the model-tier export above. Default stays the safe
        // `workspace-write`; an env already set (advanced / CI) wins.
        let codex_sandbox = resolve_and_publish_codex_sandbox(&project_root);
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
            attachments: Vec::new(),
            input_history: VecDeque::new(),
            input_history_idx: None,
            input_history_draft: None,
            palette_selected: 0,
            mention_selected: 0,
            mention_dismissed: false,
            mention_files: std::cell::RefCell::new(None),
            history: VecDeque::new(),
            transcript_scroll: std::cell::Cell::new(0),
            transcript_prev_hidden: std::cell::Cell::new(0),
            transcript_max_scroll: std::cell::Cell::new(0),
            transcript_viewport_rows: std::cell::Cell::new(0),
            input_text_cols: std::cell::Cell::new(0),
            // ON by default: mouse capture is enabled at startup so the wheel pages
            // the transcript AND a left-drag selects text that we copy ourselves via
            // OSC 52 (the in-app selection layer below) — the Claude Code approach to
            // getting BOTH wheel-scroll and drag-copy on the alternate screen (no
            // native scrollback). `/mouse` toggles capture OFF for users who prefer
            // the terminal's native click-drag selection.
            mouse_scroll: true,
            selection: None,
            transcript_rows: std::cell::RefCell::new(Vec::new()),
            transcript_gutters: std::cell::RefCell::new(Vec::new()),
            transcript_area: std::cell::Cell::new((0, 0, 0, 0)),
            transcript_first_visible: std::cell::Cell::new(0),
            conversation: Vec::new(),
            host_chat_session_active: false,
            chat_session_id: None,
            chat_session_dirty: false,
            // A fresh persistent-chat id; `load_chat_for_launch` below may replace
            // it with the most-recent saved chat so a restart reopens the dialogue.
            chat_id: new_chat_session_id(),
            run_session_handed_to_chat: false,
            director_run_in_flight: false,
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
            cancelling: false,
            thinking_block_start: None,
            thinking_block_idx: None,
            agentic_in_flight: false,
            auto_approve_override: None,
            trust_mode_override: None,
            config_trust_cache: std::cell::Cell::new(None),
            trust_ledger: umadev_agent::TrustLedger::load(&project_root),
            backends: Vec::new(),
            show_help: false,
            help_scroll: 0,
            overlay: None,
            preview_server: std::sync::Arc::new(std::sync::Mutex::new(None)),
            project_root,
            pending_quit_confirm: false,
            interrupt_armed_at: None,
            session_tokens: 0,
            status: String::new(),
            tick: 0,
            animations: animations_enabled_default(),
            verbose: false,
            should_quit: false,
            run_started_at: None,
            phase_started_at: None,
            pending_auto_continue: None,
            queued_steer: VecDeque::new(),
            pending_steer: None,
            queued_chat: std::collections::VecDeque::new(),
            stream_tool_batch: None,
            stream_text_active: false,
            stream_md_cache: std::cell::RefCell::new(crate::ui::StreamMarkdownCache::default()),
            last_output_at: None,
            tool_in_progress: false,
            long_op_in_progress: false,
            transient_status: None,
            plan_steps: Vec::new(),
            plan_collapsed: false,
            critic_verdicts: Vec::new(),
            critics_collapsed: false,
            critic_round_open: false,
            last_intent_class: None,
        };
        app.load_history();
        if app.mode == AppMode::Chat {
            app.push_greeting();
            // Wave 5 / G11: reopen the most-recent saved chat so a restart keeps
            // the conversation instead of amnesia. Fail-open: no saved chat (or a
            // corrupt one) leaves the fresh empty buffer + freshly-minted id.
            app.load_chat_for_launch();
            app.maybe_push_resume_hint();
            app.maybe_push_goal_continuity();
            // Liability notice: if codex is the active base AND the high-risk
            // `danger-full-access` sandbox is resolved, push a loud red warning
            // last so it is the most-visible startup line.
            app.maybe_warn_codex_sandbox(codex_sandbox);
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
        // UmaDev manages NO model: it forwards nothing to the base, so the base
        // ALWAYS runs whatever it is configured / logged in with (an official
        // subscription OR a third-party / local-model routing the user set up in
        // the base itself) — UmaDev just calls it. Returning empty makes every
        // session start omit the `model` field; `detect_base_model` is still used
        // purely to SHOW the user which model their base runs (never to impose one).
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

    /// Directory holding this project's persisted chats (Wave 5 / G11):
    /// `.umadev/chat/`. One `<id>.json` per saved chat so a restart can reopen
    /// the dialogue and `/sessions` can list them.
    fn chat_dir(&self) -> std::path::PathBuf {
        self.project_root.join(".umadev").join("chat")
    }

    /// The on-disk path for a chat by id: `.umadev/chat/<id>.json`.
    fn chat_path(&self, id: &str) -> std::path::PathBuf {
        self.chat_dir().join(format!("{id}.json"))
    }

    /// Persist the live conversation to `.umadev/chat/<chat_id>.json`, **atomically**
    /// (write a temp sibling, then rename — the same temp-then-rename pattern
    /// `config::save_to` uses, so a crash mid-write never corrupts the saved chat).
    ///
    /// Best-effort + **fail-open**: an empty conversation writes nothing (no empty
    /// file litter), and ANY IO / serialise error is swallowed — a failed persist
    /// must never block a chat turn or crash the TUI. Called after every recorded
    /// turn (user + assistant) so the saved transcript tracks the live one.
    pub(crate) fn persist_chat(&self) {
        if self.conversation.is_empty() {
            return;
        }
        let session = ChatSession {
            id: self.chat_id.clone(),
            updated_at: now_iso8601(),
            backend: self.backend.clone().unwrap_or_default(),
            // Persist the base's OWN resumable session id (the LIVE one captured back
            // off each host chat turn — see `record_agentic_done`) so a relaunch can
            // resume the base's deep context, not just replay the transcript. `None`
            // (opencode / offline / no host turn yet) writes `null` — fail-open.
            base_session_id: self.chat_session_id.clone(),
            messages: self.conversation.clone(),
        };
        let Ok(body) = serde_json::to_string_pretty(&session) else {
            return;
        };
        let dir = self.chat_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let final_path = self.chat_path(&self.chat_id);
        // Temp sibling in the SAME dir so the rename is atomic on POSIX/Windows.
        let tmp = dir.join(format!("{}.json.tmp-{}", self.chat_id, std::process::id()));
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &final_path);
        }
    }

    /// List persisted chats for this project, most-recently-updated first (Wave 5).
    /// Returns `(id, updated_at, turn_count, preview)` tuples. Fail-open: a missing
    /// dir / unreadable / corrupt file yields an empty list (never an error).
    pub(crate) fn list_chats(&self) -> Vec<(String, String, usize, String)> {
        let mut out: Vec<(String, String, usize, String)> = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.chat_dir()) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(session) = serde_json::from_str::<ChatSession>(&text) else {
                continue;
            };
            // First user message as a short preview so the list is recognisable.
            let preview = session
                .messages
                .iter()
                .find(|m| m.role == "user")
                .map(|m| {
                    let line: String = m.content.split_whitespace().collect::<Vec<_>>().join(" ");
                    match line.char_indices().nth(48) {
                        Some((i, _)) => format!("{}…", &line[..i]),
                        None => line,
                    }
                })
                .unwrap_or_default();
            out.push((
                session.id,
                session.updated_at,
                session.messages.len(),
                preview,
            ));
        }
        // Most-recent first by the ISO-8601 timestamp (lexicographic == chronological).
        out.sort_by(|a, b| b.1.cmp(&a.1));
        out
    }

    /// Load a saved chat by id into the live buffer (Wave 5 / `/resume <id>`).
    /// Returns `true` on success. Fail-open: a missing / corrupt / empty file
    /// returns `false` and leaves the live conversation untouched.
    pub(crate) fn load_chat(&mut self, id: &str) -> bool {
        let Ok(text) = std::fs::read_to_string(self.chat_path(id)) else {
            return false;
        };
        let Ok(session) = serde_json::from_str::<ChatSession>(&text) else {
            return false;
        };
        if session.messages.is_empty() {
            return false;
        }
        self.conversation = session.messages;
        self.trim_conversation();
        self.chat_id = session.id;
        // Restore the base's OWN resumable session id so the NEXT host chat turn
        // RESUMES the base's deep context (the resident pre-load opens it via
        // `--resume <id>` / `thread/resume`) instead of cold-starting and replaying
        // only the ≤16-message transcript. Fail-open: an old chat file (or an
        // opencode / offline chat) has `None` here — leave `host_chat_session_active`
        // untouched so it degrades cleanly to today's fresh-session behavior.
        self.chat_session_id = session.base_session_id;
        if self.chat_session_id.is_some() {
            self.host_chat_session_active = true;
        }
        true
    }

    /// On launch (Chat mode), reopen the most-recently-updated saved chat so the
    /// dialogue survives a restart (Wave 5 / G11). Fail-open: no saved chat leaves
    /// the fresh empty buffer + freshly-minted [`Self::chat_id`]. Surfaces a short
    /// system note so the user knows prior context was restored.
    fn load_chat_for_launch(&mut self) {
        let Some((id, _, _, _)) = self.list_chats().into_iter().next() else {
            return;
        };
        if self.load_chat(&id) {
            let n = self.conversation.len();
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "chat.restored", &[&n.to_string()]),
            );
        }
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

    /// Wave 5 / G11 deliverable 4 — cross-session goal continuity. If a prior
    /// session left an unfinished plan (`.umadev/plan.json`, persisted by Wave 1),
    /// surface "resume goal X (step N/M)?" on launch so the user can pick the build
    /// back up with `/run` (or, in `auto` tier, be driven to completion). Read-only
    /// over the existing plan; fail-open: no plan / a finished plan / a corrupt file
    /// all push nothing.
    fn maybe_push_goal_continuity(&mut self) {
        let Some((next_step, done, total)) =
            umadev_agent::unfinished_plan_summary(&self.project_root)
        else {
            return;
        };
        self.push(
            ChatRole::System,
            umadev_i18n::tf(
                self.lang,
                "session.resume_goal",
                &[&next_step, &done.to_string(), &total.to_string()],
            ),
        );
    }

    /// Liability notice for the high-risk codex sandbox. Pushes a LOUD red, bold
    /// warning line ONLY when codex is the active base AND the resolved sandbox is
    /// `danger-full-access` (see [`should_warn_codex_sandbox`]); `read-only` /
    /// `workspace-write` stay silent. Trilingual via i18n.
    fn maybe_warn_codex_sandbox(&mut self, mode: umadev_agent::config::CodexSandbox) {
        if should_warn_codex_sandbox(self.backend.as_deref(), mode) {
            self.push(
                ChatRole::Error,
                umadev_i18n::t(self.lang, "codex.sandbox.danger_warning").to_string(),
            );
        }
    }

    fn push(&mut self, role: ChatRole, body: impl Into<String>) {
        self.history.push_back(ChatMessage {
            role,
            kind: MessageBody::Text(body.into()),
            collapsed: false,
        });
        while self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
    }

    /// Push a structured tool row (or merge into a running low-signal batch).
    ///
    /// P4 — the tool-call beautification. A low-signal read/grep/glob folds into
    /// the trailing merged batch row with an incrementing count (greatest-seen,
    /// so a streamed count never jumps backwards); Write / Edit / Bash / web /
    /// agent each push their own `Running` row. Fully fail-open: a corrupt
    /// trailing row simply means a fresh row is pushed.
    fn push_tool_use(&mut self, name: &str, detail: &str) {
        let lang = self.lang;
        let arg: String = detail.chars().take(80).collect();
        let low_signal = is_low_signal_tool(name);

        // Merge a contiguous low-signal run into one row. We only merge when the
        // trailing row is ALSO a (low-signal) tool row — the moment a Write /
        // Bash / text bubble lands, the next read starts a fresh batch.
        if low_signal {
            let mut merged_count: Option<u32> = None;
            if let Some(last) = self.history.back_mut() {
                if last.role == ChatRole::Host {
                    if let MessageBody::Tool(t) = &mut last.kind {
                        if t.merged {
                            // Greatest-seen guard: never let a streamed count
                            // visibly decrease.
                            t.count = t.count.saturating_add(1).max(t.count);
                            t.status = ToolStatus::Running;
                            t.collapsed = false;
                            t.arg = merged_batch_summary(lang, t.count);
                            // The result of the prior call in the batch is no
                            // longer the headline — the live count is.
                            t.result = None;
                            merged_count = Some(t.count);
                        }
                    }
                }
            }
            if let Some(count) = merged_count {
                self.stream_tool_batch = Some((name.to_string(), count));
                return;
            }
            // Start a fresh merged batch row.
            let count = 1;
            self.stream_tool_batch = Some((name.to_string(), count));
            let summary = merged_batch_summary(lang, count);
            self.history.push_back(ChatMessage {
                role: ChatRole::Host,
                kind: MessageBody::Tool(ToolCall {
                    name: name.to_string(),
                    arg: summary,
                    status: ToolStatus::Running,
                    result: None,
                    merged: true,
                    count,
                    collapsed: false,
                }),
                collapsed: false,
            });
            while self.history.len() > HISTORY_CAP {
                self.history.pop_front();
            }
            return;
        }

        // A single-row tool (Write / Edit / Bash / web / agent / other): always
        // its own `Running` row. Breaks any in-flight low-signal batch.
        self.stream_tool_batch = None;
        self.history.push_back(ChatMessage {
            role: ChatRole::Host,
            kind: MessageBody::Tool(ToolCall {
                name: name.to_string(),
                arg,
                status: ToolStatus::Running,
                result: None,
                merged: false,
                count: 1,
                collapsed: false,
            }),
            collapsed: false,
        });
        while self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
    }

    /// Push a structured diff card for a `Write`/`Edit` (P1). Breaks any
    /// in-flight low-signal merged batch (a write is a hard boundary) and appends
    /// a `MessageBody::Diff` row — rendered as a Claude-Code-style diff card the
    /// moment the tool call arrives, so the user sees code being written live.
    ///
    /// Fail-open: a no-op edit (identical before/after → zero hunks) degrades to
    /// the plain tool row instead of an empty card.
    fn push_diff(&mut self, edit: &umadev_runtime::ToolEdit) {
        let diff = FileDiff::from_tool_edit(edit);
        if diff.hunks.is_empty() {
            // Nothing actually changed (or unreadable) — keep the plain row so the
            // activity is still visible, never an empty card.
            let name = if edit.before.is_empty() {
                "Write"
            } else {
                "Edit"
            };
            self.push_tool_use(name, &edit.path);
            return;
        }
        self.stream_tool_batch = None;
        self.history.push_back(ChatMessage {
            role: ChatRole::Host,
            kind: MessageBody::Diff(diff),
            collapsed: false,
        });
        while self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
    }

    /// Fold a `ToolResult` into the trailing tool row: flip its status to
    /// Ok/Fail, attach the result summary, and auto-collapse a finished OK call
    /// (a failed call stays expanded so the error is never hidden). A low-signal
    /// merged row keeps its `read N files` summary as the headline and folds the
    /// metric in rather than dumping the raw output. Fail-open: with no trailing
    /// tool row (e.g. a result with no preceding use) it pushes a bare line.
    fn attach_tool_result(&mut self, ok: bool, summary: &str) {
        let lang = self.lang;
        let status = if ok { ToolStatus::Ok } else { ToolStatus::Fail };
        // Update the trailing tool row, then carry whether it was a merged batch
        // out of the borrow so the (separate) `stream_tool_batch` field can be
        // set without overlapping the `&mut self.history` borrow.
        let mut batch: Option<(String, u32)> = None;
        let mut handled = false;
        if let Some(last) = self.history.back_mut() {
            if last.role == ChatRole::Host {
                if let MessageBody::Tool(t) = &mut last.kind {
                    t.status = status;
                    let preview: String = summary.chars().take(200).collect();
                    if t.merged {
                        // The headline stays the running count; only fold the
                        // metric in (e.g. `(3 matches)`), never the raw dump.
                        t.result = read_only_metric(lang, &t.name, &preview);
                        batch = Some((t.name.clone(), t.count));
                    } else if let Some(metric) = read_only_metric(lang, &t.name, &preview) {
                        // A read-only tool (Grep/Glob) gets a clean metric line
                        // (`3 matches`) instead of its raw output dump.
                        t.result = Some(metric);
                    } else if ok
                        && matches!(
                            t.name.as_str(),
                            "Read" | "NotebookRead" | "LS" | "Glob" | "Grep" | "WebFetch"
                        )
                    {
                        // A successful read: the tool row already names the target
                        // (the arg), so the raw file/content dump is pure noise —
                        // suppress it, matching the clean reference look.
                        t.result = None;
                    } else {
                        t.result = if preview.trim().is_empty() {
                            None
                        } else {
                            Some(preview)
                        };
                    }
                    // Auto-collapse a finished OK call; a failure stays open.
                    t.collapsed = ok;
                    handled = true;
                }
            }
        }
        if handled {
            self.stream_tool_batch = batch;
            return;
        }
        // A diff card was just pushed for this Write/Edit — its result ("File
        // written") is implied by the card itself, so absorb a SUCCESS silently
        // (no redundant `[ok]` line). A FAILURE still surfaces below so a failed
        // write is never hidden.
        if ok {
            if let Some(last) = self.history.back() {
                if matches!(last.kind, MessageBody::Diff(_)) {
                    return;
                }
            }
        }
        // No trailing tool row — fail-open to a plain status line (old look).
        let mark = if ok { "[ok]" } else { "[fail]" };
        let preview: String = summary.chars().take(100).collect();
        if !preview.trim().is_empty() {
            self.push(ChatRole::Host, format!("  {mark} {preview}"));
        }
    }

    /// Toggle the fold state of the most recent collapsible row (Ctrl+R — the
    /// secondary "fold just the latest" gesture; the global reveal-all is Ctrl+O
    /// / [`verbose`](Self::verbose)). Walks from newest to oldest and flips the
    /// `collapsed` flag of the first row long enough to be foldable (a long
    /// Host/UmaDev text body, or a finished tool row whose result is long). No-op
    /// (fail-open) when nothing in view is long enough to fold.
    fn toggle_last_collapsible(&mut self) {
        if let Some(msg) = self
            .history
            .iter_mut()
            .rev()
            .find(|m| message_is_collapsible(m))
        {
            match &mut msg.kind {
                // A long tool result lives on the ToolCall's own `collapsed`.
                MessageBody::Tool(t) => t.collapsed = !t.collapsed,
                // A big diff card folds on its own `collapsed` flag.
                MessageBody::Diff(d) => d.collapsed = !d.collapsed,
                // A long text body uses the message-level `collapsed`.
                MessageBody::Text(_) => msg.collapsed = !msg.collapsed,
            }
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
        // The 9-phase dot bar is the LEGACY pipeline's progress view. The
        // brain-routing / chat / plan path never advances these phases (its progress
        // is the live plan checklist), so an all-Pending `0/N` bar is a frozen
        // vestige in the header. Show the dots ONLY once the legacy pipeline actually
        // moves a phase (done > 0 or one is running); otherwise omit them entirely so
        // a chat / brain-routed turn shows a clean header, not a stuck `0/9`. The
        // separator travels WITH the dots so an empty bar leaves no dangling " · ".
        let any_running = self.phases.iter().any(|r| r.status == PhaseStatus::Running);
        let dots = if total > 0 && (done > 0 || any_running) {
            format!(" · {bar} {done}/{total}")
        } else {
            String::new()
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
            format!(
                " · [aborted] {}",
                umadev_i18n::t(self.lang, "status.aborted")
            )
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
            "● {}{}{}{}{}{}{}",
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

    /// `true` when the interrupt is ARMED (a first Esc landed recently) — a second
    /// Esc within the window cancels the run. The window auto-expires so a stray
    /// single Esc just shows the hint briefly and is forgotten.
    #[must_use]
    pub fn interrupt_armed(&self) -> bool {
        self.interrupt_armed_at
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3))
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
        self.long_op_in_progress = false;
        self.last_output_at = None;
        // No live phase → no heartbeat reassurance should remain.
        self.transient_status = None;
        // A bailed round is terminal — drop the live plan / team-review panel so
        // its last (now stale) state doesn't hang under the transcript.
        self.clear_live_panels();
        self.push(ChatRole::System, body);
        self.refresh_status();
    }

    // ---- transcript scrollback -------------------------------------------
    //
    // `transcript_scroll` is the number of wrapped rows the user has scrolled
    // UP from the bottom (0 = pinned to bottom). The renderer publishes the
    // current upper bound into `transcript_max_scroll` every frame, so these
    // helpers clamp against the real, width-aware overflow instead of guessing.

    /// Current transcript scroll offset (rows scrolled UP from the bottom; `0` =
    /// pinned to the bottom). Reads the interior-mutable cell (P5b).
    #[must_use]
    pub fn transcript_scroll(&self) -> usize {
        self.transcript_scroll.get()
    }

    /// Set the transcript scroll offset directly (rows from the bottom).
    pub fn set_transcript_scroll(&self, rows: usize) {
        self.transcript_scroll.set(rows);
    }

    /// Scroll the transcript UP by `rows` (toward older history). Any non-zero
    /// scroll makes the renderer STOP auto-sticking to the bottom.
    pub fn transcript_scroll_up(&mut self, rows: usize) {
        let max = self.transcript_max_scroll.get();
        self.transcript_scroll
            .set(self.transcript_scroll.get().saturating_add(rows).min(max));
    }

    /// Scroll the transcript DOWN by `rows` (toward the newest content). Hitting
    /// `0` re-pins to the bottom and re-enables auto-stick.
    pub fn transcript_scroll_down(&mut self, rows: usize) {
        self.transcript_scroll
            .set(self.transcript_scroll.get().saturating_sub(rows));
    }

    /// Jump to the very top of the transcript (oldest content on screen).
    pub fn transcript_scroll_to_top(&mut self) {
        self.transcript_scroll.set(self.transcript_max_scroll.get());
    }

    /// Jump back to the bottom (newest content) and re-enable auto-stick.
    pub fn transcript_scroll_to_bottom(&mut self) {
        self.transcript_scroll.set(0);
    }

    /// Route one mouse-wheel notch (`up` = `ScrollUp`) to the right surface.
    ///
    /// Precedence: a modal **overlay**, when open, owns the viewport and scrolls
    /// regardless of the `/mouse` wheel-capture toggle — it is content the user is
    /// actively reading, so the wheel must move IT, not the transcript hidden
    /// behind it (the reported "overlay won't scroll" was the wheel scrolling that
    /// hidden transcript). With no overlay open, the wheel scrolls the chat
    /// transcript, but only when wheel-capture is enabled (`/mouse`) and we're on
    /// the chat screen, matching the existing chat-mode gating. Returns `true` if
    /// the notch was consumed. Fail-open: an out-of-range notch is clamped by the
    /// underlying scroll helpers, never panics.
    pub fn mouse_wheel(&mut self, up: bool, step: usize) -> bool {
        if let Some(ov) = self.overlay.as_mut() {
            if up {
                ov.scroll_up(step);
            } else {
                ov.scroll_down(step);
            }
            return true;
        }
        if self.mouse_scroll && matches!(self.mode, AppMode::Chat) {
            if up {
                self.transcript_scroll_up(step);
            } else {
                self.transcript_scroll_down(step);
            }
            return true;
        }
        false
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

    // ---- in-app text selection (the Claude-Code drag-to-copy layer) ------
    //
    // The event loop turns raw mouse coordinates into these three calls; the
    // pure mapping + extraction + OSC 52 encoding live in `crate::selection`.
    // All three are fail-open: a point outside the transcript, an empty
    // selection, or stale/empty cached rows simply clears or no-ops.

    /// Map the screen point against the last frame's cached geometry.
    fn map_mouse_point(&self, col: u16, row: u16) -> Option<crate::selection::Point> {
        let rows = self.transcript_rows.borrow();
        let gutters = self.transcript_gutters.borrow();
        crate::selection::screen_to_content(
            col,
            row,
            self.transcript_area.get(),
            self.transcript_first_visible.get(),
            &rows,
            &gutters,
        )
    }

    /// Mouse-down (left button) at screen `(col, row)`: begin a fresh selection
    /// anchored at the mapped content point. A click OUTSIDE the transcript
    /// clears any existing selection (so a copied span un-highlights once the
    /// user clicks away).
    pub fn selection_begin(&mut self, col: u16, row: u16) {
        match self.map_mouse_point(col, row) {
            Some(p) => self.selection = Some(crate::selection::Selection::at(p)),
            None => self.selection = None,
        }
    }

    /// Mouse-drag (left button held) at screen `(col, row)`: extend the live
    /// selection's cursor to the mapped content point. No-op when there's no
    /// active selection or the point doesn't map (e.g. dragged off the area —
    /// the anchor + last good cursor are kept).
    pub fn selection_extend(&mut self, col: u16, row: u16) {
        if let Some(p) = self.map_mouse_point(col, row) {
            if let Some(sel) = self.selection.as_mut() {
                sel.cursor = p;
            }
        }
    }

    /// Mouse-up (left button) — finish the selection and copy it. When there is
    /// a non-empty selection, extracts its text, pushes a "copied N chars" toast
    /// (kept private to this module) and returns `Some(text)` for the caller to
    /// hand to `copy_to_clipboard` (which owns the native-command-vs-OSC 52
    /// decision). The selection is KEPT highlighted so the user sees what was
    /// copied; a later mouse-down elsewhere clears it. Returns `None` (leaving
    /// any single-click selection in place) when nothing is selected — fail-open.
    #[must_use]
    pub fn selection_finish_copy(&mut self) -> Option<String> {
        let sel = self.selection?;
        if sel.is_empty() {
            return None;
        }
        let text = {
            let rows = self.transcript_rows.borrow();
            crate::selection::extract(&rows, &sel)
        };
        if text.is_empty() {
            return None;
        }
        let count = text.chars().count();
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "tui.copied", &[&count.to_string()]),
        );
        Some(text)
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
        // Editing re-opens a dismissed `@`-mention popover and resets its
        // highlight (the candidate set just changed).
        self.mention_selected = 0;
        self.mention_dismissed = false;
    }

    /// Insert a whole string at the cursor (bracketed paste / CJK IME commit).
    /// Newlines are kept (multi-line prompts); other control characters are
    /// dropped so a pasted terminal escape sequence can't corrupt the buffer or
    /// the render. Honors [`INPUT_CAP`] and advances the char-cursor by the
    /// number of characters actually inserted.
    pub fn insert_str_at_cursor(&mut self, text: &str) {
        // Filter to insertable chars + enforce the cap ONCE, then do a SINGLE
        // byte-splice insert. The old per-char loop recomputed `byte_index`
        // (O(cursor)) and `String::insert` (O(n) memmove) for EVERY char — O(n²)
        // overall, so a large paste visibly lagged / appeared to hang (worse on
        // slower consoles, user-reported on Windows).
        let room = INPUT_CAP.saturating_sub(self.input_len());
        let mut buf = String::with_capacity(text.len().min(room * 4));
        let mut added = 0usize;
        for c in text.chars() {
            if added >= room {
                break;
            }
            if c != '\n' && c.is_control() {
                continue;
            }
            buf.push(c);
            added += 1;
        }
        if !buf.is_empty() {
            let pos = self.byte_index(self.input_cursor);
            self.input.insert_str(pos, &buf);
            self.input_cursor += added;
        }
        self.input_history_idx = None;
        self.palette_selected = 0;
        self.mention_selected = 0;
        self.mention_dismissed = false;
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
        self.mention_selected = 0;
        self.mention_dismissed = false;
    }

    /// Delete the character AT the cursor (forward Delete).
    pub fn forward_delete(&mut self) {
        if self.input_cursor >= self.input_len() {
            return;
        }
        let start = self.byte_index(self.input_cursor);
        let end = self.byte_index(self.input_cursor + 1);
        self.input.replace_range(start..end, "");
        self.palette_selected = 0;
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
        self.palette_selected = 0;
    }

    /// Char index of the previous word boundary before the cursor: skip any spaces
    /// to the left, then the word. Shared by Ctrl+W (delete word back) and the
    /// word-left motion (Alt/Ctrl+←).
    fn prev_word_boundary(&self) -> usize {
        let mut c = self.input_cursor;
        let ch_at = |i: usize| self.input[..self.byte_index(i + 1)].chars().last();
        while c > 0 && ch_at(c - 1).is_some_and(char::is_whitespace) {
            c -= 1;
        }
        while c > 0 && ch_at(c - 1).is_some_and(|ch| !ch.is_whitespace()) {
            c -= 1;
        }
        c
    }

    /// Char index of the next word boundary after the cursor: skip the current word
    /// to the right, then trailing spaces. For the word-right motion (Alt/Ctrl+→).
    fn next_word_boundary(&self) -> usize {
        let len = self.input_len();
        let mut c = self.input_cursor;
        let ch_at = |i: usize| self.input[..self.byte_index(i + 1)].chars().last();
        while c < len && ch_at(c).is_some_and(|ch| !ch.is_whitespace()) {
            c += 1;
        }
        while c < len && ch_at(c).is_some_and(char::is_whitespace) {
            c += 1;
        }
        c
    }

    /// Move the caret to the previous / next word boundary (Alt/Ctrl+←/→) — the
    /// readline word-motion every power user reaches for; UmaDev had Ctrl+W delete
    /// but no word MOVE.
    pub fn move_word_left(&mut self) {
        self.input_cursor = self.prev_word_boundary();
        self.input_history_idx = None;
        self.palette_selected = 0;
    }

    /// See [`move_word_left`](Self::move_word_left).
    pub fn move_word_right(&mut self) {
        self.input_cursor = self.next_word_boundary();
        self.input_history_idx = None;
        self.palette_selected = 0;
    }

    /// Delete the word before the cursor (Ctrl+W / Alt+Backspace).
    pub fn delete_word_back(&mut self) {
        let c = self.prev_word_boundary();
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

    /// Try to move the caret UP one wrapped visual row, preserving the display
    /// column (Claude Code parity for multi-line / wrapped prompts). Returns
    /// `true` if the caret actually moved — i.e. it was NOT already on the first
    /// visual row. When it returns `false`, the caller falls through to history
    /// recall. Uses the input width the renderer last published; with no width
    /// yet (pre-first-render) it reports "can't move" so history recall still
    /// works. Fail-open: any degenerate width clamps to 1.
    #[must_use]
    pub fn caret_move_up_wrapped(&mut self) -> bool {
        let w = self.input_text_cols.get();
        if w == 0 {
            return false;
        }
        let (row, col) = crate::ui::caret_in_wrapped(&self.input, self.input_cursor, w);
        if row == 0 {
            return false;
        }
        self.input_cursor = crate::ui::offset_at_wrapped(&self.input, row - 1, col, w);
        true
    }

    /// Try to move the caret DOWN one wrapped visual row, preserving the display
    /// column. Mirror of [`Self::caret_move_up_wrapped`]; returns `false` (so the
    /// caller can recall newer history) when the caret is already on the last
    /// visual row.
    #[must_use]
    pub fn caret_move_down_wrapped(&mut self) -> bool {
        let w = self.input_text_cols.get();
        if w == 0 {
            return false;
        }
        let total = crate::ui::wrapped_row_count(&self.input, w);
        let (row, col) = crate::ui::caret_in_wrapped(&self.input, self.input_cursor, w);
        if row + 1 >= total {
            return false;
        }
        self.input_cursor = crate::ui::offset_at_wrapped(&self.input, row + 1, col, w);
        true
    }

    /// Clear the input buffer + reset cursor + history-recall index.
    pub fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.input_history_idx = None;
        self.input_history_draft = None;
        self.attachments.clear();
        self.mention_selected = 0;
        self.mention_dismissed = false;
    }

    /// The chip token shown in the input box for image attachment `n` (1-based),
    /// e.g. `[图片 1]`. Used both when inserting on paste and when rewriting to an
    /// `@<path>` mention on submit — one definition keeps the two in lockstep.
    fn image_chip(&self, n: usize) -> String {
        format!("[{} {n}]", umadev_i18n::t(self.lang, "attach.image"))
    }

    /// Handle a bracketed-paste payload. If it is one (or several newline-separated)
    /// IMAGE file path(s) — how every terminal delivers a dragged-in image — attach
    /// each as an `[图片 N]` chip; otherwise insert the text verbatim (the common
    /// case). Fail-open: a path that can't be canonicalised / read falls back to
    /// plain text, so a normal paste containing a `.png` word is never swallowed.
    pub fn handle_paste(&mut self, text: &str) {
        let lines: Vec<&str> = text.trim().lines().collect();
        let all_images = !lines.is_empty()
            && lines
                .iter()
                .all(|l| is_image_path(&unquote_unescape(l.trim())));
        if all_images {
            let mut any = false;
            for l in &lines {
                let p = unquote_unescape(l.trim());
                if let Some(n) = self.attach_image(&p) {
                    let chip = self.image_chip(n);
                    self.insert_str_at_cursor(&chip);
                    self.insert_str_at_cursor(" ");
                    any = true;
                }
            }
            if any {
                return;
            }
        }
        // Not an attachable image paste → verbatim (real text, the dominant case).
        self.insert_str_at_cursor(text);
    }

    /// Canonicalise + validate a candidate image path; on success push it to
    /// `attachments` and return its 1-based chip number. `None` (skip) if the path
    /// doesn't resolve, isn't a regular file, or is empty.
    fn attach_image(&mut self, path: &str) -> Option<usize> {
        let abs = std::fs::canonicalize(path).ok()?;
        let meta = std::fs::metadata(&abs).ok()?;
        if !meta.is_file() || meta.len() == 0 {
            return None;
        }
        self.attachments.push(abs);
        Some(self.attachments.len())
    }

    /// Rewrite every `[图片 N]` chip in `raw` to an `@<abs-path>` mention so the base
    /// CLI ingests it as an image (it reads the file itself — UmaDev never base64s).
    /// A chip with no backing attachment is left as-is. No-op without attachments.
    fn expand_attachments(&self, raw: &str) -> String {
        if self.attachments.is_empty() {
            return raw.to_string();
        }
        let mut out = raw.to_string();
        for (i, path) in self.attachments.iter().enumerate() {
            out = out.replace(&self.image_chip(i + 1), &format!("@{}", path.display()));
        }
        out
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
            None => {
                // Recall is BEGINNING — stash whatever the user was typing so
                // stepping forward past the newest entry can bring it back.
                self.input_history_draft = Some(self.input.clone());
                self.input_history.len() - 1
            }
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
            // Stepped forward past the newest entry — restore the draft stashed
            // when recall began instead of discarding what the user was typing.
            self.input_history_idx = None;
            self.input = self.input_history_draft.take().unwrap_or_default();
            self.input_cursor = self.input_len();
        }
    }

    // ---- slash command palette ------------------------------------------

    /// The ONE command registry. The palette autocomplete, the help overlay,
    /// and the dispatch resolver all read this single table (see [`SlashCommand`]
    /// for the anti-drift rationale + the parity test that locks it). Listed in
    /// help-render order within each [`CmdGroup`]. Descriptions are i18n keys so
    /// the palette + help are localized from one string per command.
    pub const COMMANDS: &'static [SlashCommand] = &[
        // ── Worker / brain ────────────────────────────────────────────────
        // `/claude` switches to the claude-code base. No `claude-code` alias — it
        // was redundant with the shorter `/claude` (the backend id stays
        // "claude-code" internally; a mistyped `/claude-code` gets did-you-mean).
        Self::cmd("claude", &[], None, CmdGroup::Worker, "tui.cmd.claude"),
        Self::cmd("codex", &[], None, CmdGroup::Worker, "tui.cmd.codex"),
        Self::cmd("opencode", &[], None, CmdGroup::Worker, "tui.cmd.opencode"),
        Self::cmd(
            "offline",
            &[],
            None,
            CmdGroup::Worker,
            "tui.help.worker.offline",
        ),
        Self::cmd(
            "model",
            &[],
            Some("<id>"),
            CmdGroup::Worker,
            "tui.help.worker.model",
        ),
        // ── Pipeline & gates ──────────────────────────────────────────────
        Self::cmd(
            "run",
            &[],
            Some("[slug] <req>"),
            CmdGroup::Pipeline,
            "tui.help.pipe.run",
        ),
        Self::cmd(
            "goal",
            &[],
            Some("<objective>"),
            CmdGroup::Pipeline,
            "tui.cmd.goal",
        ),
        Self::cmd(
            "quick",
            &[],
            Some("<task>"),
            CmdGroup::Pipeline,
            "tui.help.pipe.quick",
        ),
        Self::cmd(
            "plan",
            &[],
            Some("skip|add|veto|up|down <id>"),
            CmdGroup::Pipeline,
            "tui.cmd.plan",
        ),
        Self::cmd(
            "continue",
            &[],
            None,
            CmdGroup::Pipeline,
            "tui.help.pipe.continue",
        ),
        Self::cmd(
            "revise",
            &[],
            Some("<txt>"),
            CmdGroup::Pipeline,
            "tui.help.pipe.revise",
        ),
        Self::cmd(
            "redo",
            &[],
            Some("[phase]"),
            CmdGroup::Pipeline,
            "tui.help.pipe.redo",
        ),
        Self::cmd(
            "rewind",
            &["rollback-files"],
            Some("[id]"),
            CmdGroup::Pipeline,
            "tui.help.pipe.rewind",
        ),
        Self::cmd(
            "checkpoint",
            &["snapshot"],
            Some("[label]"),
            CmdGroup::Pipeline,
            "tui.cmd.checkpoint",
        ),
        Self::cmd(
            "cancel",
            &["abort"],
            None,
            CmdGroup::Pipeline,
            "tui.cmd.cancel",
        ),
        Self::cmd("init", &[], None, CmdGroup::Pipeline, "tui.help.pipe.init"),
        Self::cmd("adopt", &[], None, CmdGroup::Pipeline, "tui.cmd.adopt"),
        Self::cmd(
            "manual",
            &[],
            None,
            CmdGroup::Pipeline,
            "tui.help.pipe.manual",
        ),
        Self::cmd("auto", &[], None, CmdGroup::Pipeline, "tui.help.pipe.auto"),
        Self::cmd(
            "mode",
            &[],
            Some("plan|guarded|auto"),
            CmdGroup::Pipeline,
            "tui.cmd.mode",
        ),
        Self::cmd(
            "diff",
            &[],
            Some("[artifact]"),
            CmdGroup::Pipeline,
            "tui.help.pipe.diff",
        ),
        // ── Ship it ───────────────────────────────────────────────────────
        Self::cmd(
            "preview",
            &[],
            None,
            CmdGroup::Ship,
            "tui.help.ship.preview",
        ),
        Self::cmd(
            "stop-preview",
            &[],
            None,
            CmdGroup::Ship,
            "tui.help.ship.stop_preview",
        ),
        Self::cmd("deploy", &[], None, CmdGroup::Ship, "tui.help.ship.deploy"),
        Self::cmd(
            "pr",
            &[],
            Some("[create]"),
            CmdGroup::Ship,
            "tui.help.ship.pr",
        ),
        Self::cmd("export", &[], None, CmdGroup::Ship, "tui.help.ship.export"),
        // ── Design & inspect ──────────────────────────────────────────────
        Self::cmd(
            "design",
            &[],
            Some("<name>"),
            CmdGroup::Inspect,
            "tui.help.inspect.design",
        ),
        Self::cmd(
            "template",
            &[],
            Some("<name>"),
            CmdGroup::Inspect,
            "tui.help.inspect.template",
        ),
        Self::cmd(
            "status",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.status",
        ),
        Self::cmd(
            "pitfalls",
            &["踩坑"],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.pitfalls",
        ),
        Self::cmd("lessons", &[], None, CmdGroup::Inspect, "tui.cmd.lessons"),
        Self::cmd("team", &[], None, CmdGroup::Inspect, "tui.cmd.team"),
        Self::cmd(
            "runs",
            &["history-runs"],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.runs",
        ),
        Self::cmd(
            "knowledge",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.knowledge",
        ),
        Self::cmd("mcp", &[], None, CmdGroup::Inspect, "tui.help.inspect.mcp"),
        Self::cmd(
            "skill",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.skill",
        ),
        Self::cmd(
            "usage",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.usage",
        ),
        Self::cmd(
            "spec",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.spec",
        ),
        Self::cmd(
            "verify",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.verify",
        ),
        Self::cmd(
            "config",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.config",
        ),
        Self::cmd(
            "doctor",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.doctor",
        ),
        Self::cmd(
            "history",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.history",
        ),
        Self::cmd(
            "sessions",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.sessions",
        ),
        Self::cmd(
            "resume",
            &[],
            Some("<id>"),
            CmdGroup::Inspect,
            "tui.help.inspect.resume",
        ),
        Self::cmd(
            "version",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.version",
        ),
        Self::cmd(
            "changelog",
            &[],
            None,
            CmdGroup::Inspect,
            "tui.help.inspect.changelog",
        ),
        Self::cmd("bug", &[], None, CmdGroup::Inspect, "tui.help.inspect.bug"),
        // ── UI & settings ─────────────────────────────────────────────────
        Self::cmd(
            "lang",
            &["language", "语言", "語言"],
            Some("[zh-CN|zh-TW|en]"),
            CmdGroup::System,
            "tui.cmd.lang",
        ),
        Self::cmd(
            "setup",
            &["reconfigure", "配置", "設定"],
            None,
            CmdGroup::System,
            "tui.cmd.setup",
        ),
        Self::cmd("guide", &[], None, CmdGroup::System, "tui.cmd.guide"),
        Self::cmd(
            "animations",
            &[],
            None,
            CmdGroup::System,
            "tui.cmd.animations",
        ),
        Self::cmd("mouse", &[], None, CmdGroup::System, "tui.cmd.mouse"),
        Self::cmd(
            "redraw",
            &["repaint"],
            None,
            CmdGroup::System,
            "tui.cmd.redraw",
        ),
        // ── Session & exit ────────────────────────────────────────────────
        Self::cmd(
            "compact",
            &[],
            None,
            CmdGroup::Session,
            "tui.help.edit.compact",
        ),
        Self::cmd("clear", &[], None, CmdGroup::Session, "tui.help.edit.clear"),
        Self::cmd(
            "help",
            &["?", "commands"],
            None,
            CmdGroup::Session,
            "tui.help.edit.help",
        ),
        Self::cmd(
            "quit",
            &["q", "exit"],
            None,
            CmdGroup::Session,
            "tui.help.edit.quit",
        ),
    ];

    /// Const constructor for a [`SlashCommand`] registry row — keeps each
    /// [`COMMANDS`](Self::COMMANDS) entry a single readable line. Every command
    /// is visible (`hidden: false`); the field exists for future internal verbs.
    const fn cmd(
        name: &'static str,
        aliases: &'static [&'static str],
        arg_hint: Option<&'static str>,
        group: CmdGroup,
        desc_key: &'static str,
    ) -> SlashCommand {
        SlashCommand {
            name,
            aliases,
            arg_hint,
            group,
            desc_key,
            hidden: false,
        }
    }

    /// Resolve a typed verb (after `/`, lowercased for ASCII) to its registry
    /// entry by canonical name OR any alias. `None` for an unknown verb (e.g. a
    /// dynamic per-backend id like `goose`, handled by the dispatch fallback).
    #[must_use]
    pub fn resolve_command(verb: &str) -> Option<&'static SlashCommand> {
        Self::COMMANDS
            .iter()
            .find(|c| c.name == verb || c.aliases.contains(&verb))
    }

    /// Match the verbs prefixed by what comes after `/` in the current
    /// input. Empty input or non-slash input → empty list.
    ///
    /// Combines the registry [`COMMANDS`](Self::COMMANDS) with the dynamic
    /// per-backend verbs (so typing `/go` suggests `/goose`, typing `/am`
    /// suggests `/claude`, `/codex`, etc.) — kept in sync with `BACKEND_IDS`.
    /// Descriptions are localized for the active language; hidden commands are
    /// never suggested.
    #[must_use]
    pub fn palette_matches(&self) -> Vec<PaletteEntry> {
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
        // Match by SUBSEQUENCE (every typed char appears in order) so a fuzzy stub
        // like `/dpl` finds `/deploy`, not just a literal prefix — but only once ≥2
        // chars are typed, so a single `/r` doesn't explode into every verb that
        // merely contains an 'r'. Prefix matches still rank first (stable sort).
        let fuzzy = typed.chars().count() >= 2;
        let hit = |verb: &str| {
            verb.starts_with(typed.as_str()) || (fuzzy && is_subsequence(&typed, verb))
        };
        let mut out: Vec<PaletteEntry> = Self::COMMANDS
            .iter()
            .filter(|c| !c.hidden && hit(c.name))
            .map(|c| PaletteEntry {
                verb: c.name,
                desc: umadev_i18n::t(self.lang, c.desc_key),
                arg_hint: c.arg_hint,
            })
            .collect();
        // Skip ids already covered by the registry (the three first-class
        // base CLIs) to avoid duplicate palette rows.
        let known: std::collections::HashSet<&str> = out.iter().map(|p| p.verb).collect();
        for (id, hint) in backend_palette_verbs() {
            if !known.contains(id) && hit(id) {
                out.push(PaletteEntry {
                    verb: id,
                    desc: hint,
                    arg_hint: None,
                });
            }
        }
        // Prefix matches before looser subsequence matches (stable sort preserves
        // the canonical verb order within each tier).
        out.sort_by_key(|p| !p.verb.starts_with(typed.as_str()));
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
        let verb = matches[selected].verb;
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

    // ---- @-file-mention typeahead ----------------------------------------

    /// If the cursor sits inside an `@`-file-mention token, return
    /// `(at_char, partial)`: `at_char` is the char index of the `@`, and
    /// `partial` is the text between the `@` and the cursor (the live filter).
    ///
    /// `None` when there is no active mention — there is no `@` to the left over
    /// an unbroken run of `[\w./-]` chars, OR the `@` is glued to a preceding
    /// non-space (so an email like `a@b` never opens the popover). Mirrors the
    /// slash palette's "is it open?" decision, but for a token ANYWHERE under the
    /// cursor instead of a `/` prefix on the whole input.
    #[must_use]
    fn mention_token(&self) -> Option<(usize, String)> {
        let chars: Vec<char> = self.input.chars().collect();
        let cur = self.input_cursor.min(chars.len());
        let mut i = cur;
        while i > 0 {
            let c = chars[i - 1];
            if c == '@' {
                // `@` must start the input or follow whitespace, else it is part
                // of a larger token (an email address), not a file mention.
                if i == 1 || chars[i - 2].is_whitespace() {
                    let partial: String = chars[i..cur].iter().collect();
                    return Some((i - 1, partial));
                }
                return None;
            }
            if is_mention_char(c) {
                i -= 1;
                continue;
            }
            return None; // a non-mention, non-`@` char before any `@`
        }
        None
    }

    /// Build (once) + cache the repo-relative file list backing the `@`-mention
    /// typeahead. The scan runs lazily on the first `@` and is then reused, so
    /// typing stays fast (no per-keystroke filesystem walk). Fail-open.
    fn ensure_mention_files(&self) {
        if self.mention_files.borrow().is_some() {
            return;
        }
        let files = collect_repo_files(&self.project_root);
        *self.mention_files.borrow_mut() = Some(files);
    }

    /// The ranked `@`-mention candidates for the partial currently under the
    /// cursor: repo-relative paths filtered by prefix / subsequence (basename
    /// first, then full path), capped at [`MENTION_MATCH_CAP`].
    ///
    /// Empty when no `@`-token is active, the popover was dismissed (Esc), or
    /// nothing matches — so this is the single "is the `@`-popover open?"
    /// predicate. The caller keeps it mutually exclusive with the slash palette
    /// (the `@` popover wins when a token is under the cursor).
    #[must_use]
    pub fn mention_matches(&self) -> Vec<String> {
        if self.mention_dismissed {
            return Vec::new();
        }
        let Some((_, partial)) = self.mention_token() else {
            return Vec::new();
        };
        self.ensure_mention_files();
        let files = self.mention_files.borrow();
        let Some(files) = files.as_ref() else {
            return Vec::new();
        };
        let p = partial.to_lowercase();
        // Subsequence (fuzzy) only kicks in at ≥2 typed chars, so a lone `@a`
        // doesn't explode into every path containing an 'a' (mirrors the palette).
        let fuzzy = p.chars().count() >= 2;
        let mut ranked: Vec<(u8, &String)> = Vec::new();
        for f in files {
            let path_l = f.to_lowercase();
            let base_l = f.rsplit('/').next().unwrap_or(f).to_lowercase();
            // Rank: basename prefix (best) → path prefix → fuzzy subsequence.
            let rank = if p.is_empty() || base_l.starts_with(&p) {
                0
            } else if path_l.starts_with(&p) {
                1
            } else if fuzzy && (is_subsequence(&p, &base_l) || is_subsequence(&p, &path_l)) {
                2
            } else {
                continue;
            };
            ranked.push((rank, f));
        }
        // Stable sort → alphabetical order preserved within each rank tier.
        ranked.sort_by_key(|(rank, _)| *rank);
        ranked
            .into_iter()
            .take(MENTION_MATCH_CAP)
            .map(|(_, f)| f.clone())
            .collect()
    }

    /// Move the `@`-mention highlight up/down (wrap-around), like
    /// [`Self::cycle_palette`].
    pub fn cycle_mention(&mut self, delta: isize) {
        let count = self.mention_matches().len();
        if count == 0 {
            return;
        }
        if delta < 0 {
            let back = delta.unsigned_abs() % count;
            self.mention_selected = (self.mention_selected + count - back) % count;
        } else {
            #[allow(clippy::cast_sign_loss)]
            let fwd = delta as usize;
            self.mention_selected = (self.mention_selected + fwd) % count;
        }
    }

    /// Insert the highlighted `@`-mention candidate: replace the `@partial`
    /// token under the cursor with `@<path> ` (trailing space) and place the
    /// caret after it. Also consumes any mention-char tail to the RIGHT of the
    /// cursor so editing mid-token replaces the whole reference. No-op when the
    /// popover is empty. The trailing space ends the token, so the popover closes.
    pub fn accept_mention(&mut self) {
        let matches = self.mention_matches();
        if matches.is_empty() {
            return;
        }
        let Some((at_char, _)) = self.mention_token() else {
            return;
        };
        let sel = self.mention_selected.min(matches.len() - 1);
        let chars: Vec<char> = self.input.chars().collect();
        let mut end = self.input_cursor.min(chars.len());
        while end < chars.len() && is_mention_char(chars[end]) {
            end += 1;
        }
        let start_b = self.byte_index(at_char);
        let end_b = self.byte_index(end);
        let replacement = format!("@{} ", matches[sel]);
        let added = replacement.chars().count();
        self.input.replace_range(start_b..end_b, &replacement);
        self.input_cursor = at_char + added;
        self.mention_selected = 0;
        self.mention_dismissed = false;
    }

    /// Esc while the `@`-mention popover is open closes it WITHOUT inserting —
    /// the prompt text is untouched and the popover stays closed until the next
    /// edit re-opens it.
    pub fn dismiss_mention(&mut self) {
        self.mention_dismissed = true;
    }

    fn set_phase(&mut self, phase: Phase, status: PhaseStatus) {
        if let Some(row) = self.phases.iter_mut().find(|r| r.phase == phase) {
            row.status = status;
        }
    }

    // ---- Wave-1 visible surface (intent card / live plan / team review) ----

    /// Render the **intent pre-commitment card** ([`EngineEvent::IntentDecided`]):
    /// a single, prominent UmaDev message telling the user what the router
    /// decided BEFORE any work — class, depth, rough budget, and a one-line
    /// reason ("this is a full product — I'll BUILD …" / "small change, on it").
    /// Also records `last_intent_class` so the status chip can show fast-vs-
    /// deliberate. Fail-open: unknown class/depth ids fall back to neutral text.
    fn apply_intent_decided(
        &mut self,
        class: &str,
        depth: &str,
        team: &[String],
        est_tool_calls: u32,
        rationale: &str,
    ) {
        self.last_intent_class = Some(class.to_string());
        // A localized headline per class, so the card reads like a director's
        // pre-commitment rather than a debug dump.
        let headline_key = match class {
            "build" => "intent.build",
            "debug" => "intent.debug",
            "quick_edit" => "intent.quick_edit",
            "explain" => "intent.explain",
            _ => "intent.chat",
        };
        let depth_label = match depth {
            "deep" => umadev_i18n::t(self.lang, "intent.depth.deep"),
            "standard" => umadev_i18n::t(self.lang, "intent.depth.standard"),
            _ => umadev_i18n::t(self.lang, "intent.depth.fast"),
        };
        let mut body = umadev_i18n::tf(self.lang, headline_key, &[depth_label]);
        // Append the rough budget + (for deliberate turns) the convened team, so
        // the user sees the expected cost + who's on it up front.
        if est_tool_calls > 0 {
            body.push_str(&umadev_i18n::tf(
                self.lang,
                "intent.budget",
                &[&est_tool_calls.to_string()],
            ));
        }
        if !team.is_empty() {
            body.push_str(&umadev_i18n::tf(
                self.lang,
                "intent.team",
                &[&team.join(", ")],
            ));
        }
        // The router's own one-line rationale, kept verbatim as a second line so
        // the *why* is visible (it is already localized at the source).
        let rationale = rationale.trim();
        if !rationale.is_empty() {
            body.push('\n');
            body.push_str(rationale);
        }
        self.push(ChatRole::UmaDev, body);
    }

    /// Initialise the **live plan checklist** ([`EngineEvent::PlanPosted`]) from
    /// the freshly synthesised plan. Each `PlanPosted` summary is `id · title
    /// (seat)`; we keep the id + title and start every step `pending`. The panel
    /// (rendered above the prompt) then ticks off live via `PlanStepStatus`,
    /// replacing the frozen 0/9 dot bar on the director path. A one-line "posted
    /// N steps" memo also lands in the transcript so scrollback records it.
    fn apply_plan_posted(&mut self, steps: &[String], _done: usize, total: usize) {
        self.plan_steps = steps
            .iter()
            .enumerate()
            .map(|(i, summary)| {
                // `id · title (seat)` — split off the leading `id ·`; if the
                // shape is unexpected, fall back to a positional id + the whole
                // summary as the title (fail-open, never drops a step).
                let (id, title) = split_plan_summary(summary, i);
                PlanStepRow {
                    id,
                    title,
                    status: "pending".to_string(),
                }
            })
            .collect();
        // A fresh plan un-collapses the panel so the first plan is always seen,
        // and seals any open review round (a re-plan starts a clean review cycle).
        self.plan_collapsed = false;
        self.critic_round_open = false;
        if total > 0 {
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(self.lang, "plan.posted", &[&total.to_string()]),
            );
        }
    }

    /// Tick one step in the live checklist ([`EngineEvent::PlanStepStatus`]).
    /// Matches by id; if the id is unknown (a step the post didn't carry) it is
    /// appended so the panel never silently loses a transition. Fail-open: an
    /// unrecognised status string renders as a neutral pending dot.
    fn apply_plan_step_status(&mut self, id: &str, title: &str, status: &str) {
        // A step transition means the director is between review bursts (a build
        // step is running, or a review step just (de)activated) — seal the open
        // review round so the NEXT verdict clears the prior round's seats instead
        // of mixing two rounds. A review burst itself emits no `PlanStepStatus`,
        // so this never splits a round mid-flight.
        self.critic_round_open = false;
        if let Some(row) = self.plan_steps.iter_mut().find(|s| s.id == id) {
            row.status = status.to_string();
            if !title.trim().is_empty() {
                row.title = title.to_string();
            }
        } else {
            self.plan_steps.push(PlanStepRow {
                id: id.to_string(),
                title: if title.trim().is_empty() {
                    id.to_string()
                } else {
                    title.to_string()
                },
                status: status.to_string(),
            });
        }
    }

    /// Record one reviewing seat's verdict for the **collapsible team-review
    /// panel** ([`EngineEvent::CriticVerdict`]). A repeated seat id within the
    /// SAME round replaces its prior row (a re-review updates in place, never
    /// stacks).
    ///
    /// Two extra guarantees on top of the panel:
    /// - **Fresh-round clearing.** When the prior round was sealed (a plan-step
    ///   transition / phase start happened since the last verdict, so
    ///   `critic_round_open == false`), the first verdict of the new round clears
    ///   the previous round's rows first — the panel shows the CURRENT round, not
    ///   a stale mix of two rounds' seats.
    /// - **Transcript emission.** Every verdict is ALSO pushed as a `System` note
    ///   carrying the seat + verdict + its full blocking findings, so the
    ///   complete set is always in the scrollable history. The panel's compact
    ///   "… +N" tail can clip rows; the transcript never loses content.
    fn apply_critic_verdict(
        &mut self,
        seat: String,
        accepts: bool,
        blocking: Vec<String>,
        advisory: Vec<String>,
    ) {
        // A sealed round means this verdict opens a NEW review round — drop the
        // previous round's rows before the new seats land so the panel can't show
        // a half-old / half-new mix.
        if !self.critic_round_open {
            self.critic_verdicts.clear();
            self.critic_round_open = true;
        }
        // Mirror the full verdict into the transcript (the never-lost source of
        // truth) before the value is moved into the panel row.
        self.push_critic_note(&seat, accepts, &blocking);
        let row = CriticRow {
            seat,
            accepts,
            blocking,
            advisory,
        };
        if let Some(existing) = self.critic_verdicts.iter_mut().find(|c| c.seat == row.seat) {
            *existing = row;
        } else {
            self.critic_verdicts.push(row);
        }
    }

    /// Push one reviewing seat's verdict into the transcript as a `System` note —
    /// the unbounded, scrollable record that guarantees a blocking critic's full
    /// findings are never hidden behind the panel's "… +N" clip. An accept is one
    /// line; a block lists every must-fix finding underneath. Localized.
    fn push_critic_note(&mut self, seat: &str, accepts: bool, blocking: &[String]) {
        let mut body = if accepts {
            umadev_i18n::tf(self.lang, "plan.review.note.accept", &[seat])
        } else {
            umadev_i18n::tf(
                self.lang,
                "plan.review.note.block",
                &[seat, &blocking.len().max(1).to_string()],
            )
        };
        for b in blocking {
            let item = b.trim();
            if !item.is_empty() {
                body.push_str(&format!("\n  - {item}"));
            }
        }
        self.push(ChatRole::System, body);
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
            // Wave-1 router / plan / critic events — the visible surface that makes
            // "routes intelligently, owns a steerable plan, behaves like a director"
            // actually appear on screen. Each arm is fail-open: a missing / odd field
            // degrades to a sensible default, never panics.
            EngineEvent::IntentDecided {
                class,
                depth,
                team,
                est_tool_calls,
                rationale,
            } => self.apply_intent_decided(&class, &depth, &team, est_tool_calls, &rationale),
            EngineEvent::PlanPosted { steps, done, total } => {
                self.apply_plan_posted(&steps, done, total);
            }
            EngineEvent::PlanStepStatus { id, title, status } => {
                self.apply_plan_step_status(&id, &title, &status);
            }
            EngineEvent::CriticVerdict {
                seat,
                accepts,
                blocking,
                advisory,
            } => self.apply_critic_verdict(seat, accepts, blocking, advisory),
            EngineEvent::PhaseStarted { phase } => {
                self.set_phase(phase, PhaseStatus::Running);
                self.phase_started_at = Some(std::time::Instant::now());
                // A phase is STARTING — the run is demonstrably alive and
                // progressing, so any prior terminal flag is stale. Clearing it
                // here keeps the status/placeholder honest if a single block
                // aborted earlier and the director then recovered into a new phase
                // (otherwise the run kept advancing under a frozen "本轮已中止").
                self.aborted = false;
                self.finished = false;
                // Fresh phase → fresh stall clock; nothing has stalled yet.
                self.last_output_at = Some(std::time::Instant::now());
                self.tool_in_progress = false;
                // A new phase is a clean boundary — seal any open review round so
                // the next phase's verdicts don't pile onto the last phase's.
                self.critic_round_open = false;
                // A new phase replaces any prior phase's lingering heartbeat
                // line so a stale "still working" timer never bleeds across.
                self.transient_status = None;
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
                // A gate is a CHECKPOINT, not a work phase, so it never receives a
                // PhaseCompleted — its dot would sit at ○ (indistinguishable from
                // "skipped / not reached") while later phases run, which reads as
                // "why is step 3 empty?". Reaching the gate means the preceding
                // work phase finished and we're standing on the checkpoint, so
                // fill its dot. (Clarify isn't part of the 9-dot chain.)
                let gate_phase = match gate {
                    umadev_agent::gates::Gate::DocsConfirm => Some(Phase::DocsConfirm),
                    umadev_agent::gates::Gate::PreviewConfirm => Some(Phase::PreviewConfirm),
                    umadev_agent::gates::Gate::ClarifyGate => None,
                };
                if let Some(p) = gate_phase {
                    self.set_phase(p, PhaseStatus::Done);
                }
                // Block paused at a gate — stop the live elapsed counters
                // so the status bar doesn't keep ticking while we wait on
                // the user. Also drop any heartbeat line: the wait is over.
                self.run_started_at = None;
                self.phase_started_at = None;
                self.transient_status = None;

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
                    // Surface the local preview URL right in the completion
                    // banner so a finished Delivery build doesn't "just stop" with
                    // no demo address (the actual dev server is auto-started by the
                    // event loop on the same transition). Fail-open: a non-web
                    // project (no dev server detected) adds no line.
                    let preview_line = self
                        .effective_preview_url()
                        .map(|u| umadev_i18n::tf(lang, "delivery.preview_line", &[&u]))
                        .unwrap_or_default();
                    let zip_info = format!("{zip_info}{scorecard}{preview_line}");
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::tf(lang, "delivery.complete_banner", &[&zip_info]),
                    );
                    // The run is settled — fold the last review round into a
                    // one-line summary in the transcript (the per-seat verdicts are
                    // already in scrollback) and drop the live plan / team-review
                    // panel so a finished run stops rendering a stale live list.
                    self.finalize_live_panels();
                }
            }
            EngineEvent::BackendProbed {
                backend_id,
                ready,
                detail,
            } => {
                // The `BackendProbed` event only carries `{id, ready, detail}` (it
                // lives in umadev-agent and we don't change it), so the honest auth
                // state + the base's login/install commands ride along packed into
                // `detail` by `spawn_probe`. Unpack here, fail-open to Unknown / no
                // hint if the tag is absent (an external emitter, an older build).
                let (auth, login_cmd, install_cmd, human) = parse_probe_detail(&detail);
                // Update or append the probe row.
                if let Some(existing) = self.backends.iter_mut().find(|b| b.id == backend_id) {
                    existing.ready = ready;
                    existing.detail = human.clone();
                    existing.auth = auth;
                    existing.login_cmd = login_cmd.clone();
                    existing.install_cmd = install_cmd.clone();
                } else {
                    self.backends.push(BackendInfo {
                        id: backend_id.clone(),
                        ready,
                        detail: human.clone(),
                        auth,
                        login_cmd: login_cmd.clone(),
                        install_cmd: install_cmd.clone(),
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
                // Group only into a live Host *text* bubble. A trailing tool
                // row (Host role, but a `Tool` body) must NOT absorb the line —
                // `text_mut()` returns `None` there, so we fall through to a
                // fresh text bubble (fail-open).
                let appended = self
                    .history
                    .back_mut()
                    .filter(|last| last.role == ChatRole::Host)
                    .and_then(ChatMessage::text_mut)
                    .map(|s| {
                        s.push('\n');
                        s.push_str(&trimmed);
                    })
                    .is_some();
                if !appended {
                    self.push(ChatRole::Host, trimmed);
                }
            }
            EngineEvent::TurnUsage {
                input_tokens,
                output_tokens,
            } => {
                // Accumulate the base's REAL reported per-turn usage into the live
                // session total shown on the waiting indicator — true consumption
                // (the base's own numbers), accruing across this session's turns.
                self.session_tokens = self
                    .session_tokens
                    .saturating_add(u64::from(input_tokens) + u64::from(output_tokens));
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
            EngineEvent::TransientStatus(status) => {
                // The long-phase heartbeat's periodic "still working (mm:ss)"
                // beat. This NEVER touches the transcript — it overwrites a
                // single in-place status field that the bottom status row
                // renders, so a multi-minute wait shows ONE live-updating line
                // instead of a fresh row every ~7s. A beat is still a sign of
                // life (the base is working in the background) → reset the stall
                // clock so the red cue doesn't fire mid-heartbeat.
                self.mark_output();
                self.transient_status = status;
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
                        // A long reply is NEVER truncated. CJK hits any byte budget
                        // in a few sentences (3 bytes/char), so instead of a `…` cap
                        // we roll the live segment over into a FRESH Host message past
                        // a soft threshold — natural segmentation that keeps the whole
                        // reply visible and the transcript pre-folding each segment.
                        const SEGMENT_BYTES: usize = 4000;
                        // Hard ceiling: never let one segment grow past this even
                        // mid-fence, so a runaway un-closed ``` can't make a segment
                        // unbounded (the markdown renderer's fail-open still applies).
                        const SEGMENT_BYTES_MAX: usize = 24_000;
                        self.stream_tool_batch = None;
                        // Decide WHERE the delta goes WITHOUT holding a mutable borrow
                        // across `self.push`: append to the live Host segment if it
                        // still has room, else roll over to a new one. Fence-safe: a
                        // naive `len < 4000` cut could split a ```fence``` across two
                        // independently-rendered segments (opening ``` in A, closing
                        // in B → both scramble), so we only roll over at the soft cap
                        // when there is no open fence; inside one we keep appending
                        // (up to the hard ceiling) until it closes.
                        let append_in_place = self.stream_text_active
                            && self.history.back().is_some_and(|m| {
                                if m.role != ChatRole::Host {
                                    return false;
                                }
                                // A live Host *text* segment only — a tool row never
                                // absorbs streamed prose.
                                let MessageBody::Text(body) = &m.kind else {
                                    return false;
                                };
                                if body.len() >= SEGMENT_BYTES_MAX {
                                    return false;
                                }
                                body.len() < SEGMENT_BYTES || has_open_code_fence(body)
                            });
                        if append_in_place {
                            // **Append VERBATIM** — preserve whitespace. The base now
                            // streams raw token deltas (`--include-partial-messages`),
                            // so an inter-word space ' ' or a paragraph break '\n\n'
                            // routinely arrives as its OWN delta. Dropping it (the old
                            // `if !delta.trim().is_empty()` guard) mashed 'foo'+' '+
                            // 'bar' into 'foobar' and collapsed blank lines. P5c:
                            // real content ends the reasoning block.
                            self.collapse_thinking_block();
                            if let Some(last) =
                                self.history.back_mut().and_then(ChatMessage::text_mut)
                            {
                                last.push_str(&delta);
                            }
                        } else if !delta.trim().is_empty() {
                            // Fresh stream / rollover — but a pure-whitespace delta
                            // must NOT open a new (blank) Host bubble; wait for real
                            // content. P5a: a fresh segment is a fresh body — drop the
                            // stable-prefix cache so it never reuses the prior
                            // segment's render against the new (smaller) body.
                            self.collapse_thinking_block();
                            self.reset_stream_md_cache();
                            self.push(ChatRole::Host, delta);
                            self.stream_text_active = true;
                        }
                    }
                    umadev_runtime::StreamEvent::ToolUse { name, detail, edit } => {
                        // P5c: a tool call ends the reasoning block.
                        self.collapse_thinking_block();
                        self.stream_text_active = false; // text stream interrupted
                                                         // A tool call is now in flight — a long one (npm install)
                                                         // is WORK, not a stall, so suppress the red signal until
                                                         // its result returns.
                        self.tool_in_progress = true;
                        // Adaptive stall threshold: a dependency install / full
                        // build legitimately runs for minutes with no output, so
                        // widen the stall window while one is in flight (only a
                        // `Bash`/`run` tool can be one). Cleared on its result.
                        self.long_op_in_progress =
                            matches!(name.as_str(), "Bash") && is_long_running_command(&detail);
                        // P1: a Write/Edit that carries structured content renders
                        // as a live diff card the moment the tool_use arrives (we
                        // do NOT wait for the result). Any other tool — or an edit
                        // with no recoverable content — falls back to the plain
                        // tool row. Fully fail-open.
                        match edit {
                            Some(e) if matches!(name.as_str(), "Write" | "Edit" | "MultiEdit") => {
                                self.push_diff(&e);
                            }
                            _ => self.push_tool_use(&name, &detail),
                        }
                    }
                    umadev_runtime::StreamEvent::ToolResult { ok, summary } => {
                        // P5c: a result is content → close any open reasoning block.
                        self.collapse_thinking_block();
                        self.stream_text_active = false;
                        // The in-flight tool call returned → no longer "working
                        // on a tool"; the stall clock applies normally again.
                        self.tool_in_progress = false;
                        self.long_op_in_progress = false;
                        self.attach_tool_result(ok, &summary);
                    }
                    umadev_runtime::StreamEvent::Warning { message } => {
                        // P5c: a warning closes any open reasoning block.
                        self.collapse_thinking_block();
                        self.stream_text_active = false;
                        if is_transient_warning(&message) {
                            // A RECOVERABLE mid-turn hiccup (rate-limit / overloaded /
                            // retry) — show it as ONE muted live status line that the
                            // next beat overwrites, NOT a permanent "[warn]" transcript
                            // row. Otherwise a flurry of retries spams the region next
                            // to the still-running "正在思考 (Ns)" timer and reads like
                            // the turn is failing — but the turn keeps running; only a
                            // terminal ABORT settles it. (The timer is correct; this is
                            // the "时间会乱弹错误" report.)
                            self.transient_status = Some(format!("· {message}"));
                        } else {
                            self.push(ChatRole::System, format!("[warn] {message}"));
                        }
                    }
                    umadev_runtime::StreamEvent::Thinking => {
                        // P5c: open (once) a reasoning block. A burst of `Thinking`
                        // events must NOT stack a wall of `[thinking]` rows — the
                        // FIRST opens one live placeholder (the bottom waiting
                        // indicator animates the spinner); subsequent ones are
                        // no-ops until the block collapses on the next real content.
                        self.stream_text_active = false;
                        self.stream_tool_batch = None;
                        self.open_thinking_block();
                    }
                    umadev_runtime::StreamEvent::ThinkingDelta(delta) => {
                        // Phase-2-C-P0 transparency: accumulate the base's
                        // extended-thinking reasoning into ONE collapsible
                        // `[thinking]` block (default collapsed — the global Ctrl+O
                        // verbose toggle / Ctrl+R expands it). The first delta opens
                        // the block (mirrors `Thinking`); every later delta appends
                        // to the SAME row, so a long reasoning stream is one foldable
                        // block, never a row per delta. The live "正在思考 (Ns)"
                        // spinner is the bottom waiting indicator and is untouched.
                        self.stream_text_active = false;
                        self.stream_tool_batch = None;
                        self.open_thinking_block();
                        if let Some(idx) = self.thinking_block_idx {
                            if let Some(text) =
                                self.history.get_mut(idx).and_then(ChatMessage::text_mut)
                            {
                                // The reasoning lives BELOW the `[thinking] …` header
                                // line, so the first chunk starts a fresh line. Bound
                                // the block so a runaway stream can't grow unbounded.
                                if text.len() < THINKING_REASONING_MAX {
                                    if !text.contains('\n') {
                                        text.push('\n');
                                    }
                                    text.push_str(&delta);
                                }
                            }
                        }
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
            // ↑ / k — move up, wrapping from the top row to the bottom (Claude
            // Code / opencode list parity: a long base-CLI list is much faster to
            // reach the last entry by pressing ↑ once than holding ↓).
            KeyCode::Up | KeyCode::Char('k' | 'K') => {
                self.picker_notice = None;
                let len = self.picker_items.len();
                if len > 0 {
                    self.picker_selected = if self.picker_selected == 0 {
                        len - 1
                    } else {
                        self.picker_selected - 1
                    };
                }
                Action::None
            }
            // ↓ / j — move down, wrapping from the bottom row back to the top.
            KeyCode::Down | KeyCode::Char('j' | 'J') => {
                self.picker_notice = None;
                let len = self.picker_items.len();
                if len > 0 {
                    self.picker_selected = (self.picker_selected + 1) % len;
                }
                Action::None
            }
            // Home / PageUp — jump to the first row (picker lists are short enough
            // that a page == the whole list, matching the overlay's PageUp reaching
            // the top).
            KeyCode::Home | KeyCode::PageUp => {
                self.picker_notice = None;
                self.picker_selected = 0;
                Action::None
            }
            // End / PageDown — jump to the last row.
            KeyCode::End | KeyCode::PageDown => {
                self.picker_notice = None;
                self.picker_selected = self.picker_items.len().saturating_sub(1);
                Action::None
            }
            // Digit quick-select: `1`-`9` jump straight to that 1-based row (uses
            // `PickerStep::number()`-style 1-based indexing). A digit past the end
            // is ignored. Only fires when the typed digit maps to an existing row.
            KeyCode::Char(c @ '1'..='9') => {
                self.picker_notice = None;
                // `c` is a guaranteed ASCII digit, so `to_digit` never returns None.
                let n = c.to_digit(10).unwrap_or(0) as usize;
                if n >= 1 && n <= self.picker_items.len() {
                    self.picker_selected = n - 1;
                }
                Action::None
            }
            KeyCode::Enter => {
                // Fail-open: a stale `picker_selected` past a now-shorter list must
                // never index-panic — just no-op the Enter.
                let Some(chosen) = self.picker_items.get(self.picker_selected).cloned() else {
                    return Action::None;
                };
                // Step 1 - language: set it, then advance to the base picker.
                if let Some(lang) = chosen.lang {
                    self.lang = lang;
                    umadev_i18n::set_lang(lang);
                    self.config.lang = Some(lang.code().to_string());
                    // Surface a persist failure (read-only HOME / full disk) so the
                    // choice doesn't silently revert on the next launch while the UI
                    // implied it stuck. `goto_picker_step` clears `picker_notice`,
                    // so set it AFTER advancing — it then shows on the base picker.
                    let save_err = crate::config::save_to(&self.config, &self.config_path).err();
                    self.goto_picker_step(PickerStep::BaseCli);
                    if let Some(e) = save_err {
                        self.picker_notice = Some(umadev_i18n::tf(
                            self.lang,
                            "config.save_failed_note",
                            &[&e.to_string()],
                        ));
                    }
                    return Action::None;
                }
                // A base CLI must be installed AND logged in before we commit to it
                // (gap G10): an honest three-state block so the user never picks a
                // base that fails mid-run. The matching command (login / install) is
                // surfaced inline so the fix is one copy-paste away.
                if chosen.backend_id.is_some() {
                    match chosen.auth {
                        AuthMark::NotInstalled => {
                            let cmd = if chosen.install_cmd.is_empty() {
                                chosen.detail.clone()
                            } else {
                                chosen.install_cmd.clone()
                            };
                            self.picker_notice = Some(umadev_i18n::tf(
                                self.lang,
                                "picker.block.not_installed",
                                &[&chosen.label, &cmd],
                            ));
                            return Action::None;
                        }
                        AuthMark::NotLoggedIn => {
                            let cmd = if chosen.login_cmd.is_empty() {
                                chosen.detail.clone()
                            } else {
                                chosen.login_cmd.clone()
                            };
                            self.picker_notice = Some(umadev_i18n::tf(
                                self.lang,
                                "picker.block.not_logged_in",
                                &[&chosen.label, &cmd],
                            ));
                            return Action::None;
                        }
                        // Unknown that hasn't even been confirmed installed (a base
                        // not yet probed: `ready=false`, no auth signal) stays blocked
                        // with the generic "unavailable" message — exactly the old
                        // behaviour. A confirmed-installed base whose LOGIN we couldn't
                        // verify (`ready=true` legacy, or an Unknown auth on an
                        // installed base) is allowed through conservatively (the picker
                        // footer already says login is unverified).
                        AuthMark::Unknown if !chosen.ready => {
                            self.picker_notice = Some(umadev_i18n::tf(
                                self.lang,
                                "picker.unavailable",
                                &[&chosen.label, &chosen.detail],
                            ));
                            return Action::None;
                        }
                        // LoggedIn / installed-Unknown → commit.
                        AuthMark::LoggedIn | AuthMark::Unknown => {}
                    }
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
        // The `@`-file-mention popover and the `/` slash palette are mutually
        // exclusive: when an `@`-token is under the cursor the mention popover
        // owns ↑↓/Tab/Enter/Esc, and the slash palette is suppressed (it can only
        // co-fire on a line like `/run … @src`). Otherwise the palette behaves
        // exactly as before — no regression to the `/` path.
        let has_mention = !self.mention_matches().is_empty();
        let has_palette = !has_mention && !self.palette_matches().is_empty();
        let shift = mods.contains(crossterm::event::KeyModifiers::SHIFT);
        let ctrl = mods.contains(crossterm::event::KeyModifiers::CONTROL);
        let alt = mods.contains(crossterm::event::KeyModifiers::ALT);
        // Ctrl+Alt half-page scroll keys are matched on the EXACT modifier set
        // (CONTROL | ALT, nothing else) so they never collide with a bare
        // CONTROL editing/shell key (Ctrl-U clears the line, Ctrl-D is EOF).
        let ctrl_alt = ctrl && alt && !shift;

        match key {
            // ---- @-mention popover: Esc closes it WITHOUT inserting ----
            // Higher precedence than the interrupt / quit-confirm Esc below, so a
            // user dismissing the file typeahead never accidentally arms a quit.
            KeyCode::Esc if has_mention => {
                self.dismiss_mention();
                Action::None
            }

            // ---- exit handling ----
            KeyCode::Esc => {
                // Running → Esc INTERRUPTS (like Claude Code), but require a
                // DELIBERATE double-press so a stray keypress can't nuke a long
                // build: the first Esc ARMS (the indicator shows "再按 Esc 中断"),
                // a second Esc within the window actually cancels. It never quits
                // the app while a run is in flight.
                if self.is_pipeline_active() || self.agentic_in_flight {
                    if self.interrupt_armed() {
                        self.interrupt_armed_at = None;
                        return Action::Cancel;
                    }
                    self.interrupt_armed_at = Some(std::time::Instant::now());
                    return Action::None;
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
            // Word-wise motion (Ctrl/Alt+←/→) must precede the bare char motion.
            KeyCode::Left if ctrl || alt => {
                self.move_word_left();
                Action::None
            }
            KeyCode::Right if ctrl || alt => {
                self.move_word_right();
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

            // ---- @-mention navigation (only when the file popover is open) ----
            // Owns ↑↓/Tab while an `@`-token is under the cursor — placed BEFORE
            // the palette + history arms so it wins (and `has_palette` is already
            // false whenever `has_mention` is true, keeping the two exclusive).
            KeyCode::Up if has_mention => {
                self.cycle_mention(-1);
                Action::None
            }
            KeyCode::Down if has_mention => {
                self.cycle_mention(1);
                Action::None
            }
            KeyCode::Tab if has_mention => {
                self.accept_mention();
                Action::None
            }

            // ---- palette navigation (only when /-prefix has matches) ----
            // EXCEPT while paging input history: once a recall surfaces a past
            // `/command` (input now starts with `/`, so `has_palette` flips true),
            // ↑/↓ must KEEP recalling, not hijack into palette nav — otherwise the
            // user gets stuck on the first recalled slash command and ↑ "stops
            // working" (reported). A fresh `/` the user is typing (idx == None) still
            // drives the palette.
            KeyCode::Up if has_palette && self.input_history_idx.is_none() => {
                self.cycle_palette(-1);
                Action::None
            }
            KeyCode::Down if has_palette && self.input_history_idx.is_none() => {
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

            // ---- multi-line caret nav, then input history recall ----
            // Claude Code parity: a bare ↑ inside a multi-line / wrapped prompt
            // moves the caret UP one visual row (preserving the column) instead of
            // wiping the draft with a history recall. Only when the caret is
            // already on the FIRST visual row does ↑ fall through to history. This
            // is what stops the "↑ in a multi-line prompt destroys my draft" bug.
            KeyCode::Up if !has_palette || self.input_history_idx.is_some() => {
                if self.caret_move_up_wrapped() {
                    return Action::None;
                }
                // Caret is on the first row — recall history if we have any to
                // recall (an empty box, or we're already paging history). An
                // un-recalled, non-empty single-line draft is left alone so a stray
                // ↑ at the start of a fresh draft can't clobber it.
                if self.input.is_empty() || self.input_history_idx.is_some() {
                    self.input_history_back();
                }
                Action::None
            }
            // ↓ mirrors ↑: move the caret DOWN a visual row first; only recall
            // newer history (or restore the draft) when already on the last row.
            KeyCode::Down if !has_palette || self.input_history_idx.is_some() => {
                if self.caret_move_down_wrapped() {
                    return Action::None;
                }
                if self.input_history_idx.is_some() {
                    self.input_history_forward();
                }
                Action::None
            }
            // ---- enter: accept the highlighted @-mention (popover open) ----
            // Wins over submit so Enter on the file typeahead inserts the path
            // instead of sending the half-typed `@partial`. Shift+Enter still
            // falls through to insert a literal newline.
            KeyCode::Enter if has_mention && !shift => {
                self.accept_mention();
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
                        let is_exact = matches.iter().any(|p| p.verb == typed)
                            || umadev_host::driver_for(&typed).is_some();
                        if !is_exact {
                            let sel = self.palette_selected.min(matches.len() - 1);
                            self.input = format!("/{}", matches[sel].verb);
                            self.input_cursor = self.input_len();
                        }
                    }
                }
                // Rewrite any `[图片 N]` chip → `@<abs-path>` BEFORE clearing the
                // input (clear drops the attachment list), so the base receives a
                // path it can open as an image.
                let raw = self.expand_attachments(self.input.trim());
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
                    // Seal any half-streamed reply first so the partial text is
                    // marked incomplete, not read as the whole answer.
                    self.seal_interrupted_stream();
                    self.thinking = false;
                    self.thinking_started = None;
                    self.queued_chat.clear();
                    self.clear_input();
                    self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.cancelled"));
                    self.refresh_status();
                    return Action::None;
                }
                // Idle: Ctrl+C clears a half-typed input but NEVER quits the app —
                // it's universal muscle-memory for COPY, so an accidental Ctrl+C must
                // not drop the session (user-reported). Quit deliberately with /quit
                // (or /exit), Ctrl+D, or a double-Esc.
                if self.input.is_empty() {
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "quit.use_command"),
                    );
                } else {
                    self.clear_input();
                }
                Action::None
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => {
                self.should_quit = true;
                Action::Quit
            }
            // Ctrl+O: the GLOBAL "expand everything" toggle (Claude-Code
            // convention). Flips `verbose` so EVERY collapsed tool result / diff
            // card / long reply reveals (or re-hides) at once — the single reveal
            // gesture that reaches OLDER folded rows, which Ctrl+R cannot (it only
            // toggles the most-recent one). The render reads `verbose`, so the
            // next frame shows the change.
            KeyCode::Char('o') if ctrl => {
                self.verbose = !self.verbose;
                Action::None
            }
            // Ctrl+R toggles the fold of the most recent collapsible row (a long
            // Host/UmaDev text wall, or a finished tool row's result) — the
            // "fold just the latest" lever, secondary to the global Ctrl+O. No-op
            // when nothing is foldable.
            KeyCode::Char('r') if ctrl => {
                self.toggle_last_collapsible();
                Action::None
            }
            // Ctrl+L: force a full repaint (shell / Claude-Code convention). The
            // escape hatch that recovers from any accumulated incremental-diff
            // desync — stale cells, leftover left-margin prefixes, bled long
            // lines — without losing the conversation. The event loop owns the
            // terminal, so it issues the real `terminal.clear()`.
            KeyCode::Char('l') if ctrl => Action::ForceRedraw,

            // ---- printable char ----
            // Guard on `!ctrl && !alt`: without it, every Ctrl/Alt-modified letter
            // that ISN'T intercepted by a specific arm above (Ctrl+P, Ctrl+X,
            // Ctrl+V, Ctrl+G, Alt+f, Alt+b, …) fell through here and typed its bare
            // letter into the input (user-visible: Ctrl+P inserted "p"). Shift is
            // allowed through — Shift+letter is a normal uppercase char. A modified
            // combo with no handler now lands on the `_` no-op below.
            KeyCode::Char(c) if !ctrl && !alt => {
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
        // A brain-driven turn is still in flight (`thinking`). Firing a second one
        // now would drive the SAME base `session_id` in two subprocesses at once →
        // interleaved / out-of-order replies and a scrambled memory. Park this
        // turn instead; the event loop fires it the moment the current turn lands
        // (a clean / failed terminal outcome both drain the queue). (A gate is
        // never open while `thinking`, so this check sits ahead of gate handling.)
        if self.thinking {
            // Park it WITHOUT recording into conversation memory yet. Recording at
            // submit time left a dangling "user said X" with no assistant reply in
            // memory whenever the user then interrupted (Ctrl-C clears `queued_chat`
            // but the premature record stayed) — a scrambled turn order the base
            // would later see. The turn is recorded only when it actually FIRES
            // (see `take_next_queued_chat`), so an interrupted queue leaves memory
            // clean. Tell the user it is queued — NOT the pipeline `run.queued`
            // text (there is no gate here, this is a plain conversational turn).
            self.queued_chat.push_back(text);
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "chat.queued"));
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
                match self.append_clarify_answer(&text) {
                    Ok(()) => self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::t(self.lang, "gate.clarify_recorded").to_string(),
                    ),
                    // Persist failed — don't claim "recorded"; the resume path
                    // would lose this answer. Tell the user the write failed.
                    Err(e) => self.push(
                        ChatRole::System,
                        umadev_i18n::tf(self.lang, "gate.clarify_write_failed", &[&e.to_string()]),
                    ),
                }
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
        } else if self.run_started && (self.finished || self.aborted) {
            // A SETTLED run (delivered, or aborted/hard-stopped) is no longer
            // active — free text is a fresh chat turn, routed to the base. P1-G:
            // the old condition only matched `finished`, so an ABORTED run (which
            // keeps `run_started = true`, `finished = false`) fell through to the
            // `else` and queued the message into `queued_steer` — a queue that
            // never drains after an abort (no more phase/gate gaps come), so the
            // input was silently swallowed (dead input). Treating aborted as
            // "settled, route to chat" matches `is_pipeline_active()` exactly.
            self.record_user_turn(&text);
            self.thinking = true; // animated "thinking…" until the base replies
                                  // NB: we deliberately do NOT clear `aborted`/`finished` here — the
                                  // settled run's terminal state persists until a worker `run` decision
                                  // resets it (see `plain_text_after_delivery_routes_to_worker`). The
                                  // input placeholder / bottom hint already prioritise the LIVE `thinking`
                                  // state over a stale `aborted`/`finished` (ui.rs), so the placeholder
                                  // reads "running" while this chat turn streams — without disturbing the
                                  // delivered/aborted bookkeeping the routing reset depends on.
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
        // Wave 5 / G11: mirror the live buffer to disk so a restart reopens it.
        self.persist_chat();
    }

    #[cfg(test)]
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
        // Reality-anchor a pure-chat reply: if the base RECITES an edit ("I
        // changed / implemented / added …") in a chat turn — no pipeline run, no
        // agentic tool calls, so nothing actually touched the tree — append a
        // lightweight advisory telling the user to verify against real files /
        // `git status`. Non-blocking; a false positive only adds one note.
        let claims = crate::claims_code_changes(&reply);
        self.conversation.push(umadev_runtime::Message {
            role: "assistant".to_string(),
            content: reply,
        });
        if claims {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "chat.claims_unverified").to_string(),
            );
        }
        self.trim_conversation();
    }

    /// The route ended without a usable reply (base init failed, an empty
    /// reply, or a hard error). This is a TERMINAL route outcome, so — like
    /// `record_agentic_done` — it stops the
    /// "thinking…" status; otherwise the animation would spin forever on a
    /// route that already failed. The human-readable reason is surfaced as a
    /// System note. Also clears `agentic_in_flight`: a failed agentic execution
    /// call flows through here, so this is its terminal cleanup too.
    pub(crate) fn record_route_failed(&mut self, note: String) {
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        // P5c: close any open reasoning block on a failed/aborted route.
        self.collapse_thinking_block();
        // P5a: a failed/aborted route ends any in-flight stream — drop its cache.
        self.stream_text_active = false;
        self.reset_stream_md_cache();
        // A failed director run does NOT hand a session back to chat (there is no
        // settled build session to continue) — just clear the in-flight marker.
        self.director_run_in_flight = false;
        self.refresh_status();
        self.push(ChatRole::System, note);
    }

    /// An agentic streaming turn finished cleanly. The body ALREADY streamed live
    /// into the transcript (via `WorkerStream`), so we do NOT re-render it — we
    /// only record it as the assistant turn for chat-memory continuity and clear
    /// the waiting state. A TERMINAL agentic outcome, mirroring the chat-reply
    /// recorder but without the duplicate render.
    ///
    /// `director_build` is carried back on the terminal `RouteDecision::AgenticDone`
    /// (not read from `director_run_in_flight`): the chat surface now classifies the
    /// turn INSIDE the spawned task — after the slow brain-router consult — so the
    /// event loop no longer knows the class before dispatch. The build-ness rides
    /// the terminal decision instead, and drives the Wave-5 session hand-back here.
    pub(crate) fn record_agentic_done(
        &mut self,
        reply: String,
        director_build: bool,
        base_session_id: Option<String>,
    ) {
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.stream_text_active = false;
        self.stream_tool_batch = None;
        // Capture the base's OWN resumable session id off this host chat turn so the
        // SAVED chat (persisted below + on every later turn) points at the real base
        // conversation a relaunch can `--resume`. claude's pinned `--session-id` /
        // codex's `thread.id` ride back here on the terminal decision; opencode /
        // offline carry `None`. Fail-open: a `None` / empty id leaves the prior value
        // (degrades to today's fresh-session + transcript-replay behavior).
        if let Some(id) = base_session_id {
            if !id.trim().is_empty() {
                self.chat_session_id = Some(id);
                self.host_chat_session_active = true;
            }
        }
        // P5c: a turn that ends still inside a reasoning block collapses it now.
        self.collapse_thinking_block();
        // P5a: the streamed turn is settled — drop the stable-prefix cache so the
        // final, complete body renders through one clean whole-body pass (the
        // guaranteed-consistent path) and the NEXT stream starts fresh.
        self.reset_stream_md_cache();
        // Wave 5 deliverable 2 — unify chat ↔ /run memory. A finished director
        // build hands its session back to chat: the NEXT chat turn resumes the
        // base's most-recent session in this dir (`--continue`) so "why did you
        // build it that way?" continues the SAME session that did the build, with
        // full context — instead of a disjoint cold chat session. Only fires after a
        // real director build (a plain chat / explain / quick-edit turn carries
        // `director_build = false`). Fail-open: if the base can't resume, it starts
        // fresh. The in-flight marker is always cleared (it was only ever an
        // aliveness/UI hint once the class moved into the task).
        self.director_run_in_flight = false;
        if director_build {
            self.run_session_handed_to_chat = true;
        }
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
        // Wave 5 / G11: persist after the assistant turn lands so the saved chat
        // holds complete user→assistant exchanges.
        self.persist_chat();
    }

    /// Pop the oldest chat turn parked by [`submit_text`] while a route was in
    /// flight, if any, and record it into conversation memory AT THIS MOMENT — the
    /// instant it actually fires — so the base sees user turns in true send order
    /// with no dangling "user said X" left behind by an interrupted queue. The
    /// event loop fires it as the NEXT route only after the current route result
    /// has landed, keeping same-session routing strictly serial (never two base
    /// subprocesses resuming one `session_id` at once).
    pub(crate) fn take_next_queued_chat(&mut self) -> Option<String> {
        let text = self.queued_chat.pop_front()?;
        self.record_user_turn(&text);
        Some(text)
    }

    /// Number of turns currently waiting to be sent — the chat-routing queue
    /// plus a pending pipeline steer. Drives the persistent "queued N" chip so
    /// the user can always see that parked input has NOT been lost, even after
    /// the one-off System note scrolls away.
    #[must_use]
    pub fn queued_count(&self) -> usize {
        self.queued_chat.len() + self.queued_steer.len()
    }

    /// A clone of the conversation memory to hand to a routed turn (Wave 5 / G11).
    /// The receiver (`drive_agentic_stream`) bounds it to a token budget before
    /// threading it into the request, so this is a plain clone of the live buffer.
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
        // P5c: a reset ends any open reasoning block (collapse its placeholder).
        self.collapse_thinking_block();
        // Drop any not-yet-fired queued steers so they can't bleed into a later
        // run and fire at the wrong gate.
        self.queued_steer.clear();
        self.pending_steer = None;
        // A new run owns a fresh plan + review panel — the previous run's
        // checklist / verdicts must not bleed into it.
        self.clear_live_panels();
    }

    /// Drop the live plan checklist + team-review panel state back to empty, so
    /// the panel region disappears (it only renders when a plan / review is
    /// live). Called when a run is reset for a fresh start AND when a run reaches
    /// a terminal state (finished / aborted) — a settled run must not keep a
    /// stale "计划 N/M · 进行中" / half-finished verdict list hanging on screen.
    ///
    /// This is the SINGLE run-terminal chokepoint: every abort source
    /// (`mark_block_aborted` ← idle settle / base error, `cancel_run` ←
    /// `reset_for_new_run` ← user Cancel) AND the clean-finish path
    /// (`finalize_live_panels`) converge here, so settling the in-flight
    /// tool-call rows from one place can't be "fixed in one path, missed in
    /// another." Fail-open: pure state flips, never panics.
    fn clear_live_panels(&mut self) {
        self.plan_steps.clear();
        self.plan_collapsed = false;
        self.critic_verdicts.clear();
        self.critics_collapsed = false;
        self.critic_round_open = false;
        // A terminal/reset transition must also settle any tool-call row still
        // showing a spinner: on abort/cancel the base's matching ToolResult
        // never arrives, so without this a stack of in-flight rows (TaskCreate /
        // Agent / Bash / Read / TaskUpdate) keeps spinning forever after the run
        // is over (user-reported). Done here so it covers EVERY abort source.
        self.settle_dangling_tool_rows();
    }

    /// Settle every still-in-progress tool-call row in the transcript to the
    /// terminal [`ToolStatus::Aborted`] state. Only `Queued`/`Running` rows are
    /// touched — a genuinely finished `Ok`/`Fail` row keeps its REAL terminal
    /// status (never downgraded to a fake success/abort). Fail-open: a pure
    /// status flip over `self.history`, never panics.
    fn settle_dangling_tool_rows(&mut self) {
        for msg in &mut self.history {
            if let MessageBody::Tool(t) = &mut msg.kind {
                if !t.status.is_terminal() {
                    t.status = ToolStatus::Aborted;
                }
            }
        }
        // The in-flight low-signal merge target (if any) is now settled — the
        // next tool starts a fresh batch row rather than folding into (and
        // re-animating) an already-aborted row.
        self.stream_tool_batch = None;
    }

    /// Settle the live panels at a CLEAN terminal (a finished delivery): if the
    /// last round carried any verdicts, fold them into a one-line accept/blocking
    /// tally in the transcript (the per-seat detail is already in scrollback),
    /// then [`clear_live_panels`](Self::clear_live_panels) so the live region
    /// stops rendering a stale list. Fail-open: no verdicts → no summary line.
    fn finalize_live_panels(&mut self) {
        if !self.critic_verdicts.is_empty() {
            let accepts = self.critic_verdicts.iter().filter(|c| c.accepts).count();
            let blocking = self.critic_verdicts.len() - accepts;
            self.push(
                ChatRole::System,
                umadev_i18n::tf(
                    self.lang,
                    "plan.review.note.summary",
                    &[&accepts.to_string(), &blocking.to_string()],
                ),
            );
        }
        self.clear_live_panels();
    }

    /// Reset run state after `/cancel` aborts the in-flight pipeline task, and
    /// tell the user we're back at the prompt (workflow state on disk is intact,
    /// so a later run can resume from the last gate).
    /// Seal an in-flight streamed reply on interrupt: if the base was mid-stream
    /// into a Host bubble when the user cancelled, append an `[interrupted]`
    /// marker so the user knows the reply is INCOMPLETE rather than reading a
    /// half-sentence as if it were the whole answer. No-op when nothing was
    /// streaming. Mirrors Claude Code marking an interrupted turn.
    /// P5a: forget the stable-prefix streaming markdown cache. Called whenever a
    /// streamed turn ends or the conversation context breaks, so the next frame
    /// renders cleanly from scratch. Fail-open: a poisoned/borrowed cell is left
    /// as-is (the cache's own precondition check discards a stale entry anyway).
    pub(crate) fn reset_stream_md_cache(&self) {
        if let Ok(mut c) = self.stream_md_cache.try_borrow_mut() {
            *c = crate::ui::StreamMarkdownCache::default();
        }
    }

    /// Open (once) the live `[thinking]` reasoning block: push a single System
    /// placeholder row (`[thinking] 正在思考…`), default-collapsed, and record its
    /// index + start time. A no-op when a block is already open — so a burst of
    /// `Thinking` / `ThinkingDelta` events never stacks a wall of rows. Shared by
    /// both the content-less `Thinking` pulse and the text-bearing `ThinkingDelta`.
    fn open_thinking_block(&mut self) {
        if self.thinking_block_idx.is_some() {
            return;
        }
        self.thinking_block_start = Some(std::time::Instant::now());
        self.push(
            ChatRole::System,
            format!(
                "{THINKING_PLACEHOLDER_TAG} {}",
                umadev_i18n::t(self.lang, "status.thinking")
            ),
        );
        let idx = self.history.len() - 1;
        self.thinking_block_idx = Some(idx);
        // Default collapsed: any reasoning text accumulated into this block hides
        // behind the fold until the user expands it (Ctrl+O / Ctrl+R).
        if let Some(msg) = self.history.get_mut(idx) {
            msg.collapsed = true;
        }
    }

    /// P5c: close an open reasoning block when real content arrives. Rewrites the
    /// live `[thinking]` placeholder's HEADER line to a one-line summary (`正在思考…
    /// · 4.2s`, timed from the block start). When the block accumulated reasoning
    /// text (extended thinking), the reasoning is PRESERVED below the header and the
    /// row stays a default-collapsed foldable block the user can expand (Ctrl+O);
    /// with no reasoning it degrades to the legacy plain summary (the tag is dropped
    /// — nothing to expand). No-op when no block is open.
    ///
    /// Fail-open: the stored index is re-validated against the row's content
    /// (still a System `[thinking]` row) before any rewrite, so a rolled-off or
    /// shifted index can never clobber an unrelated message; a missing timestamp
    /// degrades to a plain completion marker with no seconds.
    fn collapse_thinking_block(&mut self) {
        let Some(idx) = self.thinking_block_idx.take() else {
            return;
        };
        let start = self.thinking_block_start.take();
        let label = umadev_i18n::t(self.lang, "status.thinking");
        let summary = match start {
            // One decimal place of seconds — `思考 · 4.2s`.
            Some(t) => format!("{label} · {:.1}s", t.elapsed().as_secs_f64()),
            // Fail-open: no timing → a plain completion marker, no seconds.
            None => format!("{label} \u{2713}"),
        };
        // Re-validate: only rewrite if the row is still the System placeholder we
        // pushed (its content starts with the marker tag). Otherwise leave it be.
        let Some(msg) = self.history.get_mut(idx) else {
            return;
        };
        let is_placeholder = msg.role == ChatRole::System
            && msg
                .body()
                .trim_start()
                .starts_with(THINKING_PLACEHOLDER_TAG);
        if !is_placeholder {
            return;
        }
        // Preserve any accumulated reasoning (the lines BELOW the header): rewrite
        // only the header to the timed summary + keep the block foldable. With NO
        // reasoning, drop the tag and leave a plain one-line summary (legacy path).
        let mut has_reasoning = false;
        if let Some(text) = msg.text_mut() {
            // Own the reasoning first so the `*text = …` mutable write doesn't alias
            // the `split_once` immutable borrow.
            let reasoning = text
                .split_once('\n')
                .map(|(_, r)| r.to_string())
                .filter(|r| !r.trim().is_empty());
            if let Some(r) = reasoning {
                *text = format!("{THINKING_PLACEHOLDER_TAG} {summary}\n{r}");
                has_reasoning = true;
            } else {
                *text = summary;
            }
        }
        if has_reasoning {
            msg.collapsed = true;
        }
    }

    pub(crate) fn seal_interrupted_stream(&mut self) {
        if !self.stream_text_active {
            return;
        }
        self.stream_text_active = false;
        // P5a: the streamed reply was cut off — drop its cache so the sealed
        // body (with the `[interrupted]` marker) renders as one whole pass.
        self.reset_stream_md_cache();
        let marker = umadev_i18n::t(self.lang, "chat.interrupted");
        if let Some(last) = self.history.back_mut() {
            if last.role == ChatRole::Host {
                if let Some(text) = last.text_mut() {
                    if !text.ends_with(marker) {
                        text.push_str(marker);
                    }
                }
            }
        }
    }

    /// Interrupt the active run / in-flight turn: seal any half-streamed reply,
    /// stop the spinner, clear the in-flight + queued state, and post a cancelled
    /// note. The canonical Esc/Ctrl-C handler (see `lib.rs`).
    pub fn cancel_run(&mut self) {
        // Seal any half-streamed reply BEFORE the reset clears the stream flag, so
        // the user sees the partial answer is incomplete (not the whole reply).
        self.seal_interrupted_stream();
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
        // A cancelled director run hands nothing back to chat.
        self.director_run_in_flight = false;
        // Drop chat turns parked behind the in-flight route so they can't fire
        // into a freshly-reset state.
        self.queued_chat.clear();
        self.pending_quit_confirm = false;
        // The aborted task has now fully wound down — leave the "stopping…" state.
        self.cancelling = false;
        self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.cancelled"));
    }

    /// Enter the **stopping** state the instant Esc/Ctrl-C cancels an in-flight
    /// run/turn: keep the spinner alive and post a "stopping…" line so the UI
    /// reads as in-progress while the aborted task winds down OFF the render path
    /// (the actual drain + reset happens in the event loop's drain branch, which
    /// calls [`Self::cancel_run`] once the task has released its session). This is
    /// the public entry the loop calls so [`Self::push`] stays private.
    pub fn begin_cancelling(&mut self) {
        self.cancelling = true;
        self.thinking = true;
        self.thinking_started = Some(std::time::Instant::now());
        self.push(
            ChatRole::System,
            umadev_i18n::t(self.lang, "status.stopping"),
        );
    }

    #[cfg(test)]
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
        // Resolve aliases CENTRALLY against the registry first, so the dispatch
        // arms below only ever key on canonical names (e.g. `/q`, `/exit` →
        // `quit`; `/abort` → `cancel`; `/语言` → `lang`). An unknown verb (a
        // dynamic per-backend id, or a typo) passes through unchanged to the `_`
        // fallback. The `commands_and_dispatch_are_in_lockstep` test parses the
        // arm literals between the COMMAND-DISPATCH sentinels and locks them
        // against [`COMMANDS`](Self::COMMANDS) so no arm can drift from the
        // registry that the palette + help also read.
        let canonical = Self::resolve_command(&verb).map_or(verb.as_str(), |c| c.name);
        // COMMAND-DISPATCH-START
        let action = match canonical {
            "help" => {
                self.show_help = true;
                self.help_scroll = 0;
                Action::None
            }
            "quit" => {
                self.should_quit = true;
                Action::Quit
            }
            "clear" => {
                self.history.clear();
                self.conversation.clear();
                self.transcript_scroll.set(0);
                self.transcript_prev_hidden.set(0);
                // P5a: a cleared transcript invalidates the streaming cache.
                self.reset_stream_md_cache();
                // P5c: a cleared history drops any open reasoning-block index.
                self.thinking_block_idx = None;
                self.thinking_block_start = None;
                // A cleared transcript also clears the live plan / review panel +
                // the last-intent chip — nothing from the prior conversation lingers.
                self.plan_steps.clear();
                self.critic_verdicts.clear();
                self.last_intent_class = None;
                // A cleared transcript means the base should start a fresh
                // session on the next turn, not resume the old one.
                self.host_chat_session_active = false;
                self.chat_session_id = None;
                self.run_session_handed_to_chat = false;
                // The RESIDENT chat session held by the event loop predates the
                // cleared conversation — flag it for close so the next turn opens a
                // fresh one instead of carrying the old dialogue's live process.
                self.chat_session_dirty = true;
                // Wave 5 / G11: `/clear` starts a FRESH persistent chat — mint a new
                // id so the prior saved chat stays on disk (resumable via /resume)
                // and the next turn persists under the new id.
                self.chat_id = new_chat_session_id();
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "slash.history_cleared"),
                );
                Action::None
            }
            "claude" => self.slash_backend(Some("claude-code")),
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
                } else if !self.run_started
                    && !self.finished
                    && umadev_agent::has_resumable_run(&self.project_root)
                {
                    // Fresh session (no in-memory gate, no in-flight run) but the
                    // previous `/run` left a resumable director-loop run on disk —
                    // RE-ATTACH to the saved plan and drive only the remaining steps
                    // rather than telling the user to restart the whole pipeline. The
                    // requirement is read back from `.umadev/workflow-state.json` when
                    // the in-memory one is empty (a reopened TUI has none).
                    let req = self.resume_run_requirement();
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::t(self.lang, "continue.resuming"),
                    );
                    Action::ResumeRun(req)
                } else if !self.run_started
                    && !self.finished
                    && self.host_chat_session_active
                    && self.chat_session_id.is_some()
                {
                    // No director-run plan on disk, but the prior progress was a
                    // CHAT-driven agentic loop (the base built reactively and
                    // persisted only its OWN session — no plan.json /
                    // workflow-state.json, which is all `has_resumable_run` reads).
                    // Resume the CONVERSATION: a routed turn re-attaches the same base
                    // session (`continue_session = host_chat_session_active`), so the
                    // base picks up its full context where it left off, instead of
                    // wrongly telling the user "no pipeline started".
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::t(self.lang, "continue.resuming_chat"),
                    );
                    Action::Route(umadev_i18n::t(self.lang, "continue.chat_directive").to_string())
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
            "runs" => {
                self.open_runs_overlay();
                Action::None
            }
            "history" => {
                self.open_history_overlay();
                Action::None
            }
            // Wave 5 / G11: conversation memory surfaces.
            "sessions" => self.slash_sessions(),
            "resume" => self.slash_resume(rest),
            "compact" => self.slash_compact(),
            "manual" => self.slash_set_review_mode(false),
            "auto" => self.slash_set_review_mode(true),
            "mode" => self.slash_mode(rest),
            "model" => self.slash_model(rest),
            "lang" => self.slash_lang(rest),
            "setup" | "guide" => self.slash_setup(),
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
            "redraw" => {
                // Force a full repaint to recover from any accumulated render
                // desync (stale cells / bled long lines). The event loop owns the
                // terminal and performs the actual `terminal.clear()`.
                self.push(ChatRole::System, umadev_i18n::t(self.lang, "slash.redraw"));
                Action::ForceRedraw
            }
            "bug" => self.slash_bug(),
            "design" => self.slash_design(rest),
            "template" => self.slash_template(rest),
            "run" => self.slash_run(rest),
            "goal" => self.slash_goal(rest),
            "quick" => self.slash_quick(rest),
            "plan" => self.slash_plan(rest),
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
            "pitfalls" => {
                let body = umadev_agent::pitfall_overview(&self.project_root);
                self.overlay = Some(Overlay::from_body(
                    umadev_i18n::t(self.lang, "pitfalls.overlay_title"),
                    &body,
                ));
                Action::None
            }
            "lessons" => self.slash_lessons(),
            "team" => self.slash_team(rest),
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
            "cancel" => {
                // P1-H: `/cancel` must also abort an in-flight AGENTIC round (the
                // base inspecting/editing the repo outside the full pipeline).
                // `agentic_in_flight` is true but `is_pipeline_active()` is false in
                // that state, so the old pipeline-only check left `/cancel` unable to
                // stop a streaming agentic subprocess — only Ctrl-C could. Mirror the
                // Ctrl-C path, which already routes both to `Action::Cancel` (the
                // event loop aborts `run_task`; `cancel_run` clears the flags).
                if self.is_pipeline_active() || self.agentic_in_flight {
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
            "checkpoint" => self.slash_checkpoint(rest),
            "rewind" => self.slash_rewind(rest),
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
            // COMMAND-DISPATCH-END
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
        if let Some(c) = Self::COMMANDS.iter().find(|c| c.name.starts_with(typed)) {
            return Some(c.name);
        }
        // Also consider the dynamic backend verbs (goose, amp, junie, …).
        if let Some((verb, _)) = backend_palette_verbs()
            .iter()
            .find(|(v, _)| v.starts_with(typed))
        {
            return Some(verb);
        }
        // Otherwise Levenshtein ≤ 2 against known verbs (registry + dynamic).
        let typed_lower = typed.to_ascii_lowercase();
        let (mut best, mut best_dist) = (None, usize::MAX);
        let all_verbs = Self::COMMANDS
            .iter()
            .map(|c| c.name)
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

    /// `/sessions` — list this project's persisted chats (Wave 5 / G11), most
    /// recent first, so the user can pick one to `/resume`. Fail-open: no saved
    /// chats just says so.
    fn slash_sessions(&mut self) -> Action {
        let chats = self.list_chats();
        if chats.is_empty() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "sessions.empty"),
            );
            return Action::None;
        }
        let mut body = String::new();
        for (id, updated, turns, preview) in &chats {
            // Mark the currently-open chat so the user knows where they are.
            let here = if *id == self.chat_id { "* " } else { "  " };
            body.push_str(&format!("{here}{id}  ({updated}, {turns})  {preview}\n"));
        }
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "sessions.header", &[&body]),
        );
        Action::None
    }

    /// `/resume <id>` — load a saved chat into the live buffer (Wave 5 / G11) and
    /// point the base at its own session for that chat. Fail-open: a missing id or
    /// a corrupt file leaves the current conversation untouched and explains why.
    fn slash_resume(&mut self, arg: &str) -> Action {
        let id = arg.trim();
        if id.is_empty() {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "resume.usage"));
            return Action::None;
        }
        if !self.load_chat(id) {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "resume.not_found", &[id]),
            );
            return Action::None;
        }
        // The transcript is back. `load_chat` already restored the BASE session id
        // (`chat_session_id`, the resumable pointer the base actually created) and
        // flagged `host_chat_session_active` — so a host CLI resumes ITS OWN
        // conversation (claude `--resume <base_id>` / codex `thread/resume`), NOT the
        // chat FILE id (a DIFFERENT id the base never created, which is the bug this
        // fixes). The RESIDENT session the event loop is holding belongs to the chat
        // we're LEAVING, so flag it dirty: the loop closes it and re-opens the
        // resident session against the resumed base id. Clear any pending run-handoff
        // (we explicitly chose a chat).
        self.chat_session_dirty = true;
        self.run_session_handed_to_chat = false;
        let n = self.conversation.len();
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "resume.done", &[id, &n.to_string()]),
        );
        Action::None
    }

    /// `/compact` — token-budgeted summarize-and-fold of the conversation (Wave 5
    /// / G11), replacing the blunt FIFO-drop-at-16 with a deterministic, fail-open
    /// fold: collapse the older half of the transcript into ONE compact summary
    /// message, keeping the recent tail verbatim, so long chats stay within budget
    /// WITHOUT silently losing the whole early context. Deterministic (no brain
    /// call, so it never blocks or depends on a base); the base still sees the
    /// recent turns verbatim + a labelled digest of what came before.
    fn slash_compact(&mut self) -> Action {
        let before = self.conversation.len();
        if before <= 4 {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "compact.too_short"),
            );
            return Action::None;
        }
        // Keep the most-recent quarter (at least 4) verbatim; fold the rest.
        let keep = (before / 4).max(4).min(before);
        let split = before - keep;
        let folded: Vec<umadev_runtime::Message> = self.conversation.drain(0..split).collect();
        // Build a compact, role-tagged digest of the folded prefix, capped so the
        // summary itself can't reintroduce the bloat we just removed.
        let mut digest = String::new();
        for m in &folded {
            let line: String = m.content.split_whitespace().collect::<Vec<_>>().join(" ");
            let snippet = match line.char_indices().nth(160) {
                Some((i, _)) => format!("{}…", &line[..i]),
                None => line,
            };
            if !snippet.is_empty() {
                digest.push_str(&format!("- {}: {snippet}\n", m.role));
            }
        }
        let summary = umadev_i18n::tf(
            self.lang,
            "compact.summary",
            &[&folded.len().to_string(), &digest],
        );
        // Prepend the summary as a `user`-role context note (the base treats it as
        // grounding it must honour, like a recap). Then re-bound to the cap.
        self.conversation.insert(
            0,
            umadev_runtime::Message {
                role: "user".to_string(),
                content: summary,
            },
        );
        self.trim_conversation();
        self.persist_chat();
        let after = self.conversation.len();
        self.push(
            ChatRole::System,
            umadev_i18n::tf(
                self.lang,
                "compact.done",
                &[&before.to_string(), &after.to_string()],
            ),
        );
        Action::None
    }

    fn slash_backend(&mut self, backend: Option<&str>) -> Action {
        // P1-I: refuse to switch the base mid-run. A live run (continuous or
        // single-shot) is driving a base session pinned to the CURRENT backend;
        // swapping `self.backend` + persisting it to config now would (a) leave the
        // in-flight run on the old base while the UI/config claim the new one, and
        // (b) make the NEXT resume/continue open a session on a base the run was
        // never built against — a silent backend mismatch. Reject and tell the user
        // to cancel first; the run, its parked session, and config all stay coherent.
        if self.is_pipeline_active() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "backend.busy_no_switch"),
            );
            return Action::None;
        }
        let id = backend.unwrap_or("offline").to_string();
        self.commit_backend(backend.map(str::to_string));
        // The resident chat session is pinned to the OLD base — flag it for close so
        // the next chat turn opens a fresh session on the newly-selected base.
        self.chat_session_dirty = true;
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "backend.switched", &[&id]),
        );
        self.refresh_status();
        Action::BackendChanged
    }

    fn slash_model(&mut self, _arg: &str) -> Action {
        // UmaDev does NOT manage the model. It forwards nothing to the base,
        // which runs whatever it is configured / logged in with — so switching
        // the model here would have no effect. Tell the user where the model
        // actually lives (the base) instead of pretending to change it.
        self.push(
            ChatRole::System,
            umadev_i18n::t(self.lang, "model.unmanaged").to_string(),
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

    /// Resolve the requirement (and slug) for a `/continue` cross-session resume.
    ///
    /// A reopened TUI has an empty in-memory `requirement` / `slug`, so this reads
    /// them back from the persisted `.umadev/workflow-state.json` the previous `/run`
    /// left. The persisted slug is adopted (so branch isolation + the run baseline
    /// stay on the SAME `umadev/<slug>` branch as the original run); the persisted
    /// requirement is returned for the resumed build's firmware / lessons context.
    /// Fail-open: a missing / empty persisted field keeps the in-memory value, so a
    /// resume is never blocked by an unreadable state file.
    fn resume_run_requirement(&mut self) -> String {
        let state = umadev_agent::read_workflow_state(&self.project_root);
        if let Some(s) = &state {
            if self.slug.is_empty() && !s.slug.trim().is_empty() {
                self.slug = s.slug.clone();
            }
        }
        let persisted = state
            .map(|s| s.requirement.trim().to_string())
            .unwrap_or_default();
        if persisted.is_empty() {
            self.requirement.clone()
        } else {
            persisted
        }
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
        // The first token is the optional run SLUG only when it UNAMBIGUOUSLY looks
        // like one: ASCII alnum/-/_ AND carrying a separator (`-`/`_`), e.g.
        // `todo-app`. A natural first word of a requirement ("create", "做一个", "做一个登录页")
        // has no separator (or isn't ASCII), so it stays part of the requirement.
        // Without this, `/run 做一个 登录页` treated "做一个" as a slug → slug_invalid,
        // so ANY multi-word / Chinese requirement was rejected (user-reported).
        let looks_like_slug = |t: &str| {
            !t.is_empty()
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && t.contains(['-', '_'])
        };
        let (slug, req) = match arg.split_once(' ') {
            Some((first, rest)) if !rest.trim().is_empty() && looks_like_slug(first) => {
                (first.to_string(), rest.trim().to_string())
            }
            _ => (String::new(), arg.to_string()),
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

    /// `/goal <objective>` — start a GOAL-DRIVEN director build: drive the borrowed
    /// brain toward `<objective>` until it's met (Claude Code's native persistent
    /// `/goal` mode on a capable base; a "don't stop early" prompt fallback on the
    /// rest — the director loop drives both to completion). It rides the SAME
    /// director-build path as [`Self::slash_run`] (full design / team / knowledge /
    /// evolution + budget), so the only behavioural difference is the goal-mode
    /// framing the event loop forwards. The whole arg IS the objective (no `slug`
    /// prefix parsing — a goal is a sentence, not a project name); empty → a usage
    /// hint. Busy-pipeline + design-suggest + preflight are reused verbatim from
    /// `/run`, so the hardened interaction (streaming / alive / ESC / queue / memory)
    /// is identical.
    fn slash_goal(&mut self, arg: &str) -> Action {
        if self.is_pipeline_active() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "goal.busy_reopen"),
            );
            return Action::None;
        }
        let objective = arg.trim();
        if objective.is_empty() {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "goal.usage"));
            return Action::None;
        }
        let objective = objective.to_string();
        if self.run_started {
            self.reset_for_new_run();
        }
        self.maybe_suggest_design();
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(self.lang, "goal.starting", &[&objective]),
        );
        self.push_preflight(&objective);
        Action::StartGoal(objective)
    }

    /// Reconcile the in-memory phase vector with the furthest phase recorded in
    /// `.umadev/workflow-state.json` so `/status` reflects the REAL
    /// furthest-reached phase. The director-loop / plan build path drives the
    /// workspace and advances the persisted state (commit c29c31b43) but emits
    /// **no** `PhaseStarted` / `PhaseCompleted` events — only the legacy
    /// continuous walk does — so `self.phases` can sit all-`Pending` after a
    /// `/run` that actually wrote code, making the raw in-memory table lie.
    ///
    /// The reconciliation bumps the rendered status forward — **never** backward
    /// — to the max (by canonical [`PHASE_CHAIN`] order) of (a) the furthest
    /// phase the in-memory vector marks `Done`/`Running` and (b) `file_phase`
    /// (the workflow-state phase). Every phase before that furthest renders
    /// `Done`; the furthest renders `Running` when the in-memory vector is still
    /// actively driving it (legacy walk) else `Done` (it has been reached); the
    /// rest stay `Pending`. Fail-open: `file_phase = None` (missing / unparseable
    /// state) returns the in-memory statuses verbatim.
    fn reconcile_phase_statuses(rows: &[PhaseRow], file_phase: Option<Phase>) -> Vec<PhaseStatus> {
        // Furthest index the in-memory vector has touched (Done or Running).
        let mem_furthest = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.status, PhaseStatus::Done | PhaseStatus::Running))
            .map(|(i, _)| i)
            .max();
        // Index of the persisted workflow-state phase within `rows` (canonical
        // chain order). `None` when the file is absent / its phase is unknown.
        let file_idx = file_phase.and_then(|p| rows.iter().position(|r| r.phase == p));
        // No signal from either source → in-memory statuses unchanged.
        let furthest = match (mem_furthest, file_idx) {
            (None, None) => return rows.iter().map(|r| r.status).collect(),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (Some(a), Some(b)) => a.max(b),
        };
        rows.iter()
            .enumerate()
            .map(|(i, r)| match i.cmp(&furthest) {
                std::cmp::Ordering::Less => PhaseStatus::Done,
                std::cmp::Ordering::Equal => {
                    // The furthest reached phase: keep `Running` only while the
                    // in-memory vector is actively driving it (legacy walk);
                    // otherwise (file-derived, plan path) it has been reached.
                    if r.status == PhaseStatus::Running {
                        PhaseStatus::Running
                    } else {
                        PhaseStatus::Done
                    }
                }
                std::cmp::Ordering::Greater => PhaseStatus::Pending,
            })
            .collect()
    }

    fn open_status_overlay(&mut self) {
        // Read the persisted workflow state ONCE and reconcile each section
        // against it. The director-loop / plan path records the furthest-reached
        // phase here (and slug / requirement / active_gate) without emitting the
        // events that mutate the in-memory state, so the file is the more current
        // source after such a run — and the only source at all in a fresh session
        // reopened over a prior run. Every read below is fail-open (a missing /
        // unparseable state → in-memory only, no panic, no block).
        let ws = umadev_agent::read_workflow_state(&self.project_root);

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
        // slug / requirement fall back to the persisted state when the in-memory
        // value is empty (e.g. `/status` in a fresh session after a prior run).
        let slug_display = if self.slug.is_empty() {
            ws.as_ref()
                .map(|s| s.slug.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "(not set)".to_string())
        } else {
            self.slug.clone()
        };
        body.push_str(&format!("slug:          {slug_display}\n"));
        let req_display = if self.requirement.is_empty() {
            ws.as_ref()
                .map(|s| s.requirement.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "(none yet)".to_string())
        } else {
            self.requirement.clone()
        };
        body.push_str(&format!("requirement:   {req_display}\n"));
        body.push_str("\n## Pipeline phases\n\n");
        body.push_str("| # | Phase | Status |\n|---|---|---|\n");
        // Reconcile the in-memory phase vector with the furthest phase the
        // persisted state recorded — so a plan-path build that reached e.g.
        // `backend` shows research..backend done instead of a frozen all-pending
        // table. Fail-open: an unparseable / unknown phase → in-memory only.
        let file_phase = ws
            .as_ref()
            .and_then(|s| umadev_agent::phase_from_id(&s.phase));
        let statuses = Self::reconcile_phase_statuses(&self.phases, file_phase);
        for (i, row) in self.phases.iter().enumerate() {
            let status = statuses.get(i).copied().unwrap_or(row.status);
            let icon = match status {
                PhaseStatus::Done => "[ok]",
                PhaseStatus::Running => "[running]",
                PhaseStatus::Pending => "[pending]",
            };
            body.push_str(&format!("| {} | {} | {} |\n", i + 1, row.phase.id(), icon));
        }
        // Active gate — prefer the live in-memory gate, else the persisted one.
        let gate_display = self
            .active_gate
            .map(|g| g.id_str().to_string())
            .or_else(|| {
                ws.as_ref()
                    .map(|s| s.active_gate.clone())
                    .filter(|g| !g.is_empty())
            });
        if let Some(gate) = gate_display {
            body.push_str(&format!("\n[gate] Active gate: `{gate}`\n"));
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

    /// `/plan` — show and **steer** the live plan (Wave 1 deliverable 4).
    ///
    /// With no args: render the current checklist (or a hint when none is live).
    /// `/plan collapse` toggles the panel. The steering subcommands
    /// (`skip` / `veto` / `add` / `up` / `down`) fold a one-line directive into
    /// the next turn over the SAME session via [`queued_steer`] — so a reorder /
    /// skip / add / veto reaches the director without restarting the run. Plan
    /// edits are advisory directives, not a hard mutation of the plan DAG (the
    /// director owns the plan); the panel reflects them once the director re-posts.
    /// Fail-open: an unknown subcommand falls back to showing usage.
    fn slash_plan(&mut self, arg: &str) -> Action {
        let arg = arg.trim();
        // No args → show the plan (or a "no plan yet" hint) + a usage line.
        if arg.is_empty() {
            self.show_plan_status();
            return Action::None;
        }
        let mut parts = arg.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("").to_ascii_lowercase();
        let target = parts.next().unwrap_or("").trim();
        match sub.as_str() {
            "collapse" | "fold" | "toggle" => {
                self.plan_collapsed = !self.plan_collapsed;
                self.critics_collapsed = self.plan_collapsed;
                Action::None
            }
            "show" | "status" | "list" => {
                self.show_plan_status();
                Action::None
            }
            "skip" | "veto" | "up" | "down" | "add" => self.steer_plan(&sub, target),
            _ => {
                // Unknown subcommand → show usage (never silently swallow).
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "plan.steer.usage"),
                );
                Action::None
            }
        }
    }

    /// Render the live plan + team review as a chat note (for `/plan` with no
    /// args). Falls back to a friendly "no active plan" hint.
    ///
    /// Unlike the collapsible live panel (which clips the verdict tail to "… +N"),
    /// this prints the FULL team-review section — every reviewing seat's verdict
    /// and, for a blocking seat, ALL of its must-fix findings — so the panel's
    /// "/plan for all" affordance is truthful and nothing is hidden.
    fn show_plan_status(&mut self) {
        let has_plan = !self.plan_steps.is_empty();
        let has_review = !self.critic_verdicts.is_empty();
        if !has_plan && !has_review {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "plan.none"));
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "plan.steer.usage"),
            );
            return;
        }
        let mut body = String::new();
        if has_plan {
            let (done, total) = (
                self.plan_steps
                    .iter()
                    .filter(|s| s.status == "done")
                    .count(),
                self.plan_steps.len(),
            );
            body.push_str(&format!(
                "{} {done}/{total}\n",
                umadev_i18n::t(self.lang, "plan.panel.title")
            ));
            for step in &self.plan_steps {
                let mark = match step.status.as_str() {
                    "done" => "[x]",
                    "active" => "[~]",
                    "blocked" => "[!]",
                    _ => "[ ]",
                };
                body.push_str(&format!("  {mark} {} · {}\n", step.id, step.title));
            }
        }
        // Full team-review section: EVERY seat's verdict + a blocking seat's
        // complete findings. Mirrors the live panel's "[seat] accepts / N must-fix"
        // wording, but never clips the findings list.
        if has_review {
            let accepts = self.critic_verdicts.iter().filter(|c| c.accepts).count();
            let blocking = self.critic_verdicts.len() - accepts;
            body.push_str(&umadev_i18n::tf(
                self.lang,
                "plan.review.section",
                &[&accepts.to_string(), &blocking.to_string()],
            ));
            body.push('\n');
            for c in &self.critic_verdicts {
                let verdict = if c.accepts {
                    umadev_i18n::t(self.lang, "plan.review.accept").to_string()
                } else {
                    umadev_i18n::tf(
                        self.lang,
                        "plan.review.block",
                        &[&c.blocking.len().max(1).to_string()],
                    )
                };
                body.push_str(&format!("  [{}] {verdict}\n", c.seat));
                if !c.accepts {
                    for b in &c.blocking {
                        let item = b.trim();
                        if !item.is_empty() {
                            body.push_str(&format!("    - {item}\n"));
                        }
                    }
                }
            }
        }
        body.push_str(umadev_i18n::t(self.lang, "plan.steer.usage"));
        self.push(ChatRole::UmaDev, body);
    }

    /// `/team` — surface the AI development team as a first-class concept (Wave C
    /// of the development-team repositioning). Two parts, rendered as one
    /// `ChatRole::UmaDev` note styled like [`show_plan_status`](Self::show_plan_status):
    ///
    /// 1. **The roster** (always) — the eight specialist seats plus the
    ///    coordinator, each with the artifact it produces (the role→deliverable
    ///    model from [`TEAM_ROSTER`]).
    /// 2. **This run's team** (only with run context — a live plan OR recorded
    ///    critic verdicts) — the convened seats with their latest verdict (reusing
    ///    the team-review wording), and which deliverables actually EXIST on disk
    ///    (`produced` / `pending`). With no run context it shows the convene hint.
    ///
    /// Read-only and fail-open: a missing `output/` dir / no plan just yields the
    /// roster + the hint, never a panic.
    fn slash_team(&mut self, _arg: &str) -> Action {
        let mut body = String::new();
        // (1) Roster — always shown.
        body.push_str(umadev_i18n::t(self.lang, "team.title"));
        body.push('\n');
        for key in TEAM_ROSTER {
            body.push_str("  ");
            body.push_str(umadev_i18n::t(self.lang, key));
            body.push('\n');
        }
        // (2) This run's team — only when there is run context.
        let has_review = !self.critic_verdicts.is_empty();
        let has_plan = !self.plan_steps.is_empty();
        if has_review || has_plan {
            body.push('\n');
            body.push_str(umadev_i18n::t(self.lang, "team.run.header"));
            body.push('\n');
            // Convened seats + their latest verdict — same wording as the live
            // team-review panel / `show_plan_status` ([seat] accepts / N must-fix).
            if has_review {
                let accepts = self.critic_verdicts.iter().filter(|c| c.accepts).count();
                let blocking = self.critic_verdicts.len() - accepts;
                body.push_str(&umadev_i18n::tf(
                    self.lang,
                    "plan.review.section",
                    &[&accepts.to_string(), &blocking.to_string()],
                ));
                body.push('\n');
                for c in &self.critic_verdicts {
                    let verdict = if c.accepts {
                        umadev_i18n::t(self.lang, "plan.review.accept").to_string()
                    } else {
                        umadev_i18n::tf(
                            self.lang,
                            "plan.review.block",
                            &[&c.blocking.len().max(1).to_string()],
                        )
                    };
                    body.push_str(&format!("  [{}] {verdict}\n", c.seat));
                }
            }
            // Deliverables that actually exist on disk vs. still pending.
            body.push_str(umadev_i18n::t(self.lang, "team.run.deliverables"));
            body.push('\n');
            for (label_key, exists) in self.team_deliverable_status() {
                let mark = if exists {
                    umadev_i18n::t(self.lang, "team.run.produced")
                } else {
                    umadev_i18n::t(self.lang, "team.run.pending")
                };
                body.push_str(&format!(
                    "  {mark} {}\n",
                    umadev_i18n::t(self.lang, label_key)
                ));
            }
        } else {
            body.push('\n');
            body.push_str(umadev_i18n::t(self.lang, "team.no_run"));
            body.push('\n');
        }
        self.push(ChatRole::UmaDev, body);
        Action::None
    }

    /// Scan the workspace for each team deliverable, returning `(label_key,
    /// exists)` pairs in delivery order. Read-only + fail-open: a missing
    /// `output/` dir or an unreadable path counts as `pending`, never panics. The
    /// docs match by filename suffix so any project slug resolves; the proof +
    /// contract locations reuse the agent's canonical relative paths.
    fn team_deliverable_status(&self) -> Vec<(&'static str, bool)> {
        let root = &self.project_root;
        // `output/*-{prd,architecture,uiux}.md` — match by suffix (slug-agnostic).
        let output_has = |suffix: &str| -> bool {
            std::fs::read_dir(root.join("output"))
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(suffix)))
        };
        let contract = root.join(".umadev/contracts/openapi.json").exists()
            || root.join(".umadev/contracts/openapi.yaml").exists();
        let runtime = root.join(umadev_agent::runtime_proof_rel_path()).exists();
        let deploy = root.join(umadev_agent::deploy_proof_rel_path()).exists();
        vec![
            ("team.deliverable.prd", output_has("-prd.md")),
            (
                "team.deliverable.architecture",
                output_has("-architecture.md"),
            ),
            ("team.deliverable.uiux", output_has("-uiux.md")),
            ("team.deliverable.contract", contract),
            ("team.deliverable.runtime", runtime),
            ("team.deliverable.deploy", deploy),
        ]
    }

    /// Fold a plan-steering edit (`skip` / `veto` / `add` / `up` / `down`) into
    /// the next directive via [`queued_steer`], echoing a confirmation. The
    /// `skip` / `veto` / `up` / `down` forms validate `target` against a live
    /// step id; `add` takes free text. The edit applies at the next step boundary
    /// (the same place a queued steer fires), so it shares the run's session.
    fn steer_plan(&mut self, sub: &str, target: &str) -> Action {
        // `add` is free text; the rest reference an existing step id.
        if sub != "add" {
            if target.is_empty() {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "plan.steer.usage"),
                );
                return Action::None;
            }
            if !self.plan_steps.iter().any(|s| s.id == target) {
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "plan.steer.unknown_step", &[target]),
                );
                return Action::None;
            }
        }
        // Build the directive the director sees, and the user-facing confirmation.
        let (directive, confirm) = match sub {
            "skip" => (
                format!("Plan steering: SKIP step `{target}` — do not perform it; proceed with the rest of the plan."),
                umadev_i18n::tf(self.lang, "plan.steer.skip", &[target]),
            ),
            "veto" => (
                format!("Plan steering: VETO step `{target}` — remove it from the plan entirely and do not perform it."),
                umadev_i18n::tf(self.lang, "plan.steer.veto", &[target]),
            ),
            "up" => (
                format!("Plan steering: REORDER step `{target}` EARLIER — do it before its current predecessors where dependencies allow."),
                umadev_i18n::tf(self.lang, "plan.steer.move", &[target, "↑"]),
            ),
            "down" => (
                format!("Plan steering: REORDER step `{target}` LATER — defer it after its current successors where dependencies allow."),
                umadev_i18n::tf(self.lang, "plan.steer.move", &[target, "↓"]),
            ),
            // `add`
            _ => (
                format!("Plan steering: ADD a new step — {target}"),
                umadev_i18n::tf(self.lang, "plan.steer.add", &[target]),
            ),
        };
        // Fold into the next directive over the SAME session (the queued-steer
        // mechanism), then confirm to the user.
        self.queued_steer.push_back(directive);
        self.push(ChatRole::UmaDev, confirm);
        // If nothing is mid-run (no gap will come), tell the user the edit is
        // parked and will apply at the next step boundary — never silently lost.
        if !self.is_pipeline_active() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "plan.steer.queued"),
            );
        }
        Action::None
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
            KeyCode::End | KeyCode::Char('G') => ov.scroll_to_end(),
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
                ChatRole::Error => "WARNING",
            };
            body.push_str(&format!("[{label}] {}\n", msg.body()));
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

    /// Synthesize the **build-complete card** shown after EVERY effective build
    /// (chat / Fast / Delivery): a `✅ done` headline + what changed + the key
    /// entry point + the run command. Plain markdown (the transcript renderer
    /// formats it) — no new data model. **Fail-open**: every section is
    /// best-effort; a section whose signal is missing is simply omitted, and the
    /// card never errors. The optional `preview_pending` flag adds a "starting
    /// dev server…" line so the user knows a URL is coming (the real URL is
    /// appended later, once `wait_for_port` confirms the server is up).
    #[must_use]
    pub(crate) fn build_completion_card(&self, preview_pending: bool) -> String {
        // Recognizable app entry points, most-specific first — the first one that
        // exists on disk is reported as the build's "key entry".
        const ENTRIES: &[&str] = &[
            "src/App.tsx",
            "src/App.jsx",
            "src/main.tsx",
            "src/main.jsx",
            "app/page.tsx",
            "src/index.tsx",
            "src/index.js",
            "index.html",
            "public/index.html",
            "src/main.rs",
            "main.py",
            "app.py",
        ];
        let lang = self.lang;
        let mut lines: Vec<String> = vec![umadev_i18n::t(lang, "build.complete.title").to_string()];

        // Changed files — the real working-tree delta from `git status`. Capped
        // for readability with a "+N more". Fail-open: a non-git workspace (or an
        // empty delta) drops to listing the output directories that exist, so the
        // card always shows SOMETHING concrete the build produced.
        let changed: Vec<String> = crate::git_status_porcelain(&self.project_root)
            .map(|snap| {
                snap.lines()
                    .filter_map(crate::porcelain_path)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if changed.is_empty() {
            // No git delta → at least name the product directories that exist.
            let dirs: Vec<&str> = ["src", "app", "public", "output", "release"]
                .into_iter()
                .filter(|d| self.project_root.join(d).is_dir())
                .collect();
            if !dirs.is_empty() {
                lines.push(umadev_i18n::tf(
                    lang,
                    "build.complete.no_files",
                    &[&dirs.join(" · ")],
                ));
            }
        } else {
            const CAP: usize = 12;
            let shown = changed
                .iter()
                .take(CAP)
                .map(|f| format!("  · {f}"))
                .collect::<Vec<_>>()
                .join("\n");
            let mut block = umadev_i18n::tf(
                lang,
                "build.complete.files",
                &[&changed.len().to_string(), &shown],
            );
            if changed.len() > CAP {
                block.push_str(&umadev_i18n::tf(
                    lang,
                    "build.complete.files_more",
                    &[&(changed.len() - CAP).to_string()],
                ));
            }
            lines.push(block);
        }

        // Key entry point — the first recognizable app entry that exists on disk.
        if let Some(entry) = ENTRIES.iter().find(|p| self.project_root.join(p).is_file()) {
            lines.push(umadev_i18n::tf(lang, "build.complete.entry", &[entry]));
        }

        // Run command — worker-recorded, else self-detected dev server.
        let run_cmd = self.run_command_from_notes().or_else(|| {
            umadev_agent::verify::detect_dev_server(&self.project_root)
                .map(|ds| ds.command.to_string())
        });
        if let Some(cmd) = run_cmd {
            lines.push(umadev_i18n::tf(lang, "build.complete.run", &[&cmd]));
        }

        if preview_pending {
            lines.push(umadev_i18n::t(lang, "build.complete.preview_starting").to_string());
        }

        lines.join("\n\n")
    }

    /// The auto-preview target for a finished build: the detected dev server's
    /// command + the URL to open (worker-recorded port preferred, else the
    /// framework default). Returns `None` for a non-web project / when no dev
    /// server is detected — the caller then shows the card WITHOUT a preview line
    /// and starts no server (**fail-open**, never blocks the completion).
    #[must_use]
    pub(crate) fn auto_preview_target(&self) -> Option<(String, String)> {
        let ds = umadev_agent::verify::detect_dev_server(&self.project_root)?;
        let url = self
            .preview_url_from_notes()
            .unwrap_or_else(|| ds.default_url.to_string());
        Some((url, ds.command.to_string()))
    }

    /// Push the build-complete card into the transcript and return the
    /// auto-preview target (dev-server URL + command) when this is a web project.
    /// Called after EVERY effective build (chat / Fast / Delivery). The caller
    /// (the event loop) uses the returned target to background-start the dev
    /// server via the shared `/preview` machinery; `None` means non-web / no dev
    /// server detected → the card is shown WITHOUT a preview line and no server
    /// is started. **Fail-open**: both the card and the target are best-effort.
    pub(crate) fn post_build_completion_card(&mut self) -> Option<(String, String)> {
        let preview = self.auto_preview_target();
        let card = self.build_completion_card(preview.is_some());
        self.push(ChatRole::UmaDev, card);
        // A chat/Fast director build is settling here (this path never reaches the
        // Delivery `BlockCompleted` banner) — fold the last review round into the
        // transcript and drop the live plan / team-review panel so it doesn't hang
        // on screen below the completion card as stale state.
        self.finalize_live_panels();
        preview
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
    /// Shift+Tab cycles the full trust/autonomy tier Plan → Guarded → Auto →
    /// Plan (Claude-Code style), so the Plan tier is reachable from the keyboard
    /// — it used to flip only Auto <-> Guarded, leaving Plan reachable only via
    /// `/mode plan`. Plan = read-only research + plan; Guarded (default) = pause
    /// at every gate; Auto = run end-to-end. The current tier shows in the prompt
    /// meta row; a brief confirmation line names the new tier.
    pub fn cycle_approval_mode(&mut self) {
        use umadev_agent::TrustMode;
        let next = match self.effective_trust_mode() {
            TrustMode::Plan => TrustMode::Guarded,
            TrustMode::Guarded => TrustMode::Auto,
            TrustMode::Auto => TrustMode::Plan,
        };
        self.set_trust_mode(next);
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::t(self.lang, next.desc_key()).to_string(),
        );
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
        // No session override → derive from `.umadevrc`, but serve it from the
        // process-local cache so the render hot path never touches disk. The
        // cache is invalidated whenever the config could have changed (see
        // `invalidate_trust_cache`), so this stays correct. Fail-open: a read
        // error inside `load_project_config` yields the default (`guarded`).
        if let Some(cached) = self.config_trust_cache.get() {
            return cached;
        }
        let config_auto = umadev_agent::config::load_project_config(&self.project_root)
            .pipeline
            .auto_approve_gates;
        let mode = if config_auto {
            umadev_agent::TrustMode::Auto
        } else {
            umadev_agent::TrustMode::Guarded
        };
        self.config_trust_cache.set(Some(mode));
        mode
    }

    /// Drop the cached config-derived trust tier so the next
    /// [`effective_trust_mode`] re-reads `.umadevrc`. Call after anything that
    /// could change the on-disk `auto_approve_gates` (a `/mode` switch is held
    /// in `trust_mode_override` and wins outright, but clearing here keeps the
    /// cache honest if the override is later removed). Cheap and fail-open.
    fn invalidate_trust_cache(&self) {
        self.config_trust_cache.set(None);
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
        // A `/mode` switch may also have rewritten `.umadevrc`'s
        // `auto_approve_gates` elsewhere; drop the cache so the fallback path
        // re-reads fresh if the session override is ever cleared.
        self.invalidate_trust_cache();
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

    /// Toggle mouse capture. ON (default) lets the wheel page the history AND
    /// drives the in-app drag-to-select/copy layer (we render the highlight and
    /// copy via OSC 52 ourselves). OFF issues `DisableMouseCapture`, handing
    /// selection back to the terminal's native click-drag for users who prefer
    /// it. The event loop reads `mouse_scroll` each turn and only routes mouse
    /// events when it's on.
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
        // P5d: flip the LIVE field so the spinner switches static/animated this
        // instant — not only after the next restart re-reads settings.json.
        self.animations = new_val;
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
        // A failed persist means the language reverts on next launch — say so
        // rather than letting the "changed" line below imply it stuck.
        if let Err(e) = crate::config::save_to(&self.config, &self.config_path) {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(lang, "config.save_failed_note", &[&e.to_string()]),
            );
        }
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
                        m.body().chars().take(120).collect::<String>()
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
    /// Returns `Err` with the failure reason when the answer could NOT be
    /// persisted, so the caller can show an honest write-error note instead of a
    /// false "recorded" line. On a write failure the resume path would lose the
    /// answer silently, so the user must be told.
    fn append_clarify_answer(&self, answer: &str) -> std::io::Result<()> {
        let path = self
            .project_root
            .join("output")
            .join(format!("{}-clarify-answers.md", self.slug));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let updated = if existing.trim().is_empty() {
            answer.to_string()
        } else {
            format!("{existing}\n{answer}")
        };
        std::fs::write(&path, updated)
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
        // P5d: every spinner surface (tool-running glyph, thinking, aliveness)
        // funnels through this ONE shared braille frame source.
        //
        // - Animations off / non-TTY → a single static glyph (`⋯`), so the UI
        //   never strobes for accessibility or in a piped render.
        // - Otherwise → advance one braille frame per ~80ms tick, ALWAYS while
        //   work is in flight. We deliberately do NOT freeze on a stall: a frozen
        //   spinner reads as "crashed", the opposite of the intended signal — the
        //   base is still working, just not emitting output yet. The "taking long"
        //   cue stays on the COLOR + the "still working (mm:ss)" heartbeat text,
        //   not on stopping the spin.
        spinner_frame(self.tick, self.animations, false)
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
        // P5d: a static, non-strobing glyph when animation is off / non-TTY.
        if !self.animations {
            return '○';
        }
        // tick is ~80ms; /2 → ~160ms per frame (close to the ~120ms target,
        // and an integer divisor of the tick so the cadence stays steady).
        FRAMES[((self.tick as usize) / 2) % FRAMES.len()]
    }

    /// `true` when a phase is running but the base has gone quiet past the
    /// stall threshold — no worker output for >60s AND no tool call mid-flight.
    /// This is the HONEST "about to hang" signal: the UI paints the status red
    /// so the user sees a truthful cue instead of a fake-smooth spinner. The
    /// threshold is deliberately generous (60s): a base legitimately thinks or
    /// web-searches for up to a minute without emitting a token, so a quiet spell
    /// must NOT read as stalled — red means "probably wrong", not "still
    /// thinking". Returns `false` whenever nothing is running, a tool call is in
    /// progress (a long `npm install` is work, not a stall), or output arrived
    /// within 60s.
    #[must_use]
    pub fn is_stalled(&self) -> bool {
        // Adaptive threshold (Wave 6): the normal "about to hang" window is a
        // generous 60s, but a known long operation (a dependency install / full
        // build, flagged by `long_op_in_progress`) legitimately runs for minutes
        // with no output — using the 60s window there would paint a false red
        // mid-`npm install`. So widen the window to 5 min while one is in flight.
        // (In practice `tool_in_progress` already suppresses the red cue during a
        // tool call, so this is the belt-and-braces case where a long op's own
        // tool boundary has passed but the work it kicked off is still settling.)
        const STALL: std::time::Duration = std::time::Duration::from_secs(60);
        const STALL_LONG_OP: std::time::Duration = std::time::Duration::from_secs(300);
        let stall = if self.long_op_in_progress {
            STALL_LONG_OP
        } else {
            STALL
        };
        // Stall only makes sense while something is ACTIVELY running: a phase is
        // in flight, a chat turn is "thinking", OR the run has STARTED but not
        // yet entered its first `Running` phase. That last case is the structural
        // backstop: between `PipelineStarted` and the first phase there is a
        // silent window (cold index build / intake / vector build) where
        // `phase_started_at` is `None` and `thinking` is `false`. Without it, any
        // silent path that drops its heartbeat in that window would freeze with no
        // spinner and never go red. A gate (paused for the user) and a finished /
        // aborted run are NOT active, so we never falsely go red there.
        let pre_phase =
            self.run_started && !self.finished && !self.aborted && self.active_gate.is_none();
        let active = self.phase_started_at.is_some() || self.thinking || pre_phase;
        if !active || self.tool_in_progress {
            return false;
        }
        match self.last_output_at {
            Some(t) => t.elapsed() >= stall,
            // Nothing has arrived yet this turn: only call it a stall once the
            // active block has been running past the threshold (a just-started
            // phase / a just-launched run isn't stalled, it's spinning up). The
            // pre-phase window has no `phase_started_at`/`thinking_started`, so
            // fall back to the run's own start instant.
            None => self
                .phase_started_at
                .or(self.thinking_started)
                .or(self.run_started_at)
                .is_some_and(|t| t.elapsed() >= stall),
        }
    }

    /// Record a sign of life from the base — call on every worker stream event /
    /// host output line / progress note so [`Self::is_stalled`] resets.
    fn mark_output(&mut self) {
        self.last_output_at = Some(std::time::Instant::now());
        // Any real sign of life (host output, worker stream, a progress note)
        // supersedes the heartbeat's in-place "still working" line — drop it so
        // a stale timer doesn't linger next to fresh content. The heartbeat beat
        // itself calls `mark_output` *before* re-setting `transient_status`, so
        // an active slow phase still keeps its live line.
        self.transient_status = None;
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
/// A persisted chat session (Wave 5 / G11) — the on-disk mirror of
/// [`App::conversation`], one JSON file per saved chat under `.umadev/chat/`. The
/// schema is deliberately small and forward-compatible (`#[serde(default)]` on the
/// soft fields) so an older file still loads after a field is added.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ChatSession {
    /// Stable chat id (the file stem). Pairs with [`App::chat_id`].
    pub id: String,
    /// ISO-8601 UTC timestamp of the last persist — drives most-recent ordering
    /// in `/sessions` and the launch-time "reopen most recent" pick.
    #[serde(default)]
    pub updated_at: String,
    /// Backend id that produced this chat (advisory; for the listing).
    #[serde(default)]
    pub backend: String,
    /// The base's OWN resumable session pointer for this chat (claude's pinned
    /// `--session-id` / codex's `thread.id`; `None` for opencode, an offline chat,
    /// or a chat that never took a host turn). Persisted so reopening UmaDev can
    /// `--resume <id>` / `thread/resume` the SAME base conversation — restoring the
    /// base's DEEP accumulated context — instead of cold-starting a fresh brain that
    /// only sees the replayed ≤16-message transcript. `#[serde(default)]` for
    /// back-compat: an older chat file without the field loads as `None` (fail-open
    /// → today's fresh-session + transcript-replay behavior). Distinct from
    /// [`Self::id`] (the chat FILE id) — they are DIFFERENT ids and resuming the file
    /// id targets a base session that was never created.
    #[serde(default)]
    pub base_session_id: Option<String>,
    /// The conversation transcript, oldest → newest.
    pub messages: Vec<umadev_runtime::Message>,
}

/// A best-effort ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) WITHOUT pulling
/// in `chrono` — the TUI crate stays dependency-light. Derived from the Unix
/// epoch via a plain civil-date conversion (days-from-epoch → Y/M/D). Used only
/// for human-facing ordering/labels, so a leap-second-level imprecision is fine.
/// Whether a message is long enough to be FOLDABLE (P6). A Host/UmaDev text
/// body, or a tool row whose result, exceeds [`FOLD_THRESHOLD`] source lines.
/// Used by both the Ctrl+R toggle (to pick the row to flip) and the renderer (to
/// decide whether to draw the head-N preview + summary). Cheap line count;
/// fail-open (anything else → not foldable).
/// Resolve the effective Codex launch sandbox and publish it into
/// `UMADEV_CODEX_SANDBOX` (the env the codex driver reads). Precedence: a
/// pre-set env (advanced / CI override) wins; otherwise the project's `.umadevrc`
/// `[codex] sandbox_mode` is resolved (fail-open → `workspace-write`) and
/// exported. Returns the effective mode so the caller can decide whether to warn.
fn resolve_and_publish_codex_sandbox(
    project_root: &std::path::Path,
) -> umadev_agent::config::CodexSandbox {
    use umadev_agent::config::CodexSandbox;
    // An explicit env (set by an advanced user or CI) is authoritative.
    if let Ok(v) = std::env::var("UMADEV_CODEX_SANDBOX") {
        if !v.trim().is_empty() {
            return CodexSandbox::parse_fail_open(&v);
        }
    }
    // Otherwise publish the `.umadevrc` choice so the codex driver honors it.
    let mode = umadev_agent::config::load_project_config(project_root)
        .codex
        .resolved_sandbox();
    std::env::set_var("UMADEV_CODEX_SANDBOX", mode.as_codex_arg());
    mode
}

/// Decide whether the high-risk codex-sandbox liability warning should fire: ONLY
/// when codex is the active base AND the resolved sandbox is the high-risk
/// `danger-full-access` tier. `read-only` / `workspace-write`, or any other base,
/// stay silent. Pure + unit-testable.
pub(crate) fn should_warn_codex_sandbox(
    backend: Option<&str>,
    mode: umadev_agent::config::CodexSandbox,
) -> bool {
    mode.is_high_risk() && backend == Some("codex")
}

/// A System `[thinking]` reasoning block: its first line is the `[thinking] …`
/// header and there is at least one reasoning line below it (the base's accumulated
/// extended-thinking text). Such a row folds (default collapsed; Ctrl+O / Ctrl+R
/// expands) — distinct from an ordinary one-line System status row, which never
/// folds, and from a content-less / no-reasoning summary (whose tag was dropped on
/// collapse, so it no longer starts with the tag).
pub(crate) fn is_thinking_reasoning_block(role: ChatRole, body: &str) -> bool {
    role == ChatRole::System
        && body.trim_start().starts_with(THINKING_PLACEHOLDER_TAG)
        && body.lines().count() > 1
}

pub(crate) fn message_is_collapsible(m: &ChatMessage) -> bool {
    match &m.kind {
        MessageBody::Text(s) => {
            (matches!(m.role, ChatRole::Host | ChatRole::UmaDev)
                && s.lines().count() > FOLD_THRESHOLD)
                // A `[thinking]` reasoning block folds regardless of length, so the
                // user can collapse/expand the base's chain of thought (Ctrl+R) and
                // the global Ctrl+O reveal-all reaches it too.
                || is_thinking_reasoning_block(m.role, s)
        }
        MessageBody::Tool(t) => t
            .result
            .as_deref()
            .is_some_and(|r| r.lines().count() > FOLD_THRESHOLD),
        // A diff card is collapsible once it's large enough to be worth folding
        // (the same threshold that drives its default-collapsed state).
        MessageBody::Diff(d) => d.total_rows() > DIFF_FOLD_THRESHOLD,
    }
}

/// The headline for a merged low-signal batch row, e.g. `读取 3 个文件,搜索` /
/// `inspected 3 items`. One localized phrase carries the live count; the count
/// is greatest-seen so a streamed value never visibly jumps backwards.
fn merged_batch_summary(lang: umadev_i18n::Lang, count: u32) -> String {
    umadev_i18n::tf(lang, "tui.tool.batch", &[&count.to_string()])
}

/// Fold a read-only tool's raw result into a single metric instead of dumping
/// it: a `Grep`/`Glob` summary that mentions a count keeps `(N matches)`,
/// otherwise the result is suppressed entirely (the merged headline already
/// says what happened). Returns `None` when there is nothing worth a gutter
/// line. Fail-open: any parse miss → `None`.
fn read_only_metric(lang: umadev_i18n::Lang, name: &str, preview: &str) -> Option<String> {
    // Pull the first integer out of the summary, if any (`3 matches`, `Found 5`).
    let n: Option<usize> = preview
        .split(|c: char| !c.is_ascii_digit())
        .find_map(|s| s.parse::<usize>().ok());
    match (name, n) {
        ("Grep" | "Glob", Some(n)) => {
            Some(umadev_i18n::tf(lang, "tui.tool.matches", &[&n.to_string()]))
        }
        _ => None,
    }
}

/// A low-signal read-only tool whose calls may be *merged* into one transcript
/// row with an incrementing count (`read N files, searched M times`). These
/// never mutate the tree and their raw output is noise, so dumping each one as
/// its own row buries the actual work; folding them keeps the transcript
/// legible. Write / Edit / Bash / web / agent are NOT here — they each get their
/// own row (their result IS the signal).
fn is_low_signal_tool(name: &str) -> bool {
    matches!(name, "Read" | "Grep" | "Glob" | "NotebookRead")
}

/// The bracket tag historically shown before a tool name (`[read]` / `[write]`
/// …). Kept for the flat text rendering (export / brain transcript) and as a
/// fallback; the structured transcript row uses a status glyph instead.
fn tool_tag(name: &str) -> &'static str {
    match name {
        "Read" | "NotebookRead" | "NotebookEdit" => "[read]",
        "Write" | "Edit" => "[write]",
        "Bash" => "[run]",
        "Grep" | "Glob" => "[search]",
        "WebSearch" | "WebFetch" => "[web]",
        "Task" | "Agent" => "[agent]",
        _ => "[auto]",
    }
}

/// Whether a `Bash` command detail looks like a legitimately-long operation
/// (dependency install / full build / image pull) that runs for minutes with no
/// output — so the stall watchdog should widen its window instead of flashing red
/// on honest work. Substring match on the command text; conservative (false → the
/// normal 60s threshold still applies).
fn is_long_running_command(detail: &str) -> bool {
    const LONG: &[&str] = &[
        "npm install",
        "npm ci",
        "npm run build",
        "yarn install",
        "yarn build",
        "pnpm install",
        "pnpm build",
        "pip install",
        "poetry install",
        "cargo build",
        "cargo test",
        "cargo check",
        "go build",
        "go mod download",
        "go test",
        "mvn ",
        "gradle",
        "./gradlew",
        "make ",
        "cmake",
        "docker build",
        "docker pull",
        "docker compose",
        "bundle install",
        "composer install",
        "vite build",
        "webpack",
        "next build",
        "tsc ",
        "apt-get install",
        "brew install",
        "playwright install",
    ];
    let d = detail.to_ascii_lowercase();
    LONG.iter().any(|p| d.contains(p))
}

/// Whether `body` ends with an UNCLOSED ```code fence``` — an odd number of
/// fence lines (a line whose first non-space run is ``` or ~~~). The streaming
/// segmenter uses this to avoid rolling over to a new Host bubble mid-fence,
/// which would split one code block across two independently-rendered segments
/// and scramble both. Cheap line scan; fail-open (a malformed body just reads as
/// "closed" and rolls over normally).
/// The shared braille spinner frames — the ONE source every animated spinner
/// surface (tool-running glyph, thinking indicator, aliveness cue) draws from
/// (P5d), so they all rotate in lockstep at the same cadence.
pub(crate) const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The static glyph shown when animation is off / a non-TTY render (P5d) — a
/// horizontal ellipsis, reading as "working" without strobing.
pub(crate) const SPINNER_STATIC: char = '⋯';

/// Resolve the spinner glyph for the current tick under the P5d rules: a static
/// glyph when `animated` is false, a FROZEN frame when `stalled` (the spinner
/// must stop moving so a stall never looks like smooth progress), else the live
/// braille frame for `tick`. Pure + total — used by [`App::spinner`] and locked
/// by unit tests.
#[must_use]
pub(crate) fn spinner_frame(tick: u8, animated: bool, stalled: bool) -> char {
    if !animated {
        return SPINNER_STATIC;
    }
    if stalled {
        // Freeze on the first frame — the warning color is applied by the caller.
        return SPINNER_FRAMES[0];
    }
    SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()]
}

/// P5d: the initial animation state — `false` (static spinner) when stdout is not
/// a real terminal (CI / piped output) OR the user persisted `animations_enabled
/// = false`; `true` otherwise. Fail-open to `true` (animated, today's behaviour)
/// on any read error.
fn animations_enabled_default() -> bool {
    use std::io::IsTerminal;
    // A non-interactive stdout (piped / redirected) never benefits from a spinner
    // and a strobing braille frame just spams the log — render static there.
    if !std::io::stdout().is_terminal() {
        return false;
    }
    // Honor a persisted `/animations off`. Absent / unreadable → animated.
    let path = std::env::var("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .join(".umadev")
                .join("settings.json")
        })
        .unwrap_or_default();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("animations_enabled")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(true)
}

pub(crate) fn has_open_code_fence(body: &str) -> bool {
    let mut open = false;
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            open = !open;
        }
    }
    open
}

fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (hh, mm, ss) = (tod / 3_600, (tod % 3_600) / 60, tod % 60);
    // Civil-from-days (Howard Hinnant's algorithm), epoch = 1970-01-01. `days` is
    // small (well under i64::MAX for any plausible clock), so the conversion can't
    // realistically fail; fall back to 0 on the impossible overflow rather than
    // panicking — this is a cosmetic timestamp.
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

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
                auth: AuthMark::LoggedIn,
                login_cmd: String::new(),
                install_cmd: String::new(),
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
                    auth: probe.map_or(AuthMark::Unknown, |p| p.auth),
                    login_cmd: probe.map(|p| p.login_cmd.clone()).unwrap_or_default(),
                    install_cmd: probe.map(|p| p.install_cmd.clone()).unwrap_or_default(),
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
        let ln = lines.to_string();
        let detail = if lines == 0 {
            t(lang, "gate.detail.missing").to_string()
        } else if is_scaffold {
            warnings.push(tf(lang, "gate.scaffold_warn", &[a.as_str()]));
            tf(lang, "gate.detail.scaffold", &[ln.as_str()])
        } else if lines < 30 {
            warnings.push(tf(lang, "gate.short_warn", &[a.as_str(), ln.as_str()]));
            tf(lang, "gate.detail.short", &[ln.as_str()])
        } else {
            tf(lang, "gate.detail.ok", &[ln.as_str()])
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
            let dark = if has_dark {
                t(lang, "gate.detail.dark_ok")
            } else {
                t(lang, "gate.detail.dark_missing")
            };
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

/// Split a `PlanPosted` step summary (`id · title (seat)`) into `(id, title)`
/// for the live checklist. The `(seat)` suffix is kept on the title (it reads
/// as useful context — who owns the step). **Fail-open**: a summary that doesn't
/// match the `id ·` shape yields a positional id (`s{index}`) and the whole
/// string as the title, so a malformed summary never drops a step or panics.
pub(crate) fn split_plan_summary(summary: &str, index: usize) -> (String, String) {
    // The separator is the middle-dot `·` with surrounding spaces (see
    // `Plan::step_summaries`). Split on the FIRST occurrence only.
    if let Some((id, rest)) = summary.split_once(" · ") {
        let id = id.trim();
        if !id.is_empty() {
            return (id.to_string(), rest.trim().to_string());
        }
    }
    (format!("s{index}"), summary.trim().to_string())
}

/// Sentinel that prefixes the structured auth metadata `spawn_probe` packs onto
/// the [`EngineEvent::BackendProbed`] `detail` (since that event can't grow new
/// fields — it lives in umadev-agent, outside this crate). Shape:
/// `\u{1}auth=<state>|login=<cmd>|install=<cmd>\u{1}<human detail>`.
pub(crate) const PROBE_AUTH_SENTINEL: char = '\u{1}';

/// Unpack the auth tag `spawn_probe` packed onto a probe `detail`. Returns
/// `(auth_mark, login_cmd, install_cmd, human_detail)`. **Fail-open**: a `detail`
/// with no sentinel (an external emitter, an older build) yields
/// `(Unknown, "", "", detail)` — the human string is preserved verbatim and the
/// picker simply shows the conservative "unknown" mark.
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
        // Malformed (no closing sentinel) — treat the whole thing as human text.
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
        if let Some(v) = field.strip_prefix("auth=") {
            auth = AuthMark::from_tag(v);
        } else if let Some(v) = field.strip_prefix("login=") {
            login = v.to_string();
        } else if let Some(v) = field.strip_prefix("install=") {
            install = v.to_string();
        }
    }
    (auth, login, install, human.to_string())
}

fn refresh_picker_with_probes(items: &mut [PickerItem], probes: &[BackendInfo]) {
    for item in items.iter_mut() {
        if let Some(id) = item.backend_id.as_deref() {
            if let Some(p) = probes.iter().find(|p| p.id == id) {
                item.ready = p.ready;
                item.detail = p.detail.clone();
                item.auth = p.auth;
                item.login_cmd.clone_from(&p.login_cmd);
                item.install_cmd.clone_from(&p.install_cmd);
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
    use umadev_agent::config::CodexSandbox;

    #[test]
    fn codex_sandbox_warning_only_for_danger_full_access_on_codex() {
        // Fires ONLY for the high-risk tier on the codex base.
        assert!(should_warn_codex_sandbox(
            Some("codex"),
            CodexSandbox::DangerFullAccess
        ));
        // Safe tiers stay silent, even on codex.
        assert!(!should_warn_codex_sandbox(
            Some("codex"),
            CodexSandbox::WorkspaceWrite
        ));
        assert!(!should_warn_codex_sandbox(
            Some("codex"),
            CodexSandbox::ReadOnly
        ));
        // Other bases never warn, even at the high-risk tier (the knob is codex's).
        assert!(!should_warn_codex_sandbox(
            Some("claude-code"),
            CodexSandbox::DangerFullAccess
        ));
        assert!(!should_warn_codex_sandbox(
            None,
            CodexSandbox::DangerFullAccess
        ));
    }

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
        // gate-card tests see the manual-approval path. Remove any leftover dir
        // from a PRIOR run first so a persisted `.umadev/chat/` (Wave 5) can't
        // bleed into a test that expects a clean conversation buffer.
        let workspace = std::path::PathBuf::from(format!("/tmp/sd-test-ws-{id}"));
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::create_dir_all(&workspace);
        let _ = std::fs::write(
            workspace.join(".umadevrc"),
            "[pipeline]\nauto_approve_gates = false\n",
        );
        let mut app = App::new(
            "demo",
            cfg,
            std::path::PathBuf::from(format!("/tmp/sd-test-cfg-{id}.toml")),
            workspace,
        );
        // P5d: force animations ON in tests so spinner-cadence assertions are
        // deterministic regardless of whether the test host's stdout is a TTY
        // (where `animations_enabled_default` would otherwise pick `false`).
        app.animations = true;
        app
    }

    #[test]
    fn transient_status_updates_field_without_growing_transcript() {
        // The long-phase heartbeat's periodic beats arrive as TransientStatus
        // and must update the in-place status field WITHOUT pushing a transcript
        // row — this is the flood-bug fix. A repeated beat overwrites, never
        // appends; a `None` clears the line.
        let mut app = fresh_app(Some("offline"));
        let before = app.history.len();

        app.apply_engine(EngineEvent::TransientStatus(Some(
            "做事 仍在进行(已 0:03)".to_string(),
        )));
        assert_eq!(
            app.history.len(),
            before,
            "a transient beat must NOT add a transcript row"
        );
        assert_eq!(
            app.transient_status.as_deref(),
            Some("做事 仍在进行(已 0:03)"),
            "the in-place status field must be set"
        );

        // A second beat OVERWRITES the field (still no new row).
        app.apply_engine(EngineEvent::TransientStatus(Some(
            "做事 仍在进行(已 0:10)".to_string(),
        )));
        assert_eq!(app.history.len(), before, "second beat must not add a row");
        assert_eq!(
            app.transient_status.as_deref(),
            Some("做事 仍在进行(已 0:10)"),
            "the field must be overwritten by the newer beat"
        );

        // Completion clears the line.
        app.apply_engine(EngineEvent::TransientStatus(None));
        assert_eq!(app.history.len(), before, "clearing must not add a row");
        assert!(
            app.transient_status.is_none(),
            "TransientStatus(None) must clear the in-place line"
        );
    }

    #[test]
    fn real_output_and_phase_boundary_clear_a_stale_heartbeat_line() {
        // A real sign of life (host output) or a new phase supersedes the
        // heartbeat reassurance — the in-place line must not linger next to
        // fresh content.
        let mut app = fresh_app(Some("offline"));
        app.transient_status = Some("阶段 仍在进行(已 1:51)".to_string());
        app.apply_engine(EngineEvent::HostOutput {
            phase: Phase::Frontend,
            line: "real worker output".to_string(),
        });
        assert!(
            app.transient_status.is_none(),
            "real host output must clear a stale heartbeat line"
        );

        app.transient_status = Some("阶段 仍在进行(已 2:30)".to_string());
        app.apply_engine(EngineEvent::PhaseStarted {
            phase: Phase::Backend,
        });
        assert!(
            app.transient_status.is_none(),
            "a fresh phase must clear the prior phase's heartbeat line"
        );
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
        assert_eq!(app.transcript_scroll(), 1);
        // Shift+↓ brings it back.
        let _ = app.apply_key_with_mods(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::SHIFT,
        );
        assert_eq!(app.transcript_scroll(), 0);
    }

    #[test]
    fn page_and_home_end_scroll_against_published_viewport() {
        let mut app = fresh_app(Some("offline"));
        app.transcript_max_scroll.set(100);
        app.transcript_viewport_rows.set(20);
        // PageUp = viewport - 1 rows.
        let _ = app.apply_key(crossterm::event::KeyCode::PageUp);
        assert_eq!(app.transcript_scroll(), 19);
        // Home jumps to the very top (= max scroll).
        let _ = app.apply_key(crossterm::event::KeyCode::Home);
        assert_eq!(app.transcript_scroll(), 100);
        // End re-pins to the bottom.
        let _ = app.apply_key(crossterm::event::KeyCode::End);
        assert_eq!(app.transcript_scroll(), 0);
        // Ctrl+Alt+U = half a page up (the half-page scroll moved off bare
        // Ctrl-U so the shell "clear line" key keeps its job).
        let _ = app.apply_key_with_mods(
            crossterm::event::KeyCode::Char('u'),
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
        );
        assert_eq!(app.transcript_scroll(), 10);
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
            app.transcript_scroll(),
            10,
            "Ctrl+Alt+U scrolls half a page up"
        );
        // Ctrl+Alt+D → half a viewport back down.
        let _ = app.apply_key_with_mods(crossterm::event::KeyCode::Char('d'), cmd_alt);
        assert_eq!(
            app.transcript_scroll(),
            0,
            "Ctrl+Alt+D scrolls half a page down"
        );
        // Ctrl+Alt+B / Ctrl+Alt+F are paging aliases.
        let _ = app.apply_key_with_mods(crossterm::event::KeyCode::Char('b'), cmd_alt);
        assert_eq!(app.transcript_scroll(), 10, "Ctrl+Alt+B aliases scroll-up");
        let _ = app.apply_key_with_mods(crossterm::event::KeyCode::Char('f'), cmd_alt);
        assert_eq!(app.transcript_scroll(), 0, "Ctrl+Alt+F aliases scroll-down");
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
            app.transcript_scroll(),
            0,
            "bare Ctrl-U must not move the transcript"
        );
        // Scroll up first, then bare Ctrl-D: it must NOT scroll back (Ctrl-D is
        // the terminal EOF/quit convention, not a scroll key). On empty input
        // it routes to quit, so assert via should_quit and a still-scrolled view.
        app.transcript_scroll.set(30);
        let _ = app.apply_key_with_mods(
            crossterm::event::KeyCode::Char('d'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert_eq!(
            app.transcript_scroll(),
            30,
            "bare Ctrl-D must not move the transcript"
        );
        assert!(app.should_quit, "bare Ctrl-D on empty input quits (EOF)");
    }

    #[test]
    fn slash_mouse_emits_set_capture_action_and_uses_i18n() {
        let mut app = fresh_app(Some("offline"));
        assert!(
            app.mouse_scroll,
            "mouse capture defaults ON (wheel-scroll + in-app drag-to-copy both work)"
        );
        // Toggling OFF must emit SetMouseCapture(false) so the event loop issues the
        // real DisableMouseCapture (handing selection back to the terminal), not just
        // flip a bool.
        let action = app.slash_toggle_mouse();
        assert_eq!(action, Action::SetMouseCapture(false));
        assert!(!app.mouse_scroll);
        // The pushed status line must be the i18n string, not a raw literal.
        let last = app.history.back().expect("a status line was pushed");
        assert_eq!(
            last.body(),
            umadev_i18n::t(app.lang, "slash.mouse_off"),
            "/mouse status text must come from the i18n catalog"
        );
        // Toggling back ON emits SetMouseCapture(true).
        let action = app.slash_toggle_mouse();
        assert_eq!(action, Action::SetMouseCapture(true));
        assert!(app.mouse_scroll);
    }

    #[test]
    fn submitting_a_turn_repins_transcript_to_bottom() {
        let mut app = fresh_app(Some("offline"));
        app.transcript_max_scroll.set(50);
        app.transcript_scroll.set(30); // user is reviewing history
        for c in "hello".chars() {
            let _ = app.apply_key(crossterm::event::KeyCode::Char(c));
        }
        let _ = app.apply_key(crossterm::event::KeyCode::Enter);
        assert_eq!(
            app.transcript_scroll(),
            0,
            "submitting must snap back to the newest content"
        );
    }

    #[test]
    fn slash_mouse_toggles_wheel_scroll_flag() {
        let mut app = fresh_app(Some("offline"));
        assert!(app.mouse_scroll, "mouse capture defaults on");
        for c in "/mouse".chars() {
            let _ = app.apply_key(crossterm::event::KeyCode::Char(c));
        }
        let _ = app.apply_key(crossterm::event::KeyCode::Enter);
        assert!(!app.mouse_scroll, "/mouse turns the capture binding off");
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
            .any(|m| m.role == ChatRole::Host && m.body() == "你好,我是底座"));

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

    /// Build an app rooted at a UNIQUE temp dir so the `.umadev/chat/` persistence
    /// tests don't collide with each other or the shared `/tmp/sd-test-ws-*` dirs.
    fn temp_app() -> (App, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = UserConfig {
            backend: Some("claude-code".to_string()),
            lang: Some("zh-CN".to_string()),
            ..Default::default()
        };
        let app = App::new(
            "demo",
            cfg,
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        (app, tmp)
    }

    #[test]
    fn pasted_image_path_becomes_a_chip_and_expands_to_an_at_mention() {
        let mut app = fresh_app(Some("offline"));
        let dir = tempfile::TempDir::new().unwrap();
        let img = dir.path().join("shot.png");
        std::fs::write(&img, b"\x89PNG\r\n\x1a\n").unwrap();
        app.handle_paste(img.to_str().unwrap());
        // A chip is shown in the input (not the raw path); one attachment tracked.
        assert!(
            app.input.contains("图片") || app.input.contains("Image"),
            "chip inserted, got: {}",
            app.input
        );
        assert_eq!(app.attachments.len(), 1);
        // On submit the chip rewrites to an @<abs-path> mention the base can open.
        let abs = std::fs::canonicalize(&img).unwrap();
        let expanded = app.expand_attachments(app.input.trim());
        assert!(
            expanded.contains(&format!("@{}", abs.display())),
            "expanded to @path, got: {expanded}"
        );
    }

    #[test]
    fn pasted_plain_text_with_a_png_word_is_verbatim_not_attached() {
        let mut app = fresh_app(Some("offline"));
        app.handle_paste("see the png export in the docs");
        assert_eq!(app.input, "see the png export in the docs");
        assert!(app.attachments.is_empty());
    }

    #[test]
    fn a_nonexistent_image_path_is_left_as_plain_text() {
        let mut app = fresh_app(Some("offline"));
        app.handle_paste("/no/such/dir/ghost.png");
        // Can't canonicalise → not attached → inserted verbatim, no chip.
        assert!(app.attachments.is_empty());
        assert!(app.input.contains("ghost.png"));
    }

    #[test]
    fn chat_persists_and_a_restart_reopens_the_conversation() {
        // Wave 5 / G11: a restart must reopen the SAME dialogue (no goldfish).
        let (mut app, tmp) = temp_app();
        app.record_user_turn("我在做一个看板应用");
        // A host chat turn captured the base's OWN resumable session id (claude's
        // pinned `--session-id`) — it must survive the restart so the base resumes its
        // DEEP context, not just the replayed transcript.
        app.record_agentic_done(
            "好的,已经开始搭建。".to_string(),
            false,
            Some("base-sess-kanban".to_string()),
        );
        let saved_id = app.chat_id.clone();
        assert_eq!(app.conversation.len(), 2);

        // Simulate a restart: a brand-new App over the SAME project root.
        let cfg = UserConfig {
            backend: Some("claude-code".to_string()),
            lang: Some("zh-CN".to_string()),
            ..Default::default()
        };
        let app2 = App::new(
            "demo",
            cfg,
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        // The most-recent saved chat is reopened: same id, same transcript.
        assert_eq!(app2.chat_id, saved_id, "restart reopens the saved chat id");
        assert_eq!(app2.conversation.len(), 2);
        assert_eq!(app2.conversation[0].content, "我在做一个看板应用");
        assert_eq!(app2.conversation[1].role, "assistant");
        // The base session id is restored so the NEXT host chat turn RESUMES the base's
        // deep context (the resident pre-load opens it via `--resume <id>`), and the
        // session is flagged active. This is the cross-session base-memory fix.
        assert_eq!(
            app2.chat_session_id.as_deref(),
            Some("base-sess-kanban"),
            "restart restores the base's resumable session id, not the chat file id"
        );
        assert!(
            app2.host_chat_session_active,
            "a restored base session id flags the chat session active"
        );
        // The restore note is surfaced so the user knows context was kept.
        assert!(app2
            .history
            .iter()
            .any(|m| m.role == ChatRole::System && m.body().contains("恢复")));
    }

    #[test]
    fn slash_sessions_lists_saved_chats_and_resume_reopens_one() {
        let (mut app, _tmp) = temp_app();
        // Chat A — capture its OWN base session id (claude's pinned `--session-id`).
        app.record_user_turn("第一个对话");
        app.record_agentic_done("reply A".to_string(), false, Some("base-A".to_string()));
        let id_a = app.chat_id.clone();
        // `/clear` starts a FRESH persistent chat (A stays on disk).
        let _ = app.try_slash_command("/clear");
        assert_ne!(app.chat_id, id_a, "/clear mints a new chat id");
        app.record_user_turn("第二个对话");
        app.record_agentic_done("reply B".to_string(), false, Some("base-B".to_string()));

        // `/sessions` lists BOTH saved chats.
        let _ = app.try_slash_command("/sessions");
        assert!(app
            .history
            .iter()
            .any(|m| m.body().contains(&id_a) && m.body().contains("已保存")));

        // `/resume <id_a>` reopens chat A's transcript. Clear the dirty flag first so
        // the assertion below proves `/resume` (not the earlier `/clear`) set it.
        app.chat_session_dirty = false;
        let _ = app.try_slash_command(&format!("/resume {id_a}"));
        assert_eq!(app.chat_id, id_a);
        assert_eq!(app.conversation[0].content, "第一个对话");
        // The base session is pinned to chat A's OWN persisted base session id
        // (`base-A`), NOT the chat FILE id (`id_a`) — the bug fix: a host CLI resumes
        // the conversation IT created, not an id it never saw. The resident session
        // is flagged dirty so the loop re-opens against the resumed base id.
        assert_eq!(app.chat_session_id.as_deref(), Some("base-A"));
        assert!(app.host_chat_session_active);
        assert!(
            app.chat_session_dirty,
            "/resume flags the resident session for re-open against the resumed base id"
        );
    }

    #[test]
    fn resume_unknown_id_is_fail_open() {
        let (mut app, _tmp) = temp_app();
        app.record_user_turn("hi");
        let before = app.conversation.clone();
        let _ = app.try_slash_command("/resume does-not-exist");
        // The live conversation is untouched; a clear note explains why.
        assert_eq!(app.conversation, before);
        assert!(app
            .history
            .iter()
            .any(|m| m.role == ChatRole::System && m.body().contains("没找到")));
    }

    /// (a) `ChatSession` round-trips `base_session_id`, and an OLD chat file written
    /// before the field existed deserializes to `None` (back-compat / fail-open).
    #[test]
    fn chat_session_round_trips_base_session_id_and_is_back_compat() {
        // New schema → the base session id survives a serialize/deserialize cycle.
        let s = ChatSession {
            id: "chat-1".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            backend: "claude-code".to_string(),
            base_session_id: Some("base-xyz".to_string()),
            messages: vec![umadev_runtime::Message {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: ChatSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.base_session_id.as_deref(), Some("base-xyz"));

        // OLD file (no `base_session_id` key) → `#[serde(default)]` yields `None`.
        let legacy = r#"{"id":"old","updated_at":"x","backend":"codex",
            "messages":[{"role":"user","content":"hi"}]}"#;
        let parsed: ChatSession = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            parsed.base_session_id, None,
            "an old chat file without the field loads as None (back-compat)"
        );
        assert_eq!(parsed.messages.len(), 1, "the transcript still loads");
    }

    /// (c) `persist_chat` writes the LIVE `chat_session_id` into the saved
    /// `base_session_id`; (b) `load_chat` restores it into `chat_session_id` and flags
    /// the host chat session active.
    #[test]
    fn persist_writes_and_load_restores_the_base_session_id() {
        let (mut app, _tmp) = temp_app();
        app.record_user_turn("第一句");
        // The live base session id (captured off a host turn) is persisted.
        app.chat_session_id = Some("base-live".to_string());
        app.persist_chat();
        let saved_id = app.chat_id.clone();

        // (c) The on-disk record carries the base session id.
        let path = app.chat_path(&saved_id);
        let text = std::fs::read_to_string(&path).unwrap();
        let on_disk: ChatSession = serde_json::from_str(&text).unwrap();
        assert_eq!(on_disk.base_session_id.as_deref(), Some("base-live"));

        // (b) A fresh App with the id cleared, then `load_chat`, restores it + flags.
        app.chat_session_id = None;
        app.host_chat_session_active = false;
        assert!(app.load_chat(&saved_id), "the saved chat loads");
        assert_eq!(
            app.chat_session_id.as_deref(),
            Some("base-live"),
            "load_chat restores the base session id"
        );
        assert!(
            app.host_chat_session_active,
            "a restored base session id flags the host chat session active"
        );
    }

    /// (b, fail-open) Loading a chat whose `base_session_id` is `None` (an old file /
    /// opencode / offline) leaves `chat_session_id` `None` and does NOT force the
    /// session active — degrading cleanly to today's fresh-session behavior.
    #[test]
    fn load_chat_with_no_base_session_id_is_fail_open() {
        let (mut app, _tmp) = temp_app();
        app.record_user_turn("仅文本");
        app.chat_session_id = None; // no base id captured (e.g. opencode)
        app.persist_chat();
        let saved_id = app.chat_id.clone();

        app.host_chat_session_active = false;
        assert!(app.load_chat(&saved_id));
        assert_eq!(
            app.chat_session_id, None,
            "no base session id → stays None (fresh session next turn)"
        );
        assert!(
            !app.host_chat_session_active,
            "a None base session id never force-flags the session active"
        );
    }

    #[test]
    fn slash_compact_folds_the_conversation_within_budget() {
        // Wave 5 / G11: /compact summarize-and-folds instead of FIFO-dropping, so a
        // long chat shrinks WITHOUT losing the whole early context.
        let (mut app, _tmp) = temp_app();
        for i in 0..12 {
            app.record_user_turn(&format!("user message {i}"));
            app.record_agentic_done(format!("assistant reply {i}"), false, None);
        }
        let before = app.conversation.len();
        let _ = app.try_slash_command("/compact");
        let after = app.conversation.len();
        assert!(
            after < before,
            "compact must shrink the buffer: {before}->{after}"
        );
        // The fold keeps a leading summary message that references the folded count.
        assert_eq!(app.conversation[0].role, "user");
        assert!(
            app.conversation[0].content.contains("摘要"),
            "first message after compact is the folded summary"
        );
        // The most-recent turn is preserved verbatim.
        assert_eq!(
            app.conversation.last().unwrap().content,
            "assistant reply 11"
        );
    }

    #[test]
    fn director_run_finish_hands_session_back_to_chat() {
        // Wave 5 deliverable 2: a finished director build hands its session to chat
        // so the next chat turn continues the SAME build session. The build-ness now
        // rides the terminal decision (`director_build: true`), NOT the pre-spawn
        // `director_run_in_flight` flag — the chat surface classifies in the task.
        let (mut app, _tmp) = temp_app();
        app.director_run_in_flight = true;
        app.record_agentic_done("built the app".to_string(), true, None);
        assert!(
            app.run_session_handed_to_chat,
            "a finished director build hands its session back to chat"
        );
        assert!(
            !app.director_run_in_flight,
            "the in-flight marker is cleared"
        );

        // A PLAIN chat turn (`director_build: false`) does NOT trigger the handoff,
        // even if the in-flight marker was left set.
        app.run_session_handed_to_chat = false;
        app.director_run_in_flight = true;
        app.record_agentic_done("just chatting".to_string(), false, None);
        assert!(
            !app.run_session_handed_to_chat,
            "a non-build turn never hands a session back"
        );
        assert!(
            !app.director_run_in_flight,
            "the in-flight marker is always cleared on a terminal turn"
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
        assert!(app
            .history
            .iter()
            .any(|m| m.body().contains("还没有可预览")));
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
    fn web_build_completion_card_has_files_entry_run_and_pending_preview() {
        // A finished web build's card shows what changed + the key entry + the
        // run command, and (when a dev server is detected) a "starting preview"
        // line — the "✅ done + here's the demo" finish.
        let (app, _tmp) = temp_app();
        let root = app.project_root.clone();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"dependencies":{"vite":"^5"},"scripts":{"dev":"vite"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("src").join("App.tsx"), "export default 1;").unwrap();

        // A web project resolves a dev-server target (Vite) → preview is pending.
        let target = app.auto_preview_target();
        assert!(
            target.is_some(),
            "vite project must resolve a preview target"
        );
        let card = app.build_completion_card(target.is_some());
        // Headline + the three substantive sections.
        assert!(card.contains("构建完成"), "card carries the done headline");
        assert!(
            card.contains("App.tsx"),
            "card names the key entry / a changed file"
        );
        assert!(
            card.contains("vite") || card.contains("npm run dev"),
            "card carries the run command: {card}"
        );
        // The "starting dev server…" placeholder shows because a server was found.
        assert!(
            card.contains(umadev_i18n::t(app.lang, "build.complete.preview_starting")),
            "web card flags the pending preview"
        );
    }

    #[test]
    fn non_web_build_completion_card_has_no_preview_line_fail_open() {
        // Fail-open: a non-web project detects no dev server → the card is still
        // produced (✅ done + what changed) but carries NO preview line and the
        // auto-preview target is None (so the event loop starts no server).
        let (app, _tmp) = temp_app();
        let root = app.project_root.clone();
        // A pure-Rust project: a main.rs but no package.json / index.html.
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();

        assert!(
            app.auto_preview_target().is_none(),
            "a non-web project resolves no preview target"
        );
        let card = app.build_completion_card(false);
        assert!(
            card.contains("构建完成"),
            "card still shows the done headline"
        );
        assert!(card.contains("main.rs"), "card names the rust entry");
        assert!(
            !card.contains(umadev_i18n::t(app.lang, "build.complete.preview_starting")),
            "non-web card must NOT show a preview-starting line"
        );
    }

    #[test]
    fn build_completion_card_falls_back_to_dirs_without_git_delta() {
        // No git repo (no porcelain delta) → the card still names a concrete
        // product directory instead of an empty "files changed" section.
        let (app, _tmp) = temp_app();
        std::fs::create_dir_all(app.project_root.join("src")).unwrap();
        let card = app.build_completion_card(false);
        assert!(card.contains("构建完成"));
        assert!(
            card.contains("src"),
            "falls back to naming an output dir: {card}"
        );
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
        assert!(first.body().contains("claude-code"));
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

    // --- Wave 1: intent card / live plan / team review event rendering ---

    #[test]
    fn intent_decided_pushes_intent_card_and_records_class() {
        let mut app = fresh_app(Some("offline"));
        let before = app.history.len();
        app.apply_engine(EngineEvent::IntentDecided {
            class: "build".into(),
            depth: "deep".into(),
            team: vec!["architect".into(), "qa".into()],
            est_tool_calls: 160,
            rationale: "完整构建,进研发流程".into(),
        });
        // A prominent UmaDev card landed in the transcript…
        assert!(app.history.len() > before);
        let card = app
            .history
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::UmaDev)
            .unwrap();
        // …carrying the BUILD headline, the rough budget, the team, and the reason.
        assert!(card.body().contains("160"), "shows the budget");
        assert!(card.body().contains("architect"), "shows the team");
        assert!(card.body().contains("研发流程"), "carries the rationale");
        // …and the class is recorded so the status chip can show it.
        assert_eq!(app.last_intent_class.as_deref(), Some("build"));
    }

    #[test]
    fn intent_decided_unknown_class_falls_open_to_chat_headline() {
        let mut app = fresh_app(Some("offline"));
        // A bogus class id must not panic and must not show a budget/team line.
        app.apply_engine(EngineEvent::IntentDecided {
            class: "totally-unknown".into(),
            depth: "weird".into(),
            team: vec![],
            est_tool_calls: 0,
            rationale: String::new(),
        });
        assert_eq!(app.last_intent_class.as_deref(), Some("totally-unknown"));
        // Still produced a card (the neutral chat headline), no crash.
        assert!(app.history.iter().any(|m| m.role == ChatRole::UmaDev));
    }

    #[test]
    fn plan_posted_then_step_status_drives_the_checklist() {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::PlanPosted {
            steps: vec![
                "s1 · scaffold the app (frontend)".into(),
                "s2 · login route (backend)".into(),
                "s3 · login form (frontend)".into(),
            ],
            done: 0,
            total: 3,
        });
        assert_eq!(app.plan_steps.len(), 3);
        assert_eq!(app.plan_steps[0].id, "s1");
        assert!(app.plan_steps[0].title.contains("scaffold"));
        assert!(app.plan_steps.iter().all(|s| s.status == "pending"));
        // A status transition ticks the matching step in place (not a new row).
        app.apply_engine(EngineEvent::PlanStepStatus {
            id: "s1".into(),
            title: "scaffold the app".into(),
            status: "done".into(),
        });
        assert_eq!(app.plan_steps.len(), 3, "no new row appended");
        assert_eq!(app.plan_steps[0].status, "done");
        // A status for an UNKNOWN id is appended, never dropped (fail-open).
        app.apply_engine(EngineEvent::PlanStepStatus {
            id: "s9".into(),
            title: "extra".into(),
            status: "active".into(),
        });
        assert_eq!(app.plan_steps.len(), 4);
        assert_eq!(app.plan_steps[3].id, "s9");
    }

    #[test]
    fn critic_verdict_records_and_replaces_per_seat() {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::CriticVerdict {
            seat: "architect".into(),
            accepts: true,
            blocking: vec![],
            advisory: vec!["consider a cache".into()],
        });
        app.apply_engine(EngineEvent::CriticVerdict {
            seat: "qa".into(),
            accepts: false,
            blocking: vec!["no tests".into(), "no error handling".into()],
            advisory: vec![],
        });
        assert_eq!(app.critic_verdicts.len(), 2);
        // A re-review of the SAME seat replaces its row (does not stack).
        app.apply_engine(EngineEvent::CriticVerdict {
            seat: "qa".into(),
            accepts: true,
            blocking: vec![],
            advisory: vec![],
        });
        assert_eq!(app.critic_verdicts.len(), 2, "seat replaced, not stacked");
        let qa = app.critic_verdicts.iter().find(|c| c.seat == "qa").unwrap();
        assert!(qa.accepts);
    }

    #[test]
    fn split_plan_summary_fails_open_on_odd_shape() {
        // Normal shape: `id · title (seat)`.
        let (id, title) = split_plan_summary("s2 · build the API (backend)", 1);
        assert_eq!(id, "s2");
        assert_eq!(title, "build the API (backend)");
        // No separator → positional id, whole string as title (never drops it).
        let (id, title) = split_plan_summary("just a bare title", 4);
        assert_eq!(id, "s4");
        assert_eq!(title, "just a bare title");
    }

    #[test]
    fn slash_plan_shows_usage_when_no_plan() {
        let mut app = fresh_app(Some("offline"));
        let action = app.try_slash_command("/plan").unwrap();
        assert_eq!(action, Action::None);
        // A "no active plan" hint + the usage line land (not silent).
        let joined: String = app.history.iter().map(|m| m.body().clone()).collect();
        assert!(joined.contains("/plan skip"), "usage shown: {joined}");
    }

    #[test]
    fn slash_plan_skip_folds_into_queued_steer() {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::PlanPosted {
            steps: vec![
                "s1 · scaffold (frontend)".into(),
                "s2 · login route (backend)".into(),
            ],
            done: 0,
            total: 2,
        });
        let action = app.try_slash_command("/plan skip s2").unwrap();
        assert_eq!(action, Action::None);
        // The skip directive is folded into the queued-steer queue (same-session
        // delivery), and it references the skipped step id.
        assert_eq!(app.queued_steer.len(), 1);
        assert!(app.queued_steer[0].contains("s2"));
        assert!(app.queued_steer[0].to_ascii_uppercase().contains("SKIP"));
    }

    #[test]
    fn slash_plan_unknown_step_does_not_queue() {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::PlanPosted {
            steps: vec!["s1 · only step (frontend)".into()],
            done: 0,
            total: 1,
        });
        let _ = app.try_slash_command("/plan veto s9").unwrap();
        // No such step → nothing queued, an honest "no step" note instead.
        assert!(app.queued_steer.is_empty());
        let joined: String = app.history.iter().map(|m| m.body().clone()).collect();
        assert!(joined.contains("s9"));
    }

    #[test]
    fn slash_plan_add_takes_free_text() {
        let mut app = fresh_app(Some("offline"));
        let _ = app
            .try_slash_command("/plan add write integration tests")
            .unwrap();
        assert_eq!(app.queued_steer.len(), 1);
        assert!(app.queued_steer[0].contains("write integration tests"));
        assert!(app.queued_steer[0].to_ascii_uppercase().contains("ADD"));
    }

    #[test]
    fn slash_plan_collapse_toggles_panel() {
        let mut app = fresh_app(Some("offline"));
        assert!(!app.plan_collapsed);
        let _ = app.try_slash_command("/plan collapse").unwrap();
        assert!(app.plan_collapsed);
        let _ = app.try_slash_command("/plan collapse").unwrap();
        assert!(!app.plan_collapsed);
    }

    #[test]
    fn new_run_clears_the_plan_and_review_panels() {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::PlanPosted {
            steps: vec!["s1 · do a thing (frontend)".into()],
            done: 0,
            total: 1,
        });
        app.apply_engine(EngineEvent::CriticVerdict {
            seat: "qa".into(),
            accepts: false,
            blocking: vec!["x".into()],
            advisory: vec![],
        });
        assert!(!app.plan_steps.is_empty() && !app.critic_verdicts.is_empty());
        app.reset_for_new_run();
        assert!(app.plan_steps.is_empty(), "plan cleared for a fresh run");
        assert!(app.critic_verdicts.is_empty(), "review cleared too");
    }

    #[test]
    fn critic_verdict_is_mirrored_into_the_transcript_with_full_findings() {
        // Defect 1: the panel collapses extra verdicts to "… +N"; the FULL set
        // (seat + every blocking finding) must always reach the scrollable
        // transcript so nothing is lost behind the clip.
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::CriticVerdict {
            seat: "frontend-engineer".into(),
            accepts: false,
            blocking: vec![
                "API contract drift: /login missing".into(),
                "no error states on the form".into(),
            ],
            advisory: vec![],
        });
        let joined: String = app.history.iter().map(|m| m.body().clone()).collect();
        assert!(joined.contains("[frontend-engineer]"), "seat in transcript");
        assert!(
            joined.contains("API contract drift: /login missing"),
            "first must-fix in transcript: {joined}"
        );
        assert!(
            joined.contains("no error states on the form"),
            "second must-fix (beyond the panel's first-line inline) in transcript"
        );
    }

    #[test]
    fn a_new_review_round_replaces_the_previous_rounds_seats() {
        // Defect 2a: round 1 blocks with three seats; a plan-step transition seals
        // the round; round 2 has a single passing seat. The panel must show ONLY
        // round 2's seat, not a stale mix of both rounds.
        let mut app = fresh_app(Some("offline"));
        for seat in ["frontend-engineer", "backend-engineer", "qa"] {
            app.apply_engine(EngineEvent::CriticVerdict {
                seat: seat.into(),
                accepts: false,
                blocking: vec!["fix it".into()],
                advisory: vec![],
            });
        }
        assert_eq!(app.critic_verdicts.len(), 3, "round 1 has three seats");
        // Work resumes (the director drives the next step) → the round is sealed.
        app.apply_engine(EngineEvent::PlanStepStatus {
            id: "s1".into(),
            title: "rework".into(),
            status: "active".into(),
        });
        // Round 2: a single seat now passes.
        app.apply_engine(EngineEvent::CriticVerdict {
            seat: "qa".into(),
            accepts: true,
            blocking: vec![],
            advisory: vec![],
        });
        assert_eq!(
            app.critic_verdicts.len(),
            1,
            "the new round replaced the old one, not a stale mix"
        );
        assert_eq!(app.critic_verdicts[0].seat, "qa");
        assert!(app.critic_verdicts[0].accepts, "shows the CURRENT round");
    }

    #[test]
    fn contiguous_verdicts_in_one_round_do_not_clear_each_other() {
        // The seal must NOT fire between two seats of the SAME round (no work
        // event interleaves a review burst), so both seats accumulate.
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::CriticVerdict {
            seat: "architect".into(),
            accepts: true,
            blocking: vec![],
            advisory: vec![],
        });
        app.apply_engine(EngineEvent::CriticVerdict {
            seat: "qa".into(),
            accepts: false,
            blocking: vec!["no tests".into()],
            advisory: vec![],
        });
        assert_eq!(app.critic_verdicts.len(), 2, "one round keeps both seats");
    }

    #[test]
    fn delivery_finish_clears_the_live_plan_and_review_panels() {
        // Defect 2b: a finished run must not leave a stale live plan / verdict
        // list hanging below the transcript — the terminal transition clears them
        // and folds the round into a one-line summary in the transcript.
        let mut app = fresh_app(Some("offline"));
        app.run_started = true;
        app.apply_engine(EngineEvent::PlanPosted {
            steps: vec!["s1 · ship it (frontend)".into()],
            done: 0,
            total: 1,
        });
        app.apply_engine(EngineEvent::CriticVerdict {
            seat: "qa".into(),
            accepts: true,
            blocking: vec![],
            advisory: vec![],
        });
        assert!(!app.plan_steps.is_empty() && !app.critic_verdicts.is_empty());
        app.apply_engine(EngineEvent::BlockCompleted {
            final_phase: Phase::Delivery,
            paused_at: None,
        });
        assert!(app.finished, "the run reached its terminal delivery state");
        assert!(
            app.plan_steps.is_empty(),
            "the live plan panel is cleared on finish"
        );
        assert!(
            app.critic_verdicts.is_empty(),
            "the live team-review panel is cleared on finish"
        );
        // The verdict content isn't silently dropped — a settle summary lands.
        let joined: String = app.history.iter().map(|m| m.body().clone()).collect();
        assert!(
            joined.contains(umadev_i18n::t(app.lang, "plan.review.title")),
            "a team-review settle summary is folded into the transcript: {joined}"
        );
    }

    #[test]
    fn an_aborted_block_clears_the_live_plan_and_review_panels() {
        // Defect 2b (abort branch): a bailed round is terminal too — its panels
        // must not linger as stale state.
        let mut app = fresh_app(Some("offline"));
        app.run_started = true;
        app.apply_engine(EngineEvent::PlanPosted {
            steps: vec!["s1 · do a thing (frontend)".into()],
            done: 0,
            total: 1,
        });
        app.apply_engine(EngineEvent::CriticVerdict {
            seat: "qa".into(),
            accepts: false,
            blocking: vec!["broken".into()],
            advisory: vec![],
        });
        assert!(!app.plan_steps.is_empty() && !app.critic_verdicts.is_empty());
        app.apply_engine(EngineEvent::Note(format!(
            "{}本轮已中止:磁盘写入失败",
            crate::ABORT_SENTINEL
        )));
        assert!(app.aborted, "the sentinel flips the run into aborted");
        assert!(app.plan_steps.is_empty(), "plan panel cleared on abort");
        assert!(
            app.critic_verdicts.is_empty(),
            "team-review panel cleared on abort"
        );
    }

    // --- Wave 1: honest picker auth state (gap G10) ---

    #[test]
    fn parse_probe_detail_unpacks_packed_auth_metadata() {
        // The packed shape spawn_probe emits.
        let s = PROBE_AUTH_SENTINEL;
        let packed = format!(
            "{s}auth=not_logged_in|login=claude auth login|install=npm i -g x{s}claude 1.6.0",
        );
        let (auth, login, install, human) = parse_probe_detail(&packed);
        assert_eq!(auth, AuthMark::NotLoggedIn);
        assert_eq!(login, "claude auth login");
        assert_eq!(install, "npm i -g x");
        assert_eq!(human, "claude 1.6.0");
        // Fail-open: a plain (untagged) detail keeps the human text, Unknown auth.
        let (auth, login, _i, human) = parse_probe_detail("claude 1.6.0");
        assert_eq!(auth, AuthMark::Unknown);
        assert!(login.is_empty());
        assert_eq!(human, "claude 1.6.0");
    }

    // Drive a probe through the engine, then select that base in the picker.
    fn probe_and_select(app: &mut App, id: &str, auth: &str, login: &str, install: &str) {
        let s = PROBE_AUTH_SENTINEL;
        let detail = format!("{s}auth={auth}|login={login}|install={install}{s}{id} 1.0.0");
        // `ready` mirrors spawn_probe: true only when logged in.
        app.apply_engine(EngineEvent::BackendProbed {
            backend_id: id.into(),
            ready: auth == "logged_in",
            detail,
        });
        app.goto_picker_step(PickerStep::BaseCli);
        let idx = app
            .picker_items
            .iter()
            .position(|i| i.backend_id.as_deref() == Some(id))
            .unwrap();
        app.picker_selected = idx;
    }

    #[test]
    fn picker_blocks_commit_on_not_logged_in_with_login_cmd() {
        let mut app = fresh_app(None);
        probe_and_select(
            &mut app,
            "claude-code",
            "not_logged_in",
            "claude auth login",
            "npm i -g claude",
        );
        let action = app.apply_key(KeyCode::Enter);
        // Commit is BLOCKED — stays on the picker with the login command surfaced.
        assert_eq!(action, Action::None);
        assert_eq!(app.mode, AppMode::Picker);
        let notice = app.picker_notice.as_deref().unwrap_or("");
        assert!(
            notice.contains("claude auth login"),
            "login cmd shown: {notice}"
        );
    }

    #[test]
    fn picker_blocks_commit_on_not_installed_with_install_cmd() {
        let mut app = fresh_app(None);
        probe_and_select(
            &mut app,
            "codex",
            "not_installed",
            "codex login",
            "npm install -g @openai/codex",
        );
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::None);
        assert_eq!(app.mode, AppMode::Picker);
        let notice = app.picker_notice.as_deref().unwrap_or("");
        assert!(
            notice.contains("npm install -g @openai/codex"),
            "install cmd shown: {notice}"
        );
    }

    #[test]
    fn picker_commits_when_logged_in() {
        let mut app = fresh_app(None);
        probe_and_select(
            &mut app,
            "claude-code",
            "logged_in",
            "claude auth login",
            "",
        );
        let action = app.apply_key(KeyCode::Enter);
        // A logged-in base commits straight into chat.
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
            app.history.front().unwrap().body(),
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
            .any(|m| m.body().contains("还没启动流水线") || m.body().contains("没有打开的 gate")));
    }

    #[test]
    fn slash_continue_with_a_resumable_plan_resumes_instead_of_hinting() {
        // /continue on a FRESH session (no in-memory gate) with a persisted, resumable
        // director-loop run on disk must RE-ATTACH (Action::ResumeRun + a resuming
        // note), not show the "no pipeline started" restart hint.
        let mut app = fresh_app(Some("claude-code"));
        // Persist a plan with one Pending step + a workflow-state carrying the
        // requirement — exactly what an interrupted /run leaves behind.
        let plan = umadev_agent::Plan {
            steps: vec![umadev_agent::PlanStep {
                id: "a".into(),
                title: "build the login page".into(),
                seat: umadev_agent::Seat::FrontendEngineer,
                kind: umadev_agent::StepKind::Build,
                depends_on: vec![],
                acceptance: umadev_agent::AcceptanceSpec::SourcePresent,
                status: umadev_agent::StepStatus::Pending,
            }],
            risks: vec![],
            open_questions: vec![],
        };
        umadev_agent::save_plan(&plan, &app.project_root).unwrap();
        let mut state = umadev_agent::WorkflowState::new(umadev_spec::Phase::Frontend);
        state.slug = "demo".into();
        state.requirement = "做一个登录页".into();
        state.backend = "claude-code".into();
        umadev_agent::write_workflow_state(&app.project_root, &state).unwrap();

        let before = app.history.len();
        let action = app
            .try_slash_command("/continue")
            .expect("/continue is a slash command");
        assert_eq!(
            action,
            Action::ResumeRun("做一个登录页".to_string()),
            "a resumable run resumes with the persisted requirement"
        );
        // The trilingual resuming note was surfaced (not the restart hint).
        assert!(
            app.history
                .iter()
                .skip(before)
                .any(|m| m.body().contains("续跑")),
            "the resuming note is shown"
        );
        assert!(
            !app.history
                .iter()
                .skip(before)
                .any(|m| m.body().contains("还没启动流水线")),
            "the restart hint is NOT shown"
        );
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
        assert!(app.history.iter().any(|m| m.body().contains("/revise")));
    }

    #[test]
    fn slash_goal_with_objective_starts_a_goal_driven_build() {
        // `/goal <objective>` → a goal-driven director build (StartGoal), carrying
        // the whole arg as the objective (no slug parsing — a goal is a sentence).
        let mut app = fresh_app(Some("offline"));
        for c in "/goal build a shippable todo app".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(
            action,
            Action::StartGoal("build a shippable todo app".to_string())
        );
        // A goal acknowledgement was surfaced to the user (the `goal.starting` line).
        assert!(app
            .history
            .iter()
            .any(|m| m.body().contains("build a shippable todo app")));
    }

    #[test]
    fn slash_goal_without_objective_is_noop_with_usage_hint() {
        // Empty `/goal` → a usage hint, no build kicked off.
        let mut app = fresh_app(Some("offline"));
        for c in "/goal".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::None);
        assert!(app.history.iter().any(|m| m.body().contains("/goal")));
    }

    #[test]
    fn goal_is_a_registered_slash_verb() {
        // The `/goal` verb is in the palette so completion + help surface it.
        assert!(App::COMMANDS.iter().any(|c| c.name == "goal"));
    }

    /// Parse the canonical verbs from the dispatch `match` arms by reading THIS
    /// source between the `COMMAND-DISPATCH-START/END` sentinels. An arm head is
    /// a line that (after trimming) starts with a string literal and contains
    /// `=>`; its `|`-separated quoted literals are the verbs it handles. The `_`
    /// fallback sits past the END sentinel, so dynamic per-backend ids are
    /// excluded. This reads the REAL dispatcher, so it can't drift from it.
    fn dispatch_arm_verbs() -> Vec<String> {
        let src = include_str!("app.rs");
        let start = src
            .find("// COMMAND-DISPATCH-START")
            .expect("dispatch start sentinel present");
        let end = src
            .find("// COMMAND-DISPATCH-END")
            .expect("dispatch end sentinel present");
        assert!(end > start, "END sentinel follows START");
        let mut verbs = Vec::new();
        for line in src[start..end].lines() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with('"') {
                continue;
            }
            let Some(arrow) = trimmed.find("=>") else {
                continue;
            };
            for part in trimmed[..arrow].split('|') {
                let part = part.trim();
                if let Some(inner) = part.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                    verbs.push(inner.to_string());
                }
            }
        }
        verbs
    }

    #[test]
    fn commands_and_dispatch_are_in_lockstep() {
        // The ONE-registry invariant (UX maturity Fix A): the palette, the help
        // overlay, and the dispatcher all read `App::COMMANDS`. This test locks
        // the registry against the actual dispatch arms so the three surfaces can
        // never drift again (the historical bugs: `/model` dispatchable yet not
        // in the palette; a dozen verbs missing from help; aliases only in
        // dispatch). Mirrors how a mature TUI locks its built-in command names.
        let dispatch = dispatch_arm_verbs();
        assert!(
            dispatch.len() >= 40,
            "parsed the dispatch arms (got {}): {dispatch:?}",
            dispatch.len()
        );

        // (1) Every non-hidden registry command has a dispatch arm on its
        //     canonical name — the palette/help can't advertise an unwired verb.
        for c in App::COMMANDS {
            if c.hidden {
                continue;
            }
            assert!(
                dispatch.iter().any(|v| v == c.name),
                "/{} is in COMMANDS but has no dispatch arm",
                c.name
            );
            // Each alias resolves CENTRALLY back to its command (aliases live only
            // in the registry now), so a typed alias always reaches the handler.
            for alias in c.aliases {
                let resolved = App::resolve_command(alias);
                assert!(
                    resolved.is_some_and(|r| r.name == c.name),
                    "alias /{alias} of /{} does not resolve to it",
                    c.name
                );
            }
        }

        // (2) Every dispatch arm is a registered command name — a hand-added
        //     `match` arm that forgot the registry fails right here.
        for verb in &dispatch {
            assert!(
                App::COMMANDS.iter().any(|c| c.name == verb),
                "dispatch arm \"{verb}\" is not a registered COMMANDS name"
            );
        }

        // (3) Names + aliases are globally unique, so resolution is unambiguous.
        let mut seen = std::collections::HashSet::new();
        for c in App::COMMANDS {
            assert!(seen.insert(c.name), "duplicate command name /{}", c.name);
            for alias in c.aliases {
                assert!(
                    seen.insert(*alias),
                    "alias /{alias} collides with another verb"
                );
            }
            // Every description key must be present in the catalog (resolves to a
            // real string, not the key echoed back) so no palette/help row is blank.
            assert_ne!(
                umadev_i18n::t(umadev_i18n::Lang::En, c.desc_key),
                c.desc_key,
                "/{} desc_key {} is missing from the i18n catalog",
                c.name,
                c.desc_key
            );
        }
    }

    #[test]
    fn model_verb_is_registered_and_dispatchable() {
        // The exact historical drift the registry kills: `/model` was dispatched
        // + in help yet absent from the palette. Now it is one registry row that
        // all three surfaces read.
        assert!(
            App::COMMANDS.iter().any(|c| c.name == "model"),
            "/model is in the registry (so the palette suggests it)"
        );
        assert!(
            dispatch_arm_verbs().iter().any(|v| v == "model"),
            "/model has a dispatch arm"
        );
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
            .any(|m| m.body().contains("未知命令") && m.body().contains("/foo")));
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
        assert!(last.body().contains("本轮已中止"));
        assert!(
            !last.body().contains(crate::ABORT_SENTINEL),
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
        assert!(last.body().contains("Similar products"));
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
            .any(|m| m.role == ChatRole::UmaDev && m.body().contains("umadev.yaml")));
    }

    #[test]
    fn esc_during_active_pipeline_interrupts_the_run() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "build".into(),
        });
        assert!(a.is_pipeline_active());
        // Esc INTERRUPTS the running pipeline (like Claude Code), but a DELIBERATE
        // double-press — the first arms, the second cancels — so a stray keypress
        // can't nuke a long build. Neither press quits the app.
        assert_eq!(a.apply_key(KeyCode::Esc), Action::None);
        assert!(a.interrupt_armed(), "first Esc arms the interrupt");
        assert!(!a.should_quit);
        assert_eq!(a.apply_key(KeyCode::Esc), Action::Cancel);
        assert!(!a.should_quit);
    }

    #[test]
    fn interrupt_seals_a_half_streamed_reply_as_incomplete() {
        let mut a = fresh_app(Some("offline"));
        // Simulate a Host reply mid-stream.
        a.push(ChatRole::Host, "the answer so far".to_string());
        a.stream_text_active = true;
        a.cancel_run();
        let marker = umadev_i18n::t(a.lang, "chat.interrupted");
        let last = a
            .history
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::Host)
            .unwrap();
        assert!(
            last.body().contains(marker.trim()),
            "an interrupted reply must be marked incomplete: {:?}",
            last.body()
        );
        assert!(!a.stream_text_active, "the stream flag is cleared on seal");
    }

    #[test]
    fn seal_is_a_noop_when_nothing_was_streaming() {
        let mut a = fresh_app(Some("offline"));
        a.push(ChatRole::Host, "a finished reply".to_string());
        a.stream_text_active = false;
        a.seal_interrupted_stream();
        let last = a
            .history
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::Host)
            .unwrap();
        assert_eq!(
            last.body(),
            "a finished reply",
            "no marker when nothing streamed"
        );
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
        // Esc INTERRUPTS the agentic subprocess (parity with Ctrl-C) via a
        // deliberate double-press, and does NOT arm quit-confirm or drop the app.
        assert_eq!(a.apply_key(KeyCode::Esc), Action::None);
        assert!(a.interrupt_armed(), "first Esc arms the interrupt");
        assert_eq!(a.apply_key(KeyCode::Esc), Action::Cancel);
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
            .find(|m| m.body().contains("docs_confirm"))
            .expect("resume hint should mention the paused gate");
        assert_eq!(resume_msg.role, ChatRole::System);
        assert!(resume_msg.body().contains("做一个登录系统"));
        assert!(resume_msg.body().contains("/continue"));
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
            .find(|m| m.body().contains("上次跑完了") || m.body().contains("上次会话"))
            .expect("delivery-state should produce a chat hint");
        assert!(msg.body().contains("做个 todo"));
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
            .any(|m| m.body().contains("docs_confirm") || m.body().contains("上次")));
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
        assert!(a.history.iter().any(|m| m.body().contains("没有正在运行")));
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
        assert!(a.history.iter().any(|m| m.body().contains("已取消")));
    }

    #[test]
    fn slash_cancel_aborts_an_in_flight_agentic_round() {
        // P1-H: an agentic round (`agentic_in_flight`, but NOT a full pipeline) must
        // be cancellable via `/cancel`. The old pipeline-only check left it
        // un-cancellable from the prompt (only Ctrl-C worked).
        let mut a = fresh_app(Some("offline"));
        a.agentic_in_flight = true;
        assert!(
            !a.is_pipeline_active(),
            "an agentic round is not a pipeline"
        );
        for c in "/cancel".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        assert!(
            matches!(a.apply_key(KeyCode::Enter), Action::Cancel),
            "/cancel must abort an in-flight agentic round"
        );
    }

    #[test]
    fn aborted_run_free_text_routes_to_chat_not_a_dead_queue() {
        // P1-G: after an abort the run keeps `run_started = true`, `finished =
        // false`, `aborted = true`. Free text in that state must route to the base
        // as a fresh chat turn (Action::Route) — NOT get queued into `queued_steer`,
        // which never drains after an abort (no further phase/gate gaps), silently
        // swallowing the input.
        let mut a = fresh_app(Some("offline"));
        a.run_started = true;
        a.finished = false;
        a.aborted = true;
        assert!(!a.is_pipeline_active(), "an aborted run is not active");
        for c in "hello again".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(
            action,
            Action::Route("hello again".to_string()),
            "aborted-state free text must route to chat"
        );
        assert!(
            a.queued_steer.is_empty(),
            "aborted-state input must NOT land in the never-draining steer queue"
        );
    }

    #[test]
    fn slash_backend_is_rejected_during_an_active_run() {
        // P1-I: switching the base mid-run would leave the in-flight run on the old
        // base while config/UI claim the new one (a silent backend mismatch on the
        // next resume). `/backend` must refuse while a run is active and leave the
        // backend unchanged.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "build a dashboard".into(),
        });
        assert!(a.is_pipeline_active());
        let before = a.backend.clone();
        // `/codex` is the backend-switch verb (TUI uses per-base verbs, not
        // `/backend <id>`); it routes through `slash_backend`.
        for c in "/codex".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(
            action,
            Action::None,
            "mid-run base switch is a rejected no-op"
        );
        assert_eq!(a.backend, before, "the backend must be unchanged mid-run");
        assert!(
            a.history.iter().any(|m| m.body().contains("/cancel")),
            "the rejection tells the user to /cancel first"
        );
    }

    #[test]
    fn slash_backend_switches_when_no_run_is_active() {
        // The guard is scoped to an ACTIVE run only — switching at the idle prompt
        // still works exactly as before.
        let mut a = fresh_app(Some("offline"));
        assert!(!a.is_pipeline_active());
        for c in "/codex".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::BackendChanged);
        assert_eq!(a.backend.as_deref(), Some("codex"));
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
                .any(|m| m.body().contains("使用统计") || m.body().contains("还没有使用记录")),
            "partial /usag + Enter should run /usage"
        );
        assert!(
            !a.history.iter().any(|m| m.body().contains("未知命令")),
            "should not report unknown command for a resolvable partial"
        );
    }

    #[test]
    fn slash_model_explains_the_base_owns_the_model() {
        let mut a = fresh_app(Some("offline"));
        for c in "/model".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        // /model no longer switches anything — it explains the base owns the model
        // (the message names the bases, lang-agnostic check).
        assert!(a
            .history
            .iter()
            .any(|m| m.body().contains("codex") && m.body().contains("opencode")));
        // It never touches config.model.
        assert!(a.config.model.is_none());
    }

    #[test]
    fn slash_model_with_arg_does_not_set_a_model() {
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
        // UmaDev no longer manages the model: `/model <arg>` does NOT set or persist
        // a model — the base owns it. So config.model stays None, on disk too.
        assert_eq!(app.config.model, None);
        let loaded = crate::config::load_from(&cfg_path);
        assert_eq!(loaded.model, None);
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
            .any(|m| m.body().contains("还没有部署指令")));
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
            .any(|m| m.body().contains("npx vercel --prod")));
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
        assert!(last.body().contains("/quitz"));
        assert!(last.body().contains("/quit"));
        assert!(last.body().contains("是想用"));
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
        assert!(card.body().contains("output/demo-prd.md"));
        assert!(card.body().contains("output/demo-architecture.md"));
        assert!(card.body().contains("output/demo-uiux.md"));
        // Lists next-step verbs.
        assert!(card.body().contains("/continue"));
        assert!(card.body().contains("/revise"));
        assert!(card.body().contains("/diff"));
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
        assert!(card.body().contains("output/shop-frontend-notes.md"));
        assert!(card.body().contains("output/shop-execution-plan.md"));
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
        assert!(card.body().contains("审批清单"));
        assert!(card.body().contains("验收标准") || card.body().contains("验收"));
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
            .find(|m| m.body().contains("[fail]"))
            .expect("verify failure message");
        assert!(msg.body().contains("依赖未安装"), "got: {}", msg.body());
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
        assert!(!a.history.iter().any(|m| m.body().contains("直接描述需求")));
    }

    #[test]
    fn chinese_greeting_is_plain_chat_not_pipeline() {
        let mut a = fresh_app(Some("offline"));
        for c in "你好".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Route("你好".to_string()));
        assert!(!a.history.iter().any(|m| m.body().contains("收到需求")));
    }

    #[test]
    fn how_are_you_is_plain_chat_not_pipeline() {
        let mut a = fresh_app(Some("offline"));
        for c in "你好吗？我很好啊".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Route("你好吗？我很好啊".to_string()));
        assert!(!a.history.iter().any(|m| m.body().contains("流水线启动")));
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
            last.body().contains("还没启动流水线"),
            "expected redirect hint, got: {}",
            last.body()
        );
    }

    #[test]
    fn preflight_message_lands_when_starting_run() {
        let mut a = fresh_app(Some("offline"));
        a.prepare_worker_routed_run("build me a thing");
        // The UmaDev preflight message includes the 9-phase plan.
        assert!(a.history.iter().any(|m| m.role == ChatRole::UmaDev
            && m.body().contains("9 阶段")
            && m.body().contains("docs_confirm")
            && m.body().contains("preview_confirm")));
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
    fn slash_run_only_treats_a_separatored_ascii_first_word_as_a_slug() {
        // `todo-app` (ASCII + a `-` separator) IS the optional run slug.
        let mut a = fresh_app(Some("offline"));
        let _ = a.slash_run("todo-app 做一个待办应用");
        assert_eq!(a.slug, "todo-app");
        // A multi-word / Chinese requirement's first word is NOT mistaken for a
        // slug (no separator / not ASCII), so the whole thing stays the requirement
        // and no slug-invalid error fires (was: '/run with spaces' wrongly rejected).
        let mut b = fresh_app(Some("offline"));
        let _ = b.slash_run("做一个 带空格 的登录页");
        assert_ne!(
            b.slug, "做一个",
            "the first word must not become a phantom slug"
        );
    }

    #[test]
    fn palette_fuzzy_finds_deploy_from_dpl() {
        let mut a = fresh_app(Some("offline"));
        for c in "/dpl".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let verbs: Vec<&str> = a.palette_matches().iter().map(|p| p.verb).collect();
        assert!(
            verbs.contains(&"deploy"),
            "fuzzy /dpl → deploy, got: {verbs:?}"
        );
    }

    #[test]
    fn word_motion_jumps_across_words() {
        let mut a = fresh_app(Some("offline"));
        for c in "hello world foo".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        a.move_word_left();
        assert_eq!(a.input_cursor, 12, "→ start of last word 'foo'");
        a.move_word_left();
        assert_eq!(a.input_cursor, 6, "→ start of 'world'");
        a.move_word_right();
        assert_eq!(a.input_cursor, 12, "→ back to start of 'foo'");
    }

    #[test]
    fn palette_matches_filter_by_prefix() {
        let mut a = fresh_app(Some("offline"));
        for c in "/cl".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let matches = a.palette_matches();
        // /claude /clear → 2 matches.
        let verbs: Vec<&str> = matches.iter().map(|p| p.verb).collect();
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

    // ---- @-file-mention typeahead ----

    /// Seed a few files into the test workspace so the `@`-typeahead has real
    /// candidates to rank: `src/main.rs`, `src/lib.rs`, `README.md`.
    fn seed_mention_files(a: &App) {
        let root = &a.project_root;
        let _ = std::fs::create_dir_all(root.join("src"));
        let _ = std::fs::write(root.join("src/main.rs"), "fn main() {}\n");
        let _ = std::fs::write(root.join("src/lib.rs"), "// lib\n");
        let _ = std::fs::write(root.join("README.md"), "# readme\n");
    }

    #[test]
    fn mention_detects_partial_under_cursor() {
        let mut a = fresh_app(Some("offline"));
        for c in "look at @sr".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        assert_eq!(
            a.mention_token(),
            Some((8, "sr".to_string())),
            "the `@sr` token under the cursor is detected with its partial"
        );
    }

    #[test]
    fn mention_inactive_without_at_token() {
        let mut a = fresh_app(Some("offline"));
        for c in "hello world".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        assert_eq!(a.mention_token(), None, "no `@` → no mention context");
        assert!(a.mention_matches().is_empty(), "no `@` → no candidates");
        // An `@` glued to a preceding non-space (an email) must NOT open it.
        let mut b = fresh_app(Some("offline"));
        for c in "ping a@host".chars() {
            let _ = b.apply_key(KeyCode::Char(c));
        }
        assert_eq!(b.mention_token(), None, "`a@host` is not a file mention");
    }

    #[test]
    fn mention_candidates_filter_by_partial() {
        let mut a = fresh_app(Some("offline"));
        seed_mention_files(&a);
        for c in "@main".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let m = a.mention_matches();
        assert!(
            m.iter().any(|p| p == "src/main.rs"),
            "`@main` ranks src/main.rs, got {m:?}"
        );
        assert!(
            !m.iter().any(|p| p == "README.md"),
            "`README.md` is filtered out by the `main` partial, got {m:?}"
        );
    }

    #[test]
    fn mention_accept_inserts_path_and_replaces_partial() {
        let mut a = fresh_app(Some("offline"));
        seed_mention_files(&a);
        for c in "@main".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Tab);
        assert_eq!(
            a.input, "@src/main.rs ",
            "Tab replaced `@main` with the path"
        );
        assert_eq!(
            a.input_cursor,
            a.input_len(),
            "caret lands after the insert"
        );
        assert!(
            a.mention_matches().is_empty(),
            "the trailing space closes the popover"
        );
    }

    #[test]
    fn mention_enter_inserts_selected_path() {
        let mut a = fresh_app(Some("offline"));
        seed_mention_files(&a);
        for c in "@README".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        assert_eq!(a.input, "@README.md ", "Enter accepted the mention");
    }

    #[test]
    fn mention_popover_suppresses_slash_palette() {
        // A line that is BOTH a slash command and carries an `@`-token: the
        // mention popover wins, so Tab inserts the file path — not the slash
        // completion. Proves the two popovers are mutually exclusive.
        let mut a = fresh_app(Some("offline"));
        seed_mention_files(&a);
        for c in "/run @main".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        assert!(
            !a.mention_matches().is_empty(),
            "the `@main` token is active"
        );
        assert!(
            !a.palette_matches().is_empty(),
            "`/run` still matches the palette registry"
        );
        let _ = a.apply_key(KeyCode::Tab);
        assert_eq!(
            a.input, "/run @src/main.rs ",
            "Tab accepted the mention, not the slash autocomplete"
        );
    }

    #[test]
    fn mention_esc_closes_without_inserting() {
        let mut a = fresh_app(Some("offline"));
        seed_mention_files(&a);
        for c in "@main".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        assert!(!a.mention_matches().is_empty(), "popover open before Esc");
        let _ = a.apply_key(KeyCode::Esc);
        assert!(a.mention_dismissed, "Esc dismissed the popover");
        assert!(a.mention_matches().is_empty(), "popover closed after Esc");
        assert_eq!(a.input, "@main", "Esc left the prompt text untouched");
        // A further edit re-opens the popover (dismissal is not sticky).
        let _ = a.apply_key(KeyCode::Char('.'));
        assert!(
            !a.mention_matches().is_empty(),
            "editing re-opened the popover"
        );
    }

    #[test]
    fn mention_arrow_down_cycles_selection() {
        let mut a = fresh_app(Some("offline"));
        seed_mention_files(&a);
        // A bare `@` lists every file (≥2), so ↓ can move the highlight.
        let _ = a.apply_key(KeyCode::Char('@'));
        let count = a.mention_matches().len();
        assert!(count >= 2, "expected ≥2 candidates, got {count}");
        assert_eq!(a.mention_selected, 0, "starts on the first candidate");
        let _ = a.apply_key(KeyCode::Down);
        assert_eq!(a.mention_selected, 1, "↓ moved the mention highlight");
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
        // A real frame publishes `max_scroll` (the top-most reachable VISUAL row)
        // before any key is handled; simulate that so scroll_down has room to move.
        a.overlay.as_ref().unwrap().max_scroll.set(100);
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
        // End jumps to the published last reachable row (not a logical-line guess).
        let _ = a.apply_key(KeyCode::End);
        assert_eq!(a.overlay.as_ref().unwrap().scroll, 100);
    }

    #[test]
    fn overlay_wheel_scrolls_overlay_not_transcript() {
        let mut a = fresh_app(Some("offline"));
        // A tall transcript so a mis-routed wheel WOULD visibly move it.
        a.transcript_max_scroll.set(100);
        a.set_transcript_scroll(0);
        // Open an overlay (taller than the viewport — publish a non-zero max).
        for c in "/spec".chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let _ = a.apply_key(KeyCode::Enter);
        assert!(a.overlay.is_some());
        a.overlay.as_ref().unwrap().max_scroll.set(100);
        // Wheel DOWN with the overlay open scrolls the OVERLAY, not the transcript.
        assert!(a.mouse_wheel(false, 3));
        assert_eq!(a.overlay.as_ref().unwrap().scroll, 3);
        assert_eq!(
            a.transcript_scroll(),
            0,
            "transcript stays pinned while an overlay is open"
        );
        // PageDown (key path) advances further; End clamps to the published last row.
        let _ = a.apply_key(KeyCode::PageDown);
        assert!(a.overlay.as_ref().unwrap().scroll > 3);
        let _ = a.apply_key(KeyCode::End);
        assert_eq!(a.overlay.as_ref().unwrap().scroll, 100, "End clamps to max");
        // Wheeling past the end stays clamped — never overruns the last visual row.
        assert!(a.mouse_wheel(false, 50));
        assert_eq!(a.overlay.as_ref().unwrap().scroll, 100);
        // The modal owns the wheel even when the `/mouse` toggle is OFF.
        a.mouse_scroll = false;
        let _ = a.apply_key(KeyCode::Home);
        assert_eq!(a.overlay.as_ref().unwrap().scroll, 0);
        a.overlay.as_ref().unwrap().max_scroll.set(100);
        assert!(a.mouse_wheel(false, 5));
        assert_eq!(
            a.overlay.as_ref().unwrap().scroll,
            5,
            "overlay scrolls regardless of the /mouse wheel-capture toggle"
        );
        // With the overlay CLOSED, the wheel falls back to the transcript.
        a.overlay = None;
        a.mouse_scroll = true;
        a.transcript_max_scroll.set(100);
        a.set_transcript_scroll(0);
        assert!(a.mouse_wheel(true, 4));
        assert_eq!(
            a.transcript_scroll(),
            4,
            "no overlay → the wheel scrolls the transcript"
        );
    }

    #[test]
    fn slash_plan_includes_full_team_review_section() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PlanPosted {
            steps: vec![
                "s1 · scaffold (frontend)".into(),
                "s2 · login route (backend)".into(),
            ],
            done: 1,
            total: 2,
        });
        a.apply_engine(EngineEvent::CriticVerdict {
            seat: "architect".into(),
            accepts: true,
            blocking: vec![],
            advisory: vec!["consider a cache".into()],
        });
        a.apply_engine(EngineEvent::CriticVerdict {
            seat: "qa".into(),
            accepts: false,
            blocking: vec!["no tests for login".into(), "no error handling".into()],
            advisory: vec![],
        });
        assert_eq!(a.critic_verdicts.len(), 2);
        let before = a.history.len();
        let action = a.try_slash_command("/plan").unwrap();
        assert_eq!(action, Action::None);
        let out: String = a
            .history
            .iter()
            .skip(before)
            .map(|m| m.body().clone())
            .collect();
        // Plan steps still render.
        assert!(out.contains("s1") && out.contains("s2"), "plan steps shown");
        // EVERY seat's verdict is listed (truthful "/plan for all").
        assert!(out.contains("[architect]"), "accepting seat shown: {out}");
        assert!(out.contains("[qa]"), "blocking seat shown: {out}");
        // A blocking seat's FULL findings are listed, not just the first.
        assert!(out.contains("no tests for login"), "first finding: {out}");
        assert!(out.contains("no error handling"), "second finding: {out}");
    }

    #[test]
    fn team_command_registered_and_dispatchable() {
        // Wave C: `/team` is one registry row (so the palette + help advertise it)
        // AND has a dispatch arm — the lockstep parity test relies on both.
        assert!(
            App::COMMANDS.iter().any(|c| c.name == "team"),
            "/team is in COMMANDS"
        );
        assert!(
            dispatch_arm_verbs().iter().any(|v| v == "team"),
            "/team has a dispatch arm"
        );
    }

    #[test]
    fn slash_team_no_run_shows_roster_and_convene_hint() {
        // No plan, no verdicts, no output dir → roster + the "convenes on a build"
        // hint, never the run section.
        let mut a = fresh_app(Some("offline"));
        let before = a.history.len();
        let action = a.try_slash_command("/team").unwrap();
        assert_eq!(action, Action::None);
        let out: String = a
            .history
            .iter()
            .skip(before)
            .map(|m| m.body().clone())
            .collect();
        // Roster: the title + every seat's role→deliverable line is present.
        assert!(
            out.contains(umadev_i18n::t(a.lang, "team.title")),
            "title: {out}"
        );
        for key in TEAM_ROSTER {
            assert!(
                out.contains(umadev_i18n::t(a.lang, key)),
                "roster row {key} present: {out}"
            );
        }
        // No run context → the convene hint, NOT the run header.
        assert!(
            out.contains(umadev_i18n::t(a.lang, "team.no_run")),
            "hint: {out}"
        );
        assert!(
            !out.contains(umadev_i18n::t(a.lang, "team.run.header")),
            "no run section without context: {out}"
        );
    }

    #[test]
    fn slash_team_with_verdicts_shows_per_seat_verdicts() {
        // Recorded critic verdicts are run context → the run section lists each
        // convened seat with its verdict (reusing the team-review wording).
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::CriticVerdict {
            seat: "architect".into(),
            accepts: true,
            blocking: vec![],
            advisory: vec![],
        });
        a.apply_engine(EngineEvent::CriticVerdict {
            seat: "qa".into(),
            accepts: false,
            blocking: vec!["no tests for login".into()],
            advisory: vec![],
        });
        let before = a.history.len();
        let _ = a.try_slash_command("/team").unwrap();
        let out: String = a
            .history
            .iter()
            .skip(before)
            .map(|m| m.body().clone())
            .collect();
        assert!(
            out.contains(umadev_i18n::t(a.lang, "team.run.header")),
            "run header: {out}"
        );
        assert!(out.contains("[architect]"), "accepting seat: {out}");
        assert!(out.contains("[qa]"), "blocking seat: {out}");
    }

    #[test]
    fn slash_team_reports_produced_vs_pending_deliverables() {
        // A deliverable that exists on disk renders `produced`; one that does not
        // renders `pending`. Need run context for the deliverables block to show.
        let mut a = fresh_app(Some("offline"));
        let out_dir = a.project_root.join("output");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("demo-prd.md"), "# PRD").unwrap();
        a.apply_engine(EngineEvent::CriticVerdict {
            seat: "pm".into(),
            accepts: true,
            blocking: vec![],
            advisory: vec![],
        });
        let before = a.history.len();
        let _ = a.try_slash_command("/team").unwrap();
        let out: String = a
            .history
            .iter()
            .skip(before)
            .map(|m| m.body().clone())
            .collect();
        let produced = umadev_i18n::t(a.lang, "team.run.produced");
        let pending = umadev_i18n::t(a.lang, "team.run.pending");
        let prd = umadev_i18n::t(a.lang, "team.deliverable.prd");
        let deploy = umadev_i18n::t(a.lang, "team.deliverable.deploy");
        // The written PRD shows produced; the absent deploy proof shows pending.
        assert!(
            out.contains(&format!("{produced} {prd}")),
            "PRD produced: {out}"
        );
        assert!(
            out.contains(&format!("{pending} {deploy}")),
            "deploy proof pending: {out}"
        );
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
        let body = &host_msgs[0].body();
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
    fn p5c_thinking_collapses_to_one_summary_row() {
        // P5c: a burst of Thinking events opens exactly ONE placeholder row; the
        // next real content collapses it to a single `正在思考… · N.Ns` summary
        // instead of leaving a stack of orphan `[thinking]` rows.
        let mut a = fresh_app(Some("offline"));
        let before = a.history.len();
        for _ in 0..5 {
            a.apply_engine(EngineEvent::WorkerStream {
                event: umadev_runtime::StreamEvent::Thinking,
            });
        }
        // Only ONE placeholder row was pushed despite five Thinking events.
        assert_eq!(
            a.history.len(),
            before + 1,
            "a thinking burst must not stack rows"
        );
        let placeholder_idx = a.history.len() - 1;
        assert!(a
            .history
            .back()
            .unwrap()
            .body()
            .contains(THINKING_PLACEHOLDER_TAG));
        assert!(a.thinking_block_idx.is_some(), "a reasoning block is open");
        // Real content arrives → the placeholder collapses to a summary in place
        // (no new row added for the collapse itself).
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: "here is the answer".into(),
            },
        });
        assert!(a.thinking_block_idx.is_none(), "block closed after content");
        let collapsed = a.history.get(placeholder_idx).unwrap().body().into_owned();
        assert!(
            !collapsed.contains(THINKING_PLACEHOLDER_TAG),
            "placeholder tag is gone after collapse: {collapsed:?}"
        );
        // The summary carries the thinking label + a seconds figure (`· N.Ns`).
        assert!(
            collapsed.contains('·') && collapsed.contains('s'),
            "summary shows elapsed seconds: {collapsed:?}"
        );
    }

    #[test]
    fn p5c_collapse_failopen_without_timing() {
        // Fail-open: if the block-start timestamp is missing, the collapse still
        // rewrites the placeholder (to a no-seconds completion marker), never
        // leaving an orphan `[thinking]` row.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Thinking,
        });
        let idx = a.thinking_block_idx.unwrap();
        a.thinking_block_start = None; // simulate lost timing
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolResult {
                ok: true,
                summary: "done".into(),
            },
        });
        let row = a.history.get(idx).unwrap().body().into_owned();
        assert!(
            !row.contains(THINKING_PLACEHOLDER_TAG),
            "placeholder still collapsed without timing: {row:?}"
        );
    }

    #[test]
    fn thinking_deltas_accumulate_into_one_collapsed_block() {
        // Phase-2-C-P0: a stream of reasoning deltas must build ONE foldable
        // `[thinking]` block (not a row per delta), default collapsed, and the
        // reasoning text must be preserved in that single row.
        let mut a = fresh_app(Some("offline"));
        let before = a.history.len();
        for chunk in ["Let me ", "think about ", "the architecture."] {
            a.apply_engine(EngineEvent::WorkerStream {
                event: umadev_runtime::StreamEvent::ThinkingDelta(chunk.into()),
            });
        }
        // Exactly ONE new row despite three deltas.
        assert_eq!(
            a.history.len(),
            before + 1,
            "reasoning deltas must fold into one block, not a row per delta"
        );
        let idx = a.thinking_block_idx.expect("a reasoning block is open");
        let body = a.history.get(idx).unwrap().body().into_owned();
        assert!(
            body.starts_with(THINKING_PLACEHOLDER_TAG),
            "header tag: {body:?}"
        );
        assert!(
            body.contains("Let me think about the architecture."),
            "the full reasoning text is accumulated: {body:?}"
        );
        // Default collapsed, and recognized as a foldable reasoning block.
        let msg = a.history.get(idx).unwrap();
        assert!(msg.collapsed, "the reasoning block defaults to collapsed");
        assert!(
            crate::app::is_thinking_reasoning_block(msg.role, body.as_str()),
            "row is a foldable [thinking] reasoning block"
        );
        // Real content closes the block but KEEPS the reasoning + the expandable tag.
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: "Here is the plan.".into(),
            },
        });
        assert!(a.thinking_block_idx.is_none(), "block closed after content");
        let after = a.history.get(idx).unwrap();
        let after_body = after.body().into_owned();
        assert!(
            after_body.starts_with(THINKING_PLACEHOLDER_TAG),
            "a reasoning block keeps its expandable tag after collapse: {after_body:?}"
        );
        assert!(
            after_body.contains("Let me think about the architecture."),
            "the reasoning survives collapse so it can be expanded: {after_body:?}"
        );
        assert!(after.collapsed, "still collapsed (expand with Ctrl+O)");
    }

    #[test]
    fn a_turn_with_no_thinking_shows_no_reasoning_block() {
        // A plain answer with no reasoning deltas must add NO `[thinking]` block.
        let mut a = fresh_app(Some("offline"));
        let before = a.history.len();
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: "just an answer".into(),
            },
        });
        assert!(a.thinking_block_idx.is_none(), "no block opened");
        assert!(
            a.history
                .iter()
                .skip(before)
                .all(|m| !m.body().contains(THINKING_PLACEHOLDER_TAG)),
            "no [thinking] row when the turn never reasoned"
        );
    }

    #[test]
    fn p5d_spinner_frame_static_when_animations_off() {
        // P5d: animations off → a single static glyph, never a strobing frame.
        for tick in 0..30u8 {
            assert_eq!(
                spinner_frame(tick, false, false),
                SPINNER_STATIC,
                "animations off must be static at tick {tick}"
            );
        }
    }

    #[test]
    fn p5d_spinner_frame_freezes_on_stall() {
        // P5d: a stall FREEZES the spinner on one frame (the status surface paints
        // it the warning color) — it must not keep fake-spinning.
        let frozen = spinner_frame(0, true, true);
        for tick in 0..30u8 {
            assert_eq!(
                spinner_frame(tick, true, true),
                frozen,
                "stalled spinner must not advance (tick {tick})"
            );
        }
        // And while NOT stalled it does advance through the braille frames.
        let mut seen = std::collections::HashSet::new();
        for tick in 0..10u8 {
            seen.insert(spinner_frame(tick, true, false));
        }
        assert_eq!(seen.len(), SPINNER_FRAMES.len(), "all frames appear");
    }

    #[test]
    fn p5d_app_spinner_uses_shared_frames() {
        // The App-level spinner funnels through the shared frame source, so a
        // non-animated app shows the static glyph and an animated one rotates.
        let mut a = fresh_app(Some("offline"));
        a.animations = false;
        assert_eq!(a.spinner(), SPINNER_STATIC);
        a.animations = true;
        assert_eq!(
            a.spinner(),
            SPINNER_FRAMES[a.tick as usize % SPINNER_FRAMES.len()]
        );
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
    fn stall_after_threshold_then_clears_on_output() {
        // Honest stall signal: a running phase with no output past the 60s
        // threshold reads as stalled (status painted red by the UI); any fresh
        // output clears it. A short quiet spell (the base thinking) is NOT a stall.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PhaseStarted {
            phase: Phase::Research,
        });
        // Just started → not stalled (spin-up grace).
        assert!(!a.is_stalled(), "a just-started phase is not stalled");
        // A 30s quiet spell is normal base thinking, NOT a stall.
        a.last_output_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(30));
        assert!(!a.is_stalled(), "a sub-60s quiet spell is not a stall");
        // Backdate the last-output clock past the 60s threshold.
        a.last_output_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90));
        assert!(a.is_stalled(), "no output for >60s must read as stalled");
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
        // A long tool call (e.g. a multi-minute npm install) is WORK, not a stall
        // — the red signal must stay suppressed while a ToolUse has no ToolResult
        // yet, even past the 60s threshold; the ToolResult re-arms the stall clock.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PhaseStarted {
            phase: Phase::Backend,
        });
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Bash".into(),
                detail: "npm install".into(),
                edit: None,
            },
        });
        assert!(a.tool_in_progress, "ToolUse marks a tool in flight");
        // Even with a clock well past the 60s threshold, an in-flight tool is not
        // a stall (otherwise a long `npm install` would falsely flash red).
        a.last_output_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(120));
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
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90));
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

    #[test]
    fn pre_phase_window_stalls_after_three_seconds() {
        // THE 0/9 WINDOW: a run has STARTED (PipelineStarted) but no `Running`
        // phase has begun yet (cold index build / intake / vector build). Here
        // `phase_started_at` is None and `thinking` is false — the old judge
        // would NEVER go red, so a silent freeze in this window read as smooth.
        // The structural backstop must paint it red past 60s.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "build it".into(),
        });
        assert!(a.phase_started_at.is_none(), "no Running phase yet (0/9)");
        assert!(!a.thinking, "not a chat-thinking turn");
        // Just launched → not stalled (spin-up grace).
        assert!(!a.is_stalled(), "a just-launched run is not stalled");
        // Backdate the run start past the 60s threshold (no output has arrived).
        a.run_started_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90));
        assert!(
            a.is_stalled(),
            "a silent pre-phase 0/9 window past 60s MUST read as stalled"
        );
    }

    #[test]
    fn pre_phase_gate_pause_is_not_stalled() {
        // The pre-phase backstop must NOT misfire at a gate: GateOpened clears
        // run_started_at and sets active_gate, so a human pause never flashes red.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "build it".into(),
        });
        a.apply_engine(EngineEvent::GateOpened {
            gate: umadev_agent::gates::Gate::DocsConfirm,
        });
        // Even with a very stale clock, a gate pause is not a stall.
        a.run_started_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90));
        assert!(
            !a.is_stalled(),
            "a gate pause in the pre-phase window must not read as stalled"
        );
        // A finished/aborted run is likewise never stalled.
        let mut done = fresh_app(Some("offline"));
        done.run_started = true;
        done.aborted = true;
        done.run_started_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90));
        assert!(!done.is_stalled(), "an aborted run is not stalled");
    }

    #[test]
    fn build_brain_failure_drives_aborted_terminal_state() {
        // Fix 3: a `build_brain` init failure (unknown backend / driver build
        // error) carries the ABORT_SENTINEL like the other terminal paths, so the
        // bar flips to `[aborted]` instead of sitting at a fake idle "0/9".
        let mut app = fresh_app(Some("offline"));
        app.run_started = true;
        app.run_started_at = Some(std::time::Instant::now());
        // Emulate the wrapped terminal note spawn_block now emits on init failure.
        app.apply_engine(EngineEvent::Note(format!(
            "{}{}",
            crate::ABORT_SENTINEL,
            umadev_i18n::tlf("worker.init_failed", &["claude", "not on PATH"])
        )));
        assert!(
            app.aborted,
            "an init-failure sentinel note flips to aborted"
        );
        assert!(
            !app.is_pipeline_active(),
            "the failed run is no longer active"
        );
        app.refresh_status();
        assert!(
            app.status.contains("aborted"),
            "the bar shows [aborted], not a fake idle 0/9"
        );
    }

    #[test]
    fn config_save_failure_pushes_a_note_on_lang_change() {
        // Fix 4: a persist failure on `/lang` must surface a note, not silently
        // claim success and revert on next launch. Point config_path under a
        // regular FILE so `create_dir_all(parent)` inside save_to fails.
        let mut app = fresh_app(Some("offline"));
        let blocker = std::env::temp_dir().join(format!("sd-cfg-blocker-{}", std::process::id()));
        std::fs::write(&blocker, b"x").unwrap();
        app.config_path = blocker.join("nested").join("config.toml");
        let before = app.history.len();
        let _ = app.slash_lang("en");
        let pushed: Vec<String> = app
            .history
            .iter()
            .skip(before)
            .map(|m| m.body().into_owned())
            .collect();
        assert!(
            pushed.iter().any(|b| b.contains("[warn]")),
            "a config persist failure must push a warning note: {pushed:?}"
        );
        // The language still changed for this session (fail-open).
        assert_eq!(app.lang, umadev_i18n::Lang::En);
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn chat_reply_claiming_edits_gets_an_unverified_warning() {
        // Fix 5: a pure-chat reply that recites an edit ("已修改…/新增了…") —
        // with no run and no agentic tool calls — gets a reality-anchor note.
        let mut app = fresh_app(Some("offline"));
        app.record_chat_reply("我已修改了 app.rs 并新增了一个函数".to_string());
        assert!(
            app.history.iter().any(|m| m.body().contains("[warn]")),
            "a chat reply claiming code changes must get a verify warning"
        );
        // A benign chat reply (no change claim) must NOT be warned.
        let mut benign = fresh_app(Some("offline"));
        benign.record_chat_reply("你好,有什么可以帮你的?".to_string());
        assert!(
            !benign.history.iter().any(|m| m.body().contains("[warn]")),
            "a plain chat reply must not trigger the warning"
        );
    }

    #[test]
    fn clarify_answer_write_failure_does_not_claim_recorded() {
        // Fix 6: when the clarify answer can't be persisted, the user must be
        // told it was NOT recorded — never the false "已记录" line.
        let mut app = fresh_app(Some("offline"));
        // Point the project root at a regular FILE so the output/ dir can't be
        // created and the answer write fails.
        let blocker =
            std::env::temp_dir().join(format!("sd-clarify-blocker-{}", std::process::id()));
        std::fs::write(&blocker, b"x").unwrap();
        app.project_root = blocker.clone();
        app.active_gate = Some(Gate::ClarifyGate);
        for c in "use postgres".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let _ = app.apply_key(KeyCode::Enter);
        let last = app.history.back().unwrap();
        assert!(
            !last.body().contains("已记录"),
            "a failed write must NOT claim the answer was recorded: {}",
            last.body()
        );
        assert!(
            last.body().contains("[warn]"),
            "a failed clarify write must surface a warning: {}",
            last.body()
        );
        let _ = std::fs::remove_file(&blocker);
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
        assert!(last.body().contains("Hello world"));
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
        assert_eq!(host_msgs[0].body(), "Part 1 Part 2");
    }

    #[test]
    fn long_stream_is_never_truncated_only_segmented() {
        // The bug: a long streamed reply was hard-capped at 2000 bytes and the
        // rest silenced with `…` (CJK hit that in a few sentences). The fix keeps
        // EVERY byte — once a segment fills, the reply rolls into a fresh Host
        // bubble. Stream ~20 KB of CJK in many deltas and assert nothing is lost
        // and no `…` truncation marker is appended.
        let mut a = fresh_app(Some("offline"));
        let chunk = "这是一段很长的中文回复内容用来测试不被截断"; // 21 chars
        let n = 500;
        for _ in 0..n {
            a.apply_engine(EngineEvent::WorkerStream {
                event: umadev_runtime::StreamEvent::Text {
                    delta: chunk.into(),
                },
            });
        }
        let host_total: usize = a
            .history
            .iter()
            .filter(|m| m.role == ChatRole::Host)
            .map(|m| m.body().chars().count())
            .sum();
        let expected = chunk.chars().count() * n;
        assert_eq!(
            host_total, expected,
            "every streamed char must survive — no truncation"
        );
        // It segmented into more than one bubble (proof the rollover ran), and no
        // segment carries the old truncation ellipsis.
        let host_msgs: Vec<_> = a
            .history
            .iter()
            .filter(|m| m.role == ChatRole::Host)
            .collect();
        assert!(
            host_msgs.len() > 1,
            "a 20 KB reply must roll over into multiple segments"
        );
        for m in &host_msgs {
            assert!(
                !m.body().contains('…'),
                "no segment should be truncated with an ellipsis: {}",
                m.body()
            );
        }
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
                edit: None,
            },
        });
        // Text after tool should be a NEW message, not appended to tool line
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: "New text".into(),
            },
        });
        assert!(!a.stream_text_active || a.history.back().unwrap().body() == "New text");
    }

    #[test]
    fn same_tool_type_batches() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Read".into(),
                detail: "file1".into(),
                edit: None,
            },
        });
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Read".into(),
                detail: "file2".into(),
                edit: None,
            },
        });
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Read".into(),
                detail: "file3".into(),
                edit: None,
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
            "3 same-type read calls should merge into 1 structured tool row"
        );
        // The merged row is a STRUCTURED tool call, not a flattened sentence.
        let MessageBody::Tool(t) = &host_msgs[0].kind else {
            panic!("merged read batch must be a Tool body, got Text");
        };
        assert!(t.merged, "low-signal reads merge into one batch row");
        assert_eq!(t.count, 3, "the count tracks all three reads");
        assert_eq!(t.status, ToolStatus::Running, "still in flight");
        // The flat text still surfaces the count for export / history.
        assert!(
            host_msgs[0].body().contains('3'),
            "flat text carries the count: {}",
            host_msgs[0].body()
        );
    }

    #[test]
    fn different_tool_type_resets_batch() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Read".into(),
                detail: "file1".into(),
                edit: None,
            },
        });
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Bash".into(),
                detail: "npm test".into(),
                edit: None,
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
        // The Bash row is a single, un-merged tool row (its result IS signal).
        let MessageBody::Tool(bash) = &host_msgs[1].kind else {
            panic!("Bash must render as a structured tool row");
        };
        assert!(!bash.merged, "Bash is a single-row tool, never merged");
        assert_eq!(bash.name, "Bash");
    }

    // ---- P4: structured tool rows ----------------------------------------

    #[test]
    fn tool_use_pushes_structured_tool_row_not_a_sentence() {
        // A ToolUse no longer flattens into a `[write] Edit `path`` string — it
        // becomes a typed `MessageBody::Tool` the renderer draws as one status
        // line. Guards against regressing to the "tool call reads like prose" bug.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Edit".into(),
                detail: "src/main.rs".into(),
                edit: None,
            },
        });
        let last = a.history.back().unwrap();
        let MessageBody::Tool(t) = &last.kind else {
            panic!("a tool use must produce a Tool body, not Text");
        };
        assert_eq!(t.name, "Edit");
        assert_eq!(t.arg, "src/main.rs");
        assert_eq!(t.status, ToolStatus::Running);
        assert!(t.result.is_none(), "no result yet while in flight");
    }

    #[test]
    fn edit_with_content_pushes_a_diff_card_in_real_time() {
        // P1: a Write/Edit carrying structured content renders a diff card the
        // moment the tool_use arrives — we don't wait for the result.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Edit".into(),
                detail: "src/lib.rs".into(),
                edit: Some(umadev_runtime::ToolEdit {
                    path: "src/lib.rs".into(),
                    before: "let x = 1;\nlet y = 2;\n".into(),
                    after: "let x = 1;\nlet y = 3;\n".into(),
                }),
            },
        });
        let MessageBody::Diff(d) = &a.history.back().unwrap().kind else {
            panic!("an edit with content must produce a Diff card, not a Tool row");
        };
        assert_eq!(d.path, "src/lib.rs");
        assert_eq!(d.added, 1, "one line changed → one added");
        assert_eq!(d.removed, 1, "…and one removed");
        // The unchanged `let x = 1;` is kept as ±context around the change.
        let all: Vec<(char, &str)> = d
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| (l.tag, l.text.as_str()))
            .collect();
        assert!(
            all.contains(&(' ', "let x = 1;")),
            "context line kept: {all:?}"
        );
        assert!(all.contains(&('-', "let y = 2;")), "deletion kept: {all:?}");
        assert!(all.contains(&('+', "let y = 3;")), "addition kept: {all:?}");
    }

    #[test]
    fn write_renders_as_all_additions_diff() {
        // A Write is a fresh file: every line is an addition, none removed.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Write".into(),
                detail: "src/new.rs".into(),
                edit: Some(umadev_runtime::ToolEdit {
                    path: "src/new.rs".into(),
                    before: String::new(),
                    after: "fn a() {}\nfn b() {}\n".into(),
                }),
            },
        });
        let MessageBody::Diff(d) = &a.history.back().unwrap().kind else {
            panic!("a Write with content must produce a Diff card");
        };
        assert_eq!(d.added, 2);
        assert_eq!(d.removed, 0);
        assert!(
            d.hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .all(|l| l.tag == '+'),
            "every line of a fresh Write is an addition"
        );
    }

    #[test]
    fn diff_card_keeps_only_three_context_lines() {
        // ±DIFF_CONTEXT: a far-away unchanged line is NOT kept in the hunk.
        use std::fmt::Write as _;
        let mut before = String::new();
        let mut after = String::new();
        for i in 0..20 {
            let _ = writeln!(before, "line{i}");
            if i == 10 {
                after.push_str("line10-CHANGED\n");
            } else {
                let _ = writeln!(after, "line{i}");
            }
        }
        let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
            path: "x.txt".into(),
            before,
            after,
        });
        let texts: Vec<&str> = d
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| l.text.as_str())
            .collect();
        // line7 is within 3 of the change (line10) → kept; line0 is far → dropped.
        assert!(texts.contains(&"line7"), "±3 context kept: {texts:?}");
        assert!(!texts.contains(&"line0"), "far line dropped");
        assert_eq!(DIFF_CONTEXT, 3);
    }

    #[test]
    fn noop_edit_falls_open_to_a_plain_tool_row() {
        // Fail-open: an edit whose before==after (no real change → zero hunks)
        // degrades to a plain tool row, never an empty diff card.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Edit".into(),
                detail: "same.rs".into(),
                edit: Some(umadev_runtime::ToolEdit {
                    path: "same.rs".into(),
                    before: "unchanged\n".into(),
                    after: "unchanged\n".into(),
                }),
            },
        });
        assert!(
            matches!(a.history.back().unwrap().kind, MessageBody::Tool(_)),
            "a no-op edit degrades to a plain tool row"
        );
    }

    #[test]
    fn diff_card_handles_cjk_content_without_panic() {
        // CJK lines must not panic the diff builder (char-boundary safe) and must
        // round-trip their content.
        let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
            path: "说明.md".into(),
            before: "第一行\n第二行\n".into(),
            after: "第一行\n第二行改\n".into(),
        });
        assert_eq!(d.added, 1);
        assert_eq!(d.removed, 1);
        assert!(d
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .any(|l| l.text == "第二行改"));
    }

    #[test]
    fn diff_card_absorbs_a_success_result_silently() {
        // After a diff card, a SUCCESS ToolResult is implied by the card itself —
        // no redundant `[ok]` line is appended.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Write".into(),
                detail: "f.rs".into(),
                edit: Some(umadev_runtime::ToolEdit {
                    path: "f.rs".into(),
                    before: String::new(),
                    after: "fn x() {}\n".into(),
                }),
            },
        });
        let before = a.history.len();
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolResult {
                ok: true,
                summary: "File created successfully".into(),
            },
        });
        assert_eq!(
            a.history.len(),
            before,
            "a success after a diff card adds no extra line"
        );
        assert!(matches!(
            a.history.back().unwrap().kind,
            MessageBody::Diff(_)
        ));
    }

    #[test]
    fn big_diff_defaults_collapsed_and_ctrl_r_toggles() {
        // A diff over the fold threshold defaults collapsed; Ctrl+R expands it
        // (reusing the P6 fold lever).
        use std::fmt::Write as _;
        let before = String::new();
        let mut after = String::new();
        for i in 0..40 {
            let _ = writeln!(after, "row{i}");
        }
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Write".into(),
                detail: "big.rs".into(),
                edit: Some(umadev_runtime::ToolEdit {
                    path: "big.rs".into(),
                    before,
                    after,
                }),
            },
        });
        {
            let MessageBody::Diff(d) = &a.history.back().unwrap().kind else {
                panic!("Diff card");
            };
            assert!(d.collapsed, "a big diff defaults collapsed");
            assert!(d.total_rows() > DIFF_FOLD_THRESHOLD);
        }
        // Ctrl+R toggles the most-recent collapsible row → expanded.
        let _ = a.apply_key_with_mods(
            crossterm::event::KeyCode::Char('r'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        let MessageBody::Diff(d) = &a.history.back().unwrap().kind else {
            panic!("Diff card");
        };
        assert!(!d.collapsed, "Ctrl+R expands the folded diff");
    }

    #[test]
    fn word_diff_marks_only_the_changed_token_on_each_side() {
        // `const oldName = compute(input);` → `const newName = compute(input);`
        // — only `oldName`/`newName` should carry a `changed` byte range; the
        // surrounding tokens stay unchanged (empty around them). The rename is a
        // small fraction of the line, well under the 0.4 rewrite threshold.
        let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
            path: "x.ts".into(),
            before: "const oldName = compute(input);\n".into(),
            after: "const newName = compute(input);\n".into(),
        });
        let lines: Vec<&DiffLine> = d.hunks.iter().flat_map(|h| h.lines.iter()).collect();
        let del = lines.iter().find(|l| l.tag == '-').expect("a - line");
        let ins = lines.iter().find(|l| l.tag == '+').expect("a + line");
        // Each side has exactly one changed region, and it covers the renamed
        // identifier — not the whole line.
        assert_eq!(del.changed.len(), 1, "one changed region on the - line");
        assert_eq!(ins.changed.len(), 1, "one changed region on the + line");
        let (ds, de) = del.changed[0];
        let (is, ie) = ins.changed[0];
        assert_eq!(&del.text[ds..de], "oldName", "the deleted word is marked");
        assert_eq!(&ins.text[is..ie], "newName", "the inserted word is marked");
        // The unchanged prefix `let ` and suffix ` = 1;` are NOT inside a range.
        assert!(ds >= "let ".len(), "the leading `let ` stays unchanged");
    }

    #[test]
    fn word_diff_falls_back_to_whole_line_on_a_near_total_rewrite() {
        // A line replaced wholesale (almost no shared tokens) trips the 0.4
        // rewrite ratio → both `changed` vecs come back empty so the renderer
        // whole-line-highlights instead of confetti.
        let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
            path: "x.rs".into(),
            before: "alpha beta gamma delta\n".into(),
            after: "one two three four five\n".into(),
        });
        let lines: Vec<&DiffLine> = d.hunks.iter().flat_map(|h| h.lines.iter()).collect();
        for l in lines.iter().filter(|l| l.tag != ' ') {
            assert!(
                l.changed.is_empty(),
                "a near-total rewrite drops the word signal: {:?}",
                l.changed
            );
        }
    }

    #[test]
    fn word_diff_is_cjk_byte_safe() {
        // A single CJK token changed inside an otherwise-equal line: the byte
        // ranges must land on char boundaries (slicing must not panic) and cover
        // exactly the changed CJK run.
        let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
            path: "x.md".into(),
            before: "前缀 旧值 后缀\n".into(),
            after: "前缀 新值 后缀\n".into(),
        });
        let lines: Vec<&DiffLine> = d.hunks.iter().flat_map(|h| h.lines.iter()).collect();
        let del = lines.iter().find(|l| l.tag == '-').expect("a - line");
        let ins = lines.iter().find(|l| l.tag == '+').expect("a + line");
        // Every range must be on a char boundary (no panic when sliced).
        for l in [del, ins] {
            for &(s, e) in &l.changed {
                assert!(l.text.is_char_boundary(s) && l.text.is_char_boundary(e));
                let _ = &l.text[s..e]; // would panic if mis-aligned
            }
        }
        assert!(
            del.changed
                .iter()
                .any(|&(s, e)| del.text[s..e].contains('旧')),
            "the changed CJK token is marked on the - side"
        );
        assert!(
            ins.changed
                .iter()
                .any(|&(s, e)| ins.text[s..e].contains('新')),
            "the changed CJK token is marked on the + side"
        );
    }

    #[test]
    fn tool_result_attaches_to_the_running_row_and_auto_collapses_on_ok() {
        // A successful result flips the SAME row to Ok (not a new line) and
        // auto-collapses it; a row height stays stable pending→done.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Write".into(),
                detail: "README.md".into(),
                edit: None,
            },
        });
        let before = a.history.len();
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolResult {
                ok: true,
                summary: "wrote 12 lines".into(),
            },
        });
        assert_eq!(
            a.history.len(),
            before,
            "result updates in place, no new row"
        );
        let MessageBody::Tool(t) = &a.history.back().unwrap().kind else {
            panic!("still a Tool row");
        };
        assert_eq!(t.status, ToolStatus::Ok);
        assert_eq!(t.result.as_deref(), Some("wrote 12 lines"));
        assert!(t.collapsed, "a finished OK call auto-collapses");
    }

    #[test]
    fn failed_tool_result_stays_expanded() {
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Bash".into(),
                detail: "cargo build".into(),
                edit: None,
            },
        });
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolResult {
                ok: false,
                summary: "error[E0308]".into(),
            },
        });
        let MessageBody::Tool(t) = &a.history.back().unwrap().kind else {
            panic!("Tool row");
        };
        assert_eq!(t.status, ToolStatus::Fail);
        assert!(!t.collapsed, "a failed call must never hide its error");
    }

    /// Helper: a tool row whose `status` is whatever the caller passes, so the
    /// settle tests can stand up a mix of in-flight + already-finished rows.
    fn push_tool_row(a: &mut App, name: &str, status: ToolStatus) {
        a.history.push_back(ChatMessage {
            role: ChatRole::Host,
            kind: MessageBody::Tool(ToolCall {
                name: name.to_string(),
                arg: String::new(),
                status,
                result: None,
                merged: false,
                count: 1,
                collapsed: false,
            }),
            collapsed: false,
        });
    }

    fn tool_statuses(a: &App) -> Vec<ToolStatus> {
        a.history
            .iter()
            .filter_map(|m| match &m.kind {
                MessageBody::Tool(t) => Some(t.status),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn abort_settles_in_flight_tool_rows_but_keeps_finished_ones() {
        // The user-reported bug: after the run aborts (idle settle / base error,
        // both arrive as an ABORT_SENTINEL note), a stack of base tool rows
        // (TaskCreate / Agent / Bash / Read / TaskUpdate) kept spinning forever
        // because the matching ToolResult never landed. They must now settle.
        let mut a = fresh_app(Some("offline"));
        a.run_started = true;
        push_tool_row(&mut a, "TaskCreate", ToolStatus::Running);
        push_tool_row(&mut a, "Read", ToolStatus::Ok); // already finished — keep it
        push_tool_row(&mut a, "Agent", ToolStatus::Running);
        push_tool_row(&mut a, "TaskUpdate", ToolStatus::Queued);

        a.apply_engine(EngineEvent::Note(format!(
            "{}本轮已中止:磁盘写入失败",
            crate::ABORT_SENTINEL
        )));

        assert!(a.aborted, "the sentinel flips the run into aborted");
        let statuses = tool_statuses(&a);
        // Every in-flight row is settled; NONE is left in-progress.
        assert!(
            statuses.iter().all(|s| s.is_terminal()),
            "no tool row may stay in-progress after an abort: {statuses:?}"
        );
        // The genuinely-finished Ok row is NOT downgraded to a fake abort.
        assert_eq!(
            statuses,
            vec![
                ToolStatus::Aborted,
                ToolStatus::Ok,
                ToolStatus::Aborted,
                ToolStatus::Aborted,
            ],
            "in-flight rows -> Aborted, the Ok row keeps its real success: {statuses:?}"
        );
    }

    #[test]
    fn cancel_settles_in_flight_tool_rows() {
        // The user Cancel path (Esc/Ctrl-C -> cancel_run -> reset_for_new_run ->
        // clear_live_panels) must also stop any spinning tool row.
        let mut a = fresh_app(Some("offline"));
        a.run_started = true;
        push_tool_row(&mut a, "Bash", ToolStatus::Running);
        push_tool_row(&mut a, "Edit", ToolStatus::Fail); // finished — keep it

        a.cancel_run();

        let statuses = tool_statuses(&a);
        assert!(
            statuses.iter().all(|s| s.is_terminal()),
            "cancel must settle every in-flight tool row: {statuses:?}"
        );
        assert_eq!(
            statuses,
            vec![ToolStatus::Aborted, ToolStatus::Fail],
            "the Running row -> Aborted, the Fail row keeps its failure: {statuses:?}"
        );
    }

    #[test]
    fn clean_finish_closes_dangling_in_flight_tool_row() {
        // Defensive: even a CLEAN delivery finish (finalize_live_panels, reached
        // here via the chat/Fast build completion card) must close any tool row
        // left dangling in-progress, so a settled run never keeps a spinner.
        let mut a = fresh_app(Some("offline"));
        push_tool_row(&mut a, "Write", ToolStatus::Running);
        push_tool_row(&mut a, "Read", ToolStatus::Ok);

        a.finalize_live_panels();

        let statuses = tool_statuses(&a);
        assert_eq!(
            statuses,
            vec![ToolStatus::Aborted, ToolStatus::Ok],
            "a clean finish closes the dangling Running row, keeps the Ok row: {statuses:?}"
        );
    }

    #[test]
    fn read_only_grep_folds_a_metric_not_the_raw_dump() {
        // A merged read/grep batch keeps its `inspected N` headline and folds
        // the grep result into a `(N matches)` metric — never the raw output.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Grep".into(),
                detail: "TODO".into(),
                edit: None,
            },
        });
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolResult {
                ok: true,
                summary: "3 files\nsrc/a.rs\nsrc/b.rs\nsrc/c.rs".into(),
            },
        });
        let MessageBody::Tool(t) = &a.history.back().unwrap().kind else {
            panic!("Tool row");
        };
        assert!(t.merged, "a grep is a low-signal mergeable tool");
        // The metric folds in; the raw file list is NOT dumped into the result.
        let result = t.result.as_deref().unwrap_or("");
        assert!(result.contains('3'), "folds the count metric: {result}");
        assert!(
            !result.contains("src/a.rs"),
            "must not dump the raw output: {result}"
        );
    }

    #[test]
    fn contiguous_low_signal_reads_merge_with_increasing_count() {
        // Five reads in a row collapse to one row with count 5 — and the count
        // is greatest-seen, so it can never visibly jump backwards.
        let mut a = fresh_app(Some("offline"));
        for i in 0..5 {
            a.apply_engine(EngineEvent::WorkerStream {
                event: umadev_runtime::StreamEvent::ToolUse {
                    name: "Read".into(),
                    detail: format!("file{i}"),
                    edit: None,
                },
            });
        }
        let host_rows: Vec<_> = a
            .history
            .iter()
            .filter(|m| m.role == ChatRole::Host)
            .collect();
        assert_eq!(host_rows.len(), 1, "five reads merge into one row");
        let MessageBody::Tool(t) = &host_rows[0].kind else {
            panic!("Tool row");
        };
        assert_eq!(t.count, 5);
        assert!(t.merged);
    }

    #[test]
    fn a_write_breaks_the_read_batch_so_the_next_read_starts_fresh() {
        let mut a = fresh_app(Some("offline"));
        for ev in [
            ("Read", "a"),
            ("Read", "b"),
            ("Write", "out.txt"),
            ("Read", "c"),
        ] {
            a.apply_engine(EngineEvent::WorkerStream {
                event: umadev_runtime::StreamEvent::ToolUse {
                    name: ev.0.into(),
                    detail: ev.1.into(),
                    edit: None,
                },
            });
        }
        let host_rows: Vec<_> = a
            .history
            .iter()
            .filter(|m| m.role == ChatRole::Host)
            .collect();
        // batch(a,b) · write · batch(c) → 3 rows.
        assert_eq!(host_rows.len(), 3, "a write splits the read batch");
    }

    // ---- P6: long-output folding -----------------------------------------

    #[test]
    fn a_long_host_reply_is_collapsible_and_ctrl_r_toggles_it() {
        let mut a = fresh_app(Some("offline"));
        // A 50-line Host reply — well past the fold threshold.
        let wall: String = (0..50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        a.push(ChatRole::Host, wall);
        let idx = a.history.len() - 1;
        assert!(
            message_is_collapsible(&a.history[idx]),
            "a 50-line wall is foldable"
        );
        assert!(!a.history[idx].collapsed, "starts expanded");
        // Ctrl+R folds the most recent collapsible row.
        let _ = a.apply_key_with_mods(KeyCode::Char('r'), crossterm::event::KeyModifiers::CONTROL);
        assert!(a.history[idx].collapsed, "Ctrl+R collapsed the wall");
        // Ctrl+R again expands it.
        let _ = a.apply_key_with_mods(KeyCode::Char('r'), crossterm::event::KeyModifiers::CONTROL);
        assert!(!a.history[idx].collapsed, "Ctrl+R re-expanded the wall");
    }

    #[test]
    fn a_short_reply_is_not_collapsible() {
        let mut a = fresh_app(Some("offline"));
        a.push(ChatRole::Host, "just one short line");
        let last = a.history.back().unwrap();
        assert!(
            !message_is_collapsible(last),
            "a short reply is never folded"
        );
    }

    #[test]
    fn ctrl_r_is_a_noop_when_nothing_is_foldable() {
        let mut a = fresh_app(Some("offline"));
        a.push(ChatRole::Host, "short");
        let before = a.clone();
        let _ = a.apply_key_with_mods(KeyCode::Char('r'), crossterm::event::KeyModifiers::CONTROL);
        // No foldable row → history unchanged (fail-open).
        assert_eq!(a.history.len(), before.history.len());
        assert!(!a.history.back().unwrap().collapsed);
    }

    #[test]
    fn ctrl_o_toggles_the_global_verbose_flag() {
        // UX maturity Fix B: a single global key flips `verbose`, which every
        // collapsible renderer reads so ALL collapsed output reveals/hides at
        // once — not just the most-recent row that Ctrl+R reaches.
        let mut a = fresh_app(Some("offline"));
        assert!(
            !a.verbose,
            "verbose defaults off (everything at its per-row state)"
        );
        let _ = a.apply_key_with_mods(KeyCode::Char('o'), crossterm::event::KeyModifiers::CONTROL);
        assert!(a.verbose, "Ctrl+O turns the global expand-all on");
        let _ = a.apply_key_with_mods(KeyCode::Char('o'), crossterm::event::KeyModifiers::CONTROL);
        assert!(!a.verbose, "Ctrl+O toggles it back off");
    }

    // ---- backward-compat: plain Text rows ---------------------------------

    #[test]
    fn plain_push_stays_a_text_body_and_body_reads_through() {
        // Every existing `push(role, String)` call still produces a Text body
        // and `body()` reads it back verbatim — the upgrade is invisible to the
        // dozens of plain-message call sites.
        let mut a = fresh_app(Some("offline"));
        a.push(ChatRole::System, "hello world");
        let last = a.history.back().unwrap();
        assert!(matches!(last.kind, MessageBody::Text(_)));
        assert_eq!(last.body(), "hello world");
        assert!(!last.collapsed);
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
            last.body().contains("thinking"),
            "should show thinking indicator: {}",
            last.body()
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
        assert!(
            last.body().contains("[ok]"),
            "success should show checkmark"
        );
        assert!(last.body().contains("4.6.0"));
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
        assert!(last.body().contains("[fail]"), "error should show cross");
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
    fn transient_warning_goes_to_status_not_transcript() {
        // A RECOVERABLE hiccup (rate-limit / retry / overloaded) surfaces as ONE
        // muted live status line, NOT a permanent transcript row — so a flurry of
        // retries doesn't read like the turn is erroring next to the thinking timer
        // ("时间会乱弹错误"). The turn keeps running; only a terminal ABORT settles it.
        let mut a = fresh_app(Some("offline"));
        let before = a.history.len();
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Warning {
                message: "rate limited".into(),
            },
        });
        assert_eq!(
            a.history.len(),
            before,
            "a transient warning must not be pushed to the transcript"
        );
        assert!(
            a.transient_status
                .as_deref()
                .unwrap_or("")
                .contains("rate limited"),
            "it surfaces as a transient live status line instead"
        );
    }

    #[test]
    fn notable_warning_still_shows_in_transcript() {
        // A non-transient warning stays a transcript row as before.
        let mut a = fresh_app(Some("offline"));
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Warning {
                message: "disk almost full".into(),
            },
        });
        let last = a.history.back().unwrap();
        assert!(last.body().contains("disk almost full"));
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
    fn shift_tab_cycles_plan_guarded_auto() {
        // BackTab/Shift+Tab now cycles the FULL tier Plan → Guarded → Auto → Plan
        // (was a 2-state Auto<->Guarded flip that could never reach Plan).
        let mut a = fresh_app(Some("offline"));
        a.set_trust_mode(umadev_agent::TrustMode::Plan);
        a.cycle_approval_mode();
        assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);
        a.cycle_approval_mode();
        assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Auto);
        a.cycle_approval_mode();
        assert_eq!(
            a.effective_trust_mode(),
            umadev_agent::TrustMode::Plan,
            "cycle must wrap Auto → Plan so Plan is keyboard-reachable"
        );
    }

    #[test]
    fn config_trust_mode_is_cached_not_re_read_per_call() {
        // P2-B: `effective_trust_mode` runs in the render hot path (~12/s). It
        // must NOT `load_project_config` (a disk read) on every call. Proof: the
        // first call memoises `Guarded`; rewriting `.umadevrc` to auto ON DISK is
        // then IGNORED (cache still serves `Guarded`) — i.e. no per-call read.
        // Only after an explicit invalidation does it pick up the new value.
        let a = fresh_app(Some("offline"));
        assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);

        // Flip the on-disk config behind the running app's back.
        std::fs::write(
            a.project_root.join(".umadevrc"),
            "[pipeline]\nauto_approve_gates = true\n",
        )
        .unwrap();

        // No session override is set, so without a cache this would re-read disk
        // and flip to Auto. The cache means it stays Guarded — that is the proof
        // the hot path no longer touches the filesystem.
        assert_eq!(
            a.effective_trust_mode(),
            umadev_agent::TrustMode::Guarded,
            "config-derived tier must come from the process cache, not a fresh disk read"
        );

        // After an explicit invalidation, the next call re-reads and sees Auto.
        a.invalidate_trust_cache();
        assert_eq!(
            a.effective_trust_mode(),
            umadev_agent::TrustMode::Auto,
            "invalidation must let the next call pick up the new on-disk config"
        );
    }

    #[test]
    fn gate_card_health_labels_are_localized() {
        // P2-D: the artifact health labels ([warn] MISSING / SCAFFOLD / SHORT /
        // [ok], and the dark-mode marker) were hard-coded English, so a zh-CN
        // user saw English jammed into an otherwise localized card. They now come
        // from the catalog.
        let app = fresh_app(Some("offline"));
        // No output/ artifacts exist for this fresh workspace → every doc is
        // MISSING, exercising the `lines == 0` label.
        let card = gate_card(
            Gate::DocsConfirm,
            &app.slug,
            &app.project_root,
            umadev_i18n::Lang::ZhCn,
        );
        // Localized "missing" label is present; the old raw English is gone.
        assert!(
            card.contains("缺失"),
            "zh-CN gate card should use the localized MISSING label: {card}"
        );
        assert!(
            !card.contains("MISSING") && !card.contains("SCAFFOLD") && !card.contains("SHORT"),
            "no hard-coded English health labels should leak into a zh-CN card: {card}"
        );

        // English locale still shows the English labels (round-trips the key).
        let card_en = gate_card(
            Gate::DocsConfirm,
            &app.slug,
            &app.project_root,
            umadev_i18n::Lang::En,
        );
        assert!(
            card_en.contains("MISSING"),
            "en gate card should render the English MISSING label: {card_en}"
        );
    }

    #[test]
    fn session_override_wins_over_cache() {
        // A `/mode` override always beats the cached config tier and the override
        // path never consults the cache at all (it returns before the disk path).
        let mut a = fresh_app(Some("offline"));
        assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded); // primes cache
        a.set_trust_mode(umadev_agent::TrustMode::Plan);
        assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Plan);
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
            .any(|m| m.body().contains("nonsense") || m.body().contains("未知")));
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
            a.history.iter().any(|m| m.body().contains("[trust]")),
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
    fn queued_turn_is_echoed_recorded_and_uses_chat_text() {
        let mut a = fresh_app(Some("offline"));
        // First turn starts a brain-driven turn (thinking).
        let _ = a.submit_text("first".to_string());
        assert!(a.thinking);
        let convo_before = a.conversation.len();
        let hist_before = a.history.len();
        // Second turn WHILE thinking: queued — but it must STILL be echoed to the
        // transcript (the user sees their message), recorded in conversation
        // memory (so the parked turn isn't lost from the base's context), and the
        // queue note must be the chat text, NOT the pipeline `run.queued` (no gate
        // exists here). This is the "second message looks like it did nothing" fix.
        let _ = a.submit_text("second".to_string());
        // Echoed: the user's "second" message is in the transcript.
        assert!(
            a.history.iter().any(|m| m.body() == "second"),
            "the queued user message is still echoed to the transcript"
        );
        // NOT YET recorded in conversation memory: a queued turn is recorded only
        // when it actually FIRES (in `take_next_queued_chat`), not when parked — so
        // an interrupt that clears the queue can't leave a dangling "user said X"
        // with no assistant reply in the base's context.
        assert_eq!(
            a.conversation.len(),
            convo_before,
            "a parked turn is recorded at drain time, not when queued"
        );
        // A chat.queued note was pushed (history grew by the You echo + the note).
        assert!(a.history.len() >= hist_before + 2);
        let note = umadev_i18n::t(a.lang, "chat.queued");
        assert!(
            a.history.iter().any(|m| m.body() == note),
            "the queue note uses chat.queued, not the gate-flavoured run.queued"
        );
        assert_eq!(a.queued_chat.len(), 1);
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
    fn ctrl_c_on_empty_idle_input_never_quits() {
        // Ctrl+C is universal muscle-memory for COPY, so on an idle EMPTY box it must
        // NOT quit and must NOT even arm a quit-confirm — it only hints to use /quit.
        // (Quitting stays deliberate: /quit, /q, /exit, Ctrl+D, or a double-Esc.)
        let mut a = fresh_app(Some("offline"));
        let action =
            a.apply_key_with_mods(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL);
        assert_eq!(action, Action::None);
        assert!(
            !a.pending_quit_confirm,
            "idle empty Ctrl-C does NOT arm a quit confirm"
        );
        assert!(!a.should_quit, "idle empty Ctrl-C does NOT quit the app");
        assert_eq!(
            a.history.back().expect("a hint was pushed").body(),
            umadev_i18n::t(a.lang, "quit.use_command"),
            "idle empty Ctrl-C hints to use /quit"
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

    #[test]
    fn history_recall_preserves_the_in_progress_draft() {
        let mut a = fresh_app(Some("offline"));
        a.remember_submission("first prompt");
        a.remember_submission("second prompt");
        // The user is mid-way through typing a fresh line.
        a.input = "draft I was typing".to_string();
        a.input_cursor = a.input_len();
        // Recall back through history…
        a.input_history_back();
        assert_eq!(a.input, "second prompt");
        a.input_history_back();
        assert_eq!(a.input, "first prompt");
        // …then step forward past the newest entry → the DRAFT is restored, not
        // cleared.
        a.input_history_forward();
        assert_eq!(a.input, "second prompt");
        a.input_history_forward();
        assert_eq!(
            a.input, "draft I was typing",
            "stepping forward past the newest entry restores the stashed draft"
        );
        assert_eq!(
            a.input_cursor,
            a.input_len(),
            "cursor lands at the draft end"
        );
        assert!(a.input_history_idx.is_none(), "recall is over");
    }

    #[test]
    fn picker_enter_with_stale_index_does_not_panic() {
        let mut a = fresh_app(Some("offline"));
        a.mode = AppMode::Picker;
        // Force a selection index past the end of whatever the picker holds.
        a.picker_selected = a.picker_items.len() + 5;
        // Must fail-open to a no-op Action, never index-panic.
        let act = a.apply_key_with_mods(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        assert!(matches!(act, Action::None));
    }

    #[test]
    fn forward_delete_and_kill_to_eol_reset_palette_selected() {
        let mut a = fresh_app(Some("offline"));
        a.input = "abcdef".to_string();
        a.input_cursor = 2;
        a.palette_selected = 3;
        a.forward_delete();
        assert_eq!(a.palette_selected, 0, "forward_delete resets the palette");

        a.palette_selected = 4;
        a.delete_to_line_end();
        assert_eq!(
            a.palette_selected, 0,
            "delete_to_line_end resets the palette"
        );
    }

    // ---- /status reconciles with the persisted workflow state ----

    #[test]
    fn reconcile_phase_statuses_advances_to_persisted_phase() {
        // The plan / director-loop build emits no PhaseStarted/PhaseCompleted,
        // so the in-memory vector is all-Pending. Reconciled against a
        // workflow-state that reached `backend`, every phase up to and including
        // backend must read Done and quality/delivery must stay Pending.
        let rows: Vec<PhaseRow> = PHASE_CHAIN
            .iter()
            .map(|&phase| PhaseRow {
                phase,
                status: PhaseStatus::Pending,
            })
            .collect();
        let statuses = App::reconcile_phase_statuses(&rows, Some(Phase::Backend));
        let backend_i = PHASE_CHAIN
            .iter()
            .position(|&p| p == Phase::Backend)
            .unwrap();
        for (i, (row, status)) in rows.iter().zip(&statuses).enumerate() {
            if i <= backend_i {
                assert_eq!(
                    *status,
                    PhaseStatus::Done,
                    "{} should be done",
                    row.phase.id()
                );
            } else {
                assert_eq!(
                    *status,
                    PhaseStatus::Pending,
                    "{} should be pending",
                    row.phase.id()
                );
            }
        }
    }

    #[test]
    fn reconcile_phase_statuses_fail_open_and_never_regresses() {
        // Legacy walk: research/docs/docs_confirm done, spec actively Running.
        let rows: Vec<PhaseRow> = PHASE_CHAIN
            .iter()
            .map(|&phase| {
                let status = match phase {
                    Phase::Research | Phase::Docs | Phase::DocsConfirm => PhaseStatus::Done,
                    Phase::Spec => PhaseStatus::Running,
                    _ => PhaseStatus::Pending,
                };
                PhaseRow { phase, status }
            })
            .collect();
        let spec_i = PHASE_CHAIN.iter().position(|&p| p == Phase::Spec).unwrap();
        let backend_i = PHASE_CHAIN
            .iter()
            .position(|&p| p == Phase::Backend)
            .unwrap();
        let quality_i = PHASE_CHAIN
            .iter()
            .position(|&p| p == Phase::Quality)
            .unwrap();

        // No persisted phase → in-memory statuses returned verbatim (fail-open).
        let verbatim = App::reconcile_phase_statuses(&rows, None);
        assert_eq!(
            verbatim,
            rows.iter().map(|r| r.status).collect::<Vec<_>>(),
            "missing/unparseable state must fall back to in-memory only"
        );

        // File at the SAME furthest phase → keep spec's Running (active) marker.
        let same = App::reconcile_phase_statuses(&rows, Some(Phase::Spec));
        assert_eq!(same[spec_i], PhaseStatus::Running);

        // File AHEAD (backend) → spec subsumed into Done, backend Done, quality
        // still Pending.
        let ahead = App::reconcile_phase_statuses(&rows, Some(Phase::Backend));
        assert_eq!(ahead[spec_i], PhaseStatus::Done);
        assert_eq!(ahead[backend_i], PhaseStatus::Done);
        assert_eq!(ahead[quality_i], PhaseStatus::Pending);

        // File BEHIND (docs) → never regress; spec stays Running.
        let behind = App::reconcile_phase_statuses(&rows, Some(Phase::Docs));
        assert_eq!(behind[spec_i], PhaseStatus::Running, "never goes backward");
    }

    #[test]
    fn status_overlay_reflects_persisted_phase_after_plan_run() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&state_dir).unwrap();
        // A director-loop / plan build reached `backend` (phase persisted by the
        // run) but emitted no PhaseStarted/PhaseCompleted, so self.phases is
        // all-Pending and the raw table would lie.
        let state_json = r#"{
            "phase": "backend",
            "active_gate": "",
            "slug": "shop",
            "requirement": "做个电商后台",
            "last_transition_at": "2026-06-27T10:00:00Z",
            "note": "",
            "spec_version": "UMADEV_HOST_SPEC_V1"
        }"#;
        std::fs::write(state_dir.join("workflow-state.json"), state_json).unwrap();

        let cfg = UserConfig {
            backend: Some("offline".into()),
            model: None,
            lang: Some("zh-CN".into()),
            ..Default::default()
        };
        let mut app = App::new(
            "shop",
            cfg,
            std::path::PathBuf::from("/tmp/sd-status-overlay-cfg.toml"),
            tmp.path().to_path_buf(),
        );
        // Precondition: the in-memory phase vector is frozen all-Pending.
        assert!(
            app.phases.iter().all(|r| r.status == PhaseStatus::Pending),
            "the plan path leaves self.phases all-Pending"
        );

        app.open_status_overlay();
        let lines = app.overlay.as_ref().expect("overlay opened").lines.clone();
        // The pipeline-phases table precedes the knowledge table, so `find`
        // returns the pipeline row (the only one carrying a status icon).
        let row = |phase: &str| {
            lines
                .iter()
                .find(|l| l.contains(&format!("| {phase} |")))
                .cloned()
                .unwrap_or_default()
        };
        for done in [
            "research",
            "docs",
            "docs_confirm",
            "spec",
            "frontend",
            "preview_confirm",
            "backend",
        ] {
            assert!(
                row(done).contains("[ok]"),
                "{done} row should be done, got: {:?}",
                row(done)
            );
        }
        for pending in ["quality", "delivery"] {
            assert!(
                row(pending).contains("[pending]"),
                "{pending} row should be pending, got: {:?}",
                row(pending)
            );
        }
    }
}
