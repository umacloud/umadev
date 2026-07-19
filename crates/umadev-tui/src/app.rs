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
//! Slash commands inside Chat (`/claude` `/codex` `/opencode` `/grok` `/kimi` `/offline`
//! `/init` `/continue` `/revise` `/diff` `/spec` `/verify`
//! `/doctor` `/help` `/quit` `/clear` `/history` `/commands`) plus normal
//! text.
//!
//! Plain text is normally routed to the selected base. The only local exception
//! is a small, exact set of live progress/change questions, which the shell can
//! answer from its own task, plan, and diff state without steering the running
//! agent. When a gate is open other text is a
//! gate reply (approve / revise); otherwise it is routed to the selected
//! **base** — one of five first-class choices —
//! which decides
//! for itself whether the message is conversation or a build request and
//! replies accordingly — UmaDev is only the shell around that base. The
//! running dialogue is kept in [`App::conversation`] and handed to the base
//! on every turn, so chat has memory instead of being amnesiac one-shots.

use std::collections::VecDeque;
use std::path::PathBuf;

use crossterm::event::KeyCode;
use umadev_agent::{EngineEvent, Gate, GateChoice, GateDecision};
use umadev_runtime::{
    BaseResumeIdentity, FileInputMode, PromptQueueMutation, PromptQueuePlacement,
    SessionCommandInfo, SessionMode, SessionModelInfo, SessionPlanEntry, SessionStateUpdate,
    TurnInput, TurnInputBlock,
};
use umadev_spec::{Phase, PHASE_CHAIN};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

use crate::config::UserConfig;
use crate::prompt_queue_ui::PromptQueueUi;

mod backend;
mod frozen_plan;
pub(crate) mod host_input;
mod lessons_view;
mod memory_view;
mod read_only_metric;
mod submission;
mod task_control;
mod usage_meter;

pub(crate) use backend::{parse_probe_detail, PROBE_AUTH_SENTINEL};
use backend::{refresh_picker_with_probes, step_items};
use lessons_view::{format_lessons_report, format_pitfalls_report};
use memory_view::{compact_audit_id, MemoryParseError, MemoryTuiCommand, MemoryViewScope};
use read_only_metric::read_only_metric;
use submission::{append_text_block, submitted_content_end};
use usage_meter::SessionUsageMeter;

/// Max lines kept in the chat history (older lines roll off).
const HISTORY_CAP: usize = 1000;
const INPUT_HISTORY_CAP: usize = 100;
const MAX_INPUT_HISTORY_BYTES: u64 = 1024 * 1024;
const MAX_CHAT_FILE_BYTES: u64 = 16 * 1024 * 1024;
const CLAUDE_SUBAGENT_STEM: &str = "↳ 子代理";
const CLAUDE_SUBAGENT_WORKING: &str = "工作中…";
/// Max background-run tasks kept in the registry. Single-writer means at most one
/// is live; the rest are recent finished/stopped history rows shown by `/tasks`.
/// The oldest settled task is dropped once the registry exceeds this.
const TASKS_CAP: usize = 12;
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
/// **Hard render cap** for a single tool result / text body when it is shown
/// EXPANDED (not the per-message Ctrl+R fold). Even content that is force-expanded
/// — a failed tool's error, a non-collapsed reply, a freshly streamed wall — is
/// capped to this many SOURCE lines + a `+N 行 (Ctrl+O 展开)` footer so one giant
/// output can never dominate the transcript. Released by the global `verbose`
/// (Ctrl+O) toggle, which renders the whole thing (still under the global
/// `MAX_RENDER_ROWS` post-fold cap). Generous on purpose: only a pathological
/// (hundreds/thousands of lines) output ever trips it, so normal replies are
/// untouched.
pub(crate) const FOLD_HARD_CAP: usize = 120;
/// Ingest cap (chars) for a long-running command row's result when process-log
/// visibility (`/logs`) is on — generous enough to carry a real build log's tail
/// (the host already bounds the output), while the renderer's [`FOLD_HARD_CAP`]
/// line fold still keeps a multi-thousand-line log from dominating the transcript.
/// OFF, the tight 200-char clip is kept (so a normal tool result never balloons).
pub(crate) const PROCESS_LOG_PREVIEW_CHARS: usize = 8 * 1024;
/// FIFO **fail-open floor** for the in-memory working transcript: when a
/// token-budgeted compaction can't run (the summary `complete()` failed / the
/// base is offline / the circuit breaker is tripped), the working view falls back
/// to dropping the oldest down to this many messages — the original
/// pre-compaction behaviour. The FULL transcript on disk is never trimmed, so a
/// FIFO drop here only bounds the live prompt, it never loses durable history.
const CONVERSATION_CAP: usize = 16;
/// Absolute anti-unbounded safety net on the in-memory working transcript,
/// applied **synchronously** after every recorded turn. Real compaction triggers
/// at [`COMPACTION_TOKEN_BUDGET`] (well under this), so this is only ever hit when
/// compaction is impossible (offline / breaker tripped) — it guarantees the live
/// buffer can't grow without bound while the full transcript on disk keeps
/// everything. Generous so it never fights the token-budget compactor.
const CONVERSATION_HARD_CAP: usize = 64;
/// Approximate-token threshold that TRIGGERS auto-compaction: when the working
/// transcript's estimated cost (chars/4) crosses this, the older turns are folded
/// into one structured summary and the recent tail kept verbatim. Deterministic
/// trigger; only the summary text comes from the base.
const COMPACTION_TOKEN_BUDGET: usize = 3_000;
/// Token budget for the verbatim RECENT tail kept un-folded during compaction —
/// the most-recent suffix within this cost stays word-for-word, everything older
/// is summarised. Smaller than [`COMPACTION_TOKEN_BUDGET`] so a fold actually
/// reclaims context.
const COMPACTION_TAIL_BUDGET: usize = 1_200;
/// Minimum number of most-recent messages always kept verbatim through a
/// compaction, regardless of the tail token budget (so the immediate context is
/// never folded away).
const COMPACTION_MIN_TAIL: usize = 4;
/// Max chars in the input box.
const INPUT_CAP: usize = 8192;

/// Max gap between two consecutive key events for the later one to count as part of a PASTE
/// BURST (a paste arrives back-to-back far faster than typing). Used ONLY on Windows to tell a
/// pasted newline — which the Windows console delivers as a bare Enter, not a crossterm
/// `Event::Paste` — from a genuine submit Enter (which follows a human-speed pause). Generous
/// enough to cover the per-key redraw between paste keys, well below any human key interval.
pub(crate) const PASTE_BURST_GAP: std::time::Duration = std::time::Duration::from_millis(30);

/// A bracketed paste with MORE than this many lines collapses to a single
/// `[粘贴 N 行]` chip (the full text is stashed and re-expanded on submit)
/// instead of flooding the input box into unscrollable noise. Mirrors the
/// image-attachment chip mechanism for bulky text.
const PASTE_CHIP_MIN_LINES: usize = 12;
/// A bracketed paste with MORE than this many chars also collapses to a chip —
/// catches a huge single-line paste (one 5 KB line is just as much noise as 40
/// short ones). Either trigger fires the chip.
const PASTE_CHIP_MIN_CHARS: usize = 1200;

/// TUI-side fast rejection mirrors the host protocol's hard attachment envelope.
/// The host re-validates immediately before every write, so these values are UX
/// feedback, never the security boundary.
const MAX_TURN_ATTACHMENTS: usize = 16;
const MAX_ATTACHMENT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Default)]
struct MemoryOptions {
    positional: Vec<String>,
    scope: Option<String>,
    store: Option<String>,
    days: Option<String>,
    output: Option<String>,
    yes: bool,
    clear: bool,
    run: bool,
}

fn tokenize_memory_args(input: &str) -> Result<Vec<String>, MemoryParseError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for ch in input.chars() {
        if let Some(delimiter) = quote {
            if ch == delimiter {
                quote = None;
            } else {
                current.push(ch);
            }
            started = true;
        } else if matches!(ch, '\'' | '"') {
            quote = Some(ch);
            started = true;
        } else if ch.is_whitespace() {
            if started {
                tokens.push(std::mem::take(&mut current));
                started = false;
            }
        } else {
            current.push(ch);
            started = true;
        }
    }
    if quote.is_some() {
        return Err(MemoryParseError::UnclosedQuote);
    }
    if started {
        tokens.push(current);
    }
    Ok(tokens)
}

fn set_memory_option(
    slot: &mut Option<String>,
    value: String,
    name: &str,
) -> Result<(), MemoryParseError> {
    if slot.replace(value).is_some() {
        return Err(MemoryParseError::InvalidArgument(format!(
            "duplicate --{name}"
        )));
    }
    Ok(())
}

fn memory_option_value(
    tokens: &[String],
    index: &mut usize,
    inline: Option<&str>,
    name: &str,
) -> Result<String, MemoryParseError> {
    if let Some(value) = inline {
        if value.is_empty() {
            return Err(MemoryParseError::InvalidArgument(format!(
                "--{name} requires a value"
            )));
        }
        return Ok(value.to_string());
    }
    *index += 1;
    let Some(value) = tokens.get(*index) else {
        return Err(MemoryParseError::InvalidArgument(format!(
            "--{name} requires a value"
        )));
    };
    if value.starts_with("--") {
        return Err(MemoryParseError::InvalidArgument(format!(
            "--{name} requires a value"
        )));
    }
    Ok(value.clone())
}

fn parse_memory_options(tokens: &[String]) -> Result<MemoryOptions, MemoryParseError> {
    let mut parsed = MemoryOptions::default();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        let Some(option) = token.strip_prefix("--") else {
            parsed.positional.push(token.clone());
            index += 1;
            continue;
        };
        let (name, inline) = option
            .split_once('=')
            .map_or((option, None), |(name, value)| (name, Some(value)));
        match name {
            "scope" | "store" | "days" | "output" => {
                let value = memory_option_value(tokens, &mut index, inline, name)?;
                match name {
                    "scope" => set_memory_option(&mut parsed.scope, value, name)?,
                    "store" => set_memory_option(&mut parsed.store, value, name)?,
                    "days" => set_memory_option(&mut parsed.days, value, name)?,
                    "output" => set_memory_option(&mut parsed.output, value, name)?,
                    _ => unreachable!(),
                }
            }
            "yes" | "clear" | "run" => {
                if inline.is_some() {
                    return Err(MemoryParseError::InvalidArgument(token.clone()));
                }
                let slot = match name {
                    "yes" => &mut parsed.yes,
                    "clear" => &mut parsed.clear,
                    "run" => &mut parsed.run,
                    _ => unreachable!(),
                };
                if std::mem::replace(slot, true) {
                    return Err(MemoryParseError::InvalidArgument(format!(
                        "duplicate --{name}"
                    )));
                }
            }
            _ => return Err(MemoryParseError::InvalidArgument(token.clone())),
        }
        index += 1;
    }
    Ok(parsed)
}

fn parse_memory_view_scope(value: Option<&str>) -> Result<MemoryViewScope, MemoryParseError> {
    match value
        .unwrap_or("project")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "project" | "项目" | "專案" => Ok(MemoryViewScope::Project),
        "global" | "全局" | "全域" => Ok(MemoryViewScope::Global),
        "all" | "全部" => Ok(MemoryViewScope::All),
        other => Err(MemoryParseError::InvalidArgument(other.to_string())),
    }
}

fn parse_memory_mutation_scope(
    value: Option<&str>,
) -> Result<umadev_agent::memory_control::MemoryScope, MemoryParseError> {
    let Some(value) = value else {
        return Err(MemoryParseError::MissingScope);
    };
    if value.eq_ignore_ascii_case("all") || value == "全部" {
        return Err(MemoryParseError::OneScopeRequired);
    }
    umadev_agent::memory_control::MemoryScope::parse(value)
        .ok_or_else(|| MemoryParseError::InvalidArgument(value.to_string()))
}

fn parse_memory_selector(
    value: &str,
) -> Result<umadev_agent::memory_control::MemorySelector, MemoryParseError> {
    umadev_agent::memory_control::MemorySelector::parse(value)
        .ok_or_else(|| MemoryParseError::UnknownSelector(value.to_string()))
}

fn parse_exact_memory_store(
    value: Option<&str>,
) -> Result<umadev_agent::memory_control::MemoryStore, MemoryParseError> {
    let Some(value) = value else {
        return Err(MemoryParseError::MissingStore);
    };
    match parse_memory_selector(value)? {
        umadev_agent::memory_control::MemorySelector::Store(store) => Ok(store),
        umadev_agent::memory_control::MemorySelector::Group(_) => {
            Err(MemoryParseError::ExactStoreRequired)
        }
    }
}

fn parse_memory_command(input: &str) -> Result<MemoryTuiCommand, MemoryParseError> {
    let mut tokens = tokenize_memory_args(input)?;
    if tokens.is_empty() {
        return Ok(MemoryTuiCommand::Inventory {
            scope: MemoryViewScope::Project,
        });
    }
    let operation = tokens.remove(0).to_ascii_lowercase();
    let options = parse_memory_options(&tokens)?;
    match operation.as_str() {
        "inventory" => {
            if !options.positional.is_empty()
                || options.store.is_some()
                || options.days.is_some()
                || options.output.is_some()
                || options.yes
                || options.clear
                || options.run
            {
                return Err(MemoryParseError::Usage);
            }
            Ok(MemoryTuiCommand::Inventory {
                scope: parse_memory_view_scope(options.scope.as_deref())?,
            })
        }
        "capture" | "recall" => {
            if options.positional.len() != 1
                || options.days.is_some()
                || options.output.is_some()
                || options.yes
                || options.clear
                || options.run
            {
                return Err(MemoryParseError::Usage);
            }
            let enabled = match options.positional[0].to_ascii_lowercase().as_str() {
                "on" => true,
                "off" => false,
                _ => return Err(MemoryParseError::Usage),
            };
            let scope = parse_memory_mutation_scope(options.scope.as_deref())?;
            let selector = options
                .store
                .as_deref()
                .map(parse_memory_selector)
                .transpose()?;
            if operation == "capture" {
                Ok(MemoryTuiCommand::Capture {
                    scope,
                    selector,
                    enabled,
                })
            } else {
                Ok(MemoryTuiCommand::Recall {
                    scope,
                    selector,
                    enabled,
                })
            }
        }
        "retention" => {
            if !options.positional.is_empty() || options.output.is_some() {
                return Err(MemoryParseError::Usage);
            }
            let changes = usize::from(options.days.is_some())
                + usize::from(options.clear)
                + usize::from(options.run);
            if changes > 1 || (options.yes && !options.run) {
                return Err(MemoryParseError::Usage);
            }
            if changes == 0 {
                let store = options
                    .store
                    .as_deref()
                    .map(|value| parse_exact_memory_store(Some(value)))
                    .transpose()?;
                return Ok(MemoryTuiCommand::RetentionView {
                    scope: parse_memory_view_scope(options.scope.as_deref())?,
                    store,
                });
            }
            let scope = parse_memory_mutation_scope(options.scope.as_deref())?;
            let store = parse_exact_memory_store(options.store.as_deref())?;
            if let Some(days) = options.days {
                let days = days
                    .parse::<u32>()
                    .ok()
                    .filter(|days| *days > 0)
                    .ok_or(MemoryParseError::InvalidDays)?;
                Ok(MemoryTuiCommand::RetentionSet { scope, store, days })
            } else if options.clear {
                Ok(MemoryTuiCommand::RetentionClear { scope, store })
            } else {
                Ok(MemoryTuiCommand::RetentionRun {
                    scope,
                    store,
                    confirmed: options.yes,
                })
            }
        }
        "export" => {
            if !options.positional.is_empty()
                || options.days.is_some()
                || options.clear
                || options.run
            {
                return Err(MemoryParseError::Usage);
            }
            let scope = parse_memory_mutation_scope(options.scope.as_deref())?;
            let selector = options.store.as_deref().map_or_else(
                || {
                    Ok(umadev_agent::memory_control::MemorySelector::Group(
                        umadev_agent::memory_control::MemoryGroup::All,
                    ))
                },
                parse_memory_selector,
            )?;
            let Some(destination) = options.output.map(PathBuf::from) else {
                return Err(MemoryParseError::MissingOutput);
            };
            if !destination.is_absolute() {
                return Err(MemoryParseError::AbsoluteOutputRequired);
            }
            Ok(MemoryTuiCommand::Export {
                scope,
                selector,
                destination,
                confirmed: options.yes,
            })
        }
        "forget" => {
            if !options.positional.is_empty()
                || options.days.is_some()
                || options.output.is_some()
                || options.clear
                || options.run
            {
                return Err(MemoryParseError::Usage);
            }
            let scope = parse_memory_mutation_scope(options.scope.as_deref())?;
            let selector = parse_memory_selector(
                options
                    .store
                    .as_deref()
                    .ok_or(MemoryParseError::MissingStore)?,
            )?;
            Ok(MemoryTuiCommand::Forget {
                scope,
                selector,
                confirmed: options.yes,
            })
        }
        "clear-cache" => {
            if options.days.is_some()
                || options.output.is_some()
                || options.clear
                || options.run
                || options.store.is_some()
                || options.positional.len() != 1
            {
                return Err(MemoryParseError::Usage);
            }
            let scope = parse_memory_mutation_scope(options.scope.as_deref())?;
            if scope != umadev_agent::memory_control::MemoryScope::Project {
                return Err(MemoryParseError::ProjectScopeRequired);
            }
            let store = parse_exact_memory_store(options.positional.first().map(String::as_str))?;
            if !store.clearable_cache() {
                return Err(MemoryParseError::InvalidArgument(store.id().to_string()));
            }
            Ok(MemoryTuiCommand::ClearCache {
                store,
                confirmed: options.yes,
            })
        }
        _ => Err(MemoryParseError::Usage),
    }
}

/// One composed user turn after visible chips have been resolved, but before the
/// editor state is cleared. `text` is safe for transcript/history display: it has
/// attachment labels, never local paths. `input` preserves exact block order for
/// the selected base's structured transport.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SubmittedTurn {
    pub(crate) text: String,
    pub(crate) input: TurnInput,
}

/// One serialized resident-session input waiting for the sole base writer.
/// Native commands stay typed while queued so they can never be reclassified as
/// chat, live steering, or a Director request when the preceding turn settles.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum ResidentDispatch {
    /// A normal natural-language turn that must go through model-first routing.
    RoutedChat(String),
    /// An advertised or explicitly wrapped base command sent byte-for-byte.
    NativeCommand(String),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum QueuedResidentKind {
    RoutedChat,
    NativeCommand,
}

impl SubmittedTurn {
    pub(crate) fn text(text: String) -> Self {
        Self {
            input: TurnInput::text(text.clone()),
            text,
        }
    }

    #[must_use]
    pub(crate) fn has_attachments(&self) -> bool {
        self.input
            .blocks
            .iter()
            .any(|block| !matches!(block, TurnInputBlock::Text { .. }))
    }
}

/// Max entries kept in the kill-ring (Ctrl+U/K/W feed it; Ctrl+Y / Alt+Y read
/// it). The oldest entry falls off the back when a fresh, distinct kill pushes
/// past the cap.
const KILL_RING_CAP: usize = 10;

/// Max snapshots kept on each undo / redo stack — bounds the memory a long
/// editing session can accrue while still reaching far further back than a user
/// ever asks for.
const UNDO_CAP: usize = 50;

/// How long after the previous edit-snapshot a new edit still COALESCES into it
/// (no fresh undo step). A rapid burst of keystrokes inside this window collapses
/// to one undo step; a pause longer than this opens the next step. Measured with
/// the same `Instant` clock the rest of the TUI uses — there is no wall-clock in
/// this environment.
const UNDO_COALESCE: std::time::Duration = std::time::Duration::from_millis(250);

/// Direction of a kill (Ctrl+U/K/W) so consecutive same-direction kills COALESCE
/// into one kill-ring entry the readline way: a forward kill (Ctrl+K — text to
/// the RIGHT of the caret) APPENDS to the front entry; a backward kill (Ctrl+U /
/// Ctrl+W — text to the LEFT) PREPENDS. A direction change, or any non-kill key,
/// starts a fresh ring entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KillDir {
    /// Text removed from AFTER the caret (Ctrl+K) — appends when coalescing.
    Forward,
    /// Text removed from BEFORE the caret (Ctrl+U / Ctrl+W) — prepends.
    Backward,
}

/// One point-in-time snapshot of the editable input: its text plus the caret
/// position (in CHARACTERS, matching [`App::input_cursor`]). The undo / redo
/// stacks are stacks of these, so a restore brings back both the text and where
/// the caret sat.
#[derive(Clone, Debug, Default)]
struct EditSnapshot {
    /// Input buffer contents at snapshot time.
    text: String,
    /// Caret position (char index) at snapshot time.
    cursor: usize,
}

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

/// Occupancy percent at which the proactive `/compact` nudge fires — high enough
/// that it's genuinely getting full, low enough to leave room before the base
/// fails with a "prompt too long" (the reactive `BaseFailure::Context` remedy).
pub(crate) const CONTEXT_NUDGE_PCT: u16 = 80;

/// Round `used / total` to a whole percent for the context-usage gauge. Clamped to
/// `0..=100` (the conservative denominator can under-count a larger real window, so
/// a raw ratio may exceed 100 — showing a capped `100%` reads as "full", never an
/// absurd `>100%`). Pure, saturating, `total == 0` → 0.
#[must_use]
pub(crate) fn context_usage_pct(used: u64, total: u64) -> u16 {
    if total == 0 {
        return 0;
    }
    let pct = used.saturating_mul(100) / total;
    u16::try_from(pct.min(100)).unwrap_or(100)
}

/// A spawned token-budgeted compaction job — everything the async summary task
/// needs, snapshotted off `&mut App` so the task never touches app state. The
/// task forks the base, summarises [`Self::folded`], and reports the outcome
/// back; the event loop applies it via [`App::apply_compaction`] /
/// [`App::fail_compaction`].
#[derive(Debug, Clone)]
pub(crate) struct CompactionJob {
    /// The older prefix of the working transcript to fold into one summary.
    pub(crate) folded: Vec<umadev_runtime::Message>,
    /// How many leading working-view messages the summary replaces.
    pub(crate) fold_count: usize,
    /// The conversation generation this job started under — an apply with a stale
    /// generation (a `/clear` / `/resume` happened meanwhile) is dropped.
    pub(crate) generation: u64,
}

/// Origin of a terminal routing failure.
///
/// Chat failures may suppress an exact duplicate retry. Director failures must
/// never use the chat dispatch key: an explicit `/run` does not dispatch that
/// text at all, and a model-promoted run leaves the original chat key stale once
/// it crosses the Director boundary.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum FailedRouteOrigin {
    /// An ordinary non-Director chat/agentic route.
    Chat,
    /// An explicit or model-promoted Director run.
    Director,
}

/// What the event loop should do after a key press.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Action {
    /// Nothing — keep looping.
    None,
    /// `Ctrl+V` — ask the event loop to read an image directly from the local
    /// OS clipboard. The blocking platform command never runs in `App` or on
    /// the render thread; its result comes back through the existing image-chip
    /// attachment path.
    PasteImage,
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
    /// Send an advertised base slash command directly through the resident
    /// session, bypassing semantic routing and UmaDev directive composition.
    NativeCommand(String),
    /// Change Kimi Code's model-owned thinking toggle through the resident ACP
    /// configuration channel. This is configuration, never chat text.
    SetThinking(bool),
    /// User submitted another turn while an ordinary resident base turn is
    /// active. The event loop dispatches this through the live session's typed
    /// steering surface when it is advertised; otherwise it queues the same
    /// structured snapshot for the next turn with vendor-specific wording.
    LiveInput(SubmittedTurn),
    /// Submit typed content through Grok Build's native prompt queue. The typed
    /// block sequence is retained intact; a failed dispatch restores it.
    PromptQueueEnqueue {
        /// Exact composed turn, including typed attachments.
        turn: SubmittedTurn,
        /// Append normally or use the base's native send-now operation.
        placement: PromptQueuePlacement,
    },
    /// Request one server-authoritative prompt-queue mutation. The App mirror
    /// remains unchanged until a later complete snapshot arrives.
    PromptQueueMutate(PromptQueueMutation),
    /// Ask the resident base for its complete, server-authoritative background
    /// process snapshot. Unsupported bases report that capability honestly.
    ListBackgroundProcesses,
    /// Stop one base-owned background process after the driver performs a fresh
    /// live-session ownership check.
    StopBackgroundProcess(String),
    /// User submitted text while a gate was active — record as a revision and
    /// re-run the most recent block.
    Revise(String),
    /// User asked a read-only question while a confirmation gate remains open.
    /// The event loop answers it on a fresh Plan-permission base surface without
    /// resolving, revising, or otherwise advancing the parked run.
    GateQuery {
        /// Monotonic app-local query generation. Late results from an aborted or
        /// superseded query are ignored instead of mutating a newer run.
        epoch: u64,
        /// The user's read-only question at the still-open gate.
        question: String,
    },
    /// The user TYPED the decision for a paused consequential-action approval
    /// (「批准」/"approve"/"y" → `true`, 「拒绝」/"deny"/"n" → `false`) while
    /// [`App::pending_approval`] was live. The event loop resolves the shared
    /// approval waiter with it (A2#5 — the typed-reply path; the empty-input
    /// y/n/Esc fast keys are handled before the key pipeline in lib.rs).
    ApprovalReply(bool),
    /// `/compact` — fold the older conversation turns into one structured summary
    /// via a forked base `complete()`. The event loop drives the async summary
    /// (and falls back to FIFO if the base is unreachable); the slash handler only
    /// validated there is enough to fold and signalled intent.
    Compact,
    /// `/cancel` — abort the in-flight pipeline task and return to the prompt
    /// (without quitting the app). The event loop owns the run task handle.
    Cancel,
    /// Backend was switched (saved to config); the engine task should be
    /// restarted on next `StartRun`.
    BackendChanged,
    /// The active Codex sandbox changed. A resident app-server thread keeps the
    /// permissions it was started with, so the event loop must close and
    /// immediately pre-load the session again before the next turn.
    SandboxChanged,
    /// `/init` changed project guidance or the effective slug. Re-prime the
    /// resident base so it reads the initialized workspace before the next turn.
    WorkspaceInitialized,
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
    /// `/theme auto` — re-fire the OSC 11 terminal background query so a
    /// terminal that supports it re-answers and the async reply refines the
    /// palette. The handler already released the manual pin and applied the
    /// synchronous heuristic; the event loop owns the `terminal` writer, so the
    /// escape query is issued there (mirrors [`Self::ForceRedraw`]).
    ReprobeTheme,
}

/// Classify a typed reply to a PAUSED consequential-action approval (A2#5):
/// `Some(true)` = allow, `Some(false)` = deny, `None` = not a decision (the text
/// falls through to the normal queued-chat / steering lanes untouched).
///
/// Thin wrapper over [`umadev_agent::classify_approval_reply`] — the agent crate
/// is the reply-classification home (the same one-source-of-truth discipline as
/// `gates::classify_reply` / `claims_code_changes`), so the trilingual
/// approve/deny vocabulary can never drift between the TUI and any other
/// surface. EXACT match only (trimmed, case-folded) — a decision this
/// consequential is never inferred from a substring of a longer steering
/// message.
#[must_use]
pub(crate) fn classify_approval_reply(text: &str) -> Option<bool> {
    umadev_agent::classify_approval_reply(text)
}

/// Read-only questions the TUI can answer from state it already owns.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LiveMetaIntent {
    Progress,
    Changes,
}

/// Bounded, trilingual live-meta classifier. It accepts polite/natural variants,
/// but rejects multiline, future-work, and chained-action requests so a status
/// lookup cannot swallow real work.
fn classify_live_meta(text: &str) -> Option<LiveMetaIntent> {
    const CHANGES: &[&str] = &[
        "这次改了什么",
        "这次改了啥",
        "这次都改了什么",
        "这次都改了啥",
        "这次改动都做了什么",
        "这次改动都做了啥",
        "本次改了什么",
        "本次改了啥",
        "本次改动",
        "本轮改动",
        "这轮改动",
        "改了哪些文件",
        "这次改了哪些文件",
        "這次改了什麼",
        "這次都改了什麼",
        "這次改動都做了什麼",
        "本次改動",
        "本輪改動",
        "這輪改動",
        "改了哪些檔案",
        "這次改了哪些檔案",
        "what changed",
        "what did you change",
        "what have you changed",
        "what changes did you make",
        "what files changed",
        "show me the changes",
        "what did this turn change",
    ];
    const PROGRESS: &[&str] = &[
        "当前进度",
        "目前进度",
        "现在进度",
        "进度怎么样",
        "进度怎么样了",
        "现在做到哪了",
        "做到哪了",
        "进行到哪了",
        "现在在做什么",
        "当前在做什么",
        "任务进度",
        "當前進度",
        "目前進度",
        "現在進度",
        "進度怎麼樣",
        "進度怎麼樣了",
        "現在做到哪了",
        "進行到哪了",
        "現在在做什麼",
        "當前在做什麼",
        "任務進度",
        "current progress",
        "progress update",
        "what's the progress",
        "what is the progress",
        "where are we",
        "where are you at",
        "what are you doing",
        "what are you working on",
        "current status",
        "status update",
    ];

    let lowered = text.trim().to_lowercase();
    let normalized = lowered
        .trim_matches(|c: char| {
            c.is_ascii_punctuation()
                || matches!(c, '，' | '。' | '？' | '！' | '：' | '；' | '、' | '～')
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if CHANGES.contains(&normalized.as_str()) {
        return Some(LiveMetaIntent::Changes);
    }
    if PROGRESS.contains(&normalized.as_str()) {
        return Some(LiveMetaIntent::Progress);
    }

    // Natural variants stay deliberately bounded. A longer instruction or a
    // second requested action belongs to the normal intent router.
    if text.contains(['\n', '\r']) || normalized.chars().count() > 120 {
        return None;
    }
    let compact = normalized.split_whitespace().collect::<String>();
    let contains_any =
        |haystack: &str, needles: &[&str]| needles.iter().any(|n| haystack.contains(n));
    let chained_work = contains_any(
        &compact,
        &[
            "然后修",
            "然后改",
            "然后写",
            "然后跑",
            "然后测试",
            "然後修",
            "然後改",
            "然後寫",
            "然後測試",
            "并修",
            "并改",
            "并写",
            "并补",
            "并测试",
            "並修",
            "並改",
            "並寫",
            "並補",
            "並測試",
            "顺便修",
            "顺便改",
            "順便修",
            "順便改",
            "接着修",
            "接着改",
            "接著修",
            "接著改",
        ],
    ) || contains_any(
        &normalized,
        &[
            " and then ",
            " then fix",
            " then update",
            " then edit",
            " then run",
            " and fix",
            " and update",
            " and edit",
            " and run",
            " also fix",
            " also update",
        ],
    );
    if chained_work {
        return None;
    }

    let zh_scope = contains_any(
        &compact,
        &[
            "这次", "本次", "此次", "这轮", "本轮", "刚才", "刚刚", "這次", "此次", "這輪", "本輪",
            "剛才", "剛剛",
        ],
    );
    let zh_completed_change = contains_any(
        &compact,
        &[
            "改了",
            "修改了",
            "更新了",
            "变更了",
            "變更了",
            "改动",
            "改動",
            "变更",
            "變更",
            "的修改",
            "的更新",
        ],
    );
    let zh_change_question = contains_any(
        &compact,
        &[
            "什么",
            "什麼",
            "啥",
            "哪些",
            "哪几",
            "哪幾",
            "改了些什",
            "做了些什",
        ],
    );
    let zh_present = contains_any(
        &compact,
        &[
            "说下",
            "說下",
            "说说",
            "說說",
            "讲下",
            "講下",
            "告诉我",
            "告訴我",
            "总结",
            "總結",
            "列出",
            "展示",
            "说明",
            "說明",
            "介绍",
            "介紹",
        ],
    );
    let zh_future_change = contains_any(
        &compact,
        &[
            "要改", "需改", "需要", "应该", "應該", "计划", "計劃", "打算", "要求", "目标", "目標",
            "任务", "任務",
        ],
    );
    if !zh_future_change
        && zh_completed_change
        && (zh_change_question || zh_present)
        && (zh_scope || compact.contains("你改了") || compact.contains("您改了"))
    {
        return Some(LiveMetaIntent::Changes);
    }

    let english_future_change = contains_any(
        &normalized,
        &[
            "should change",
            "should update",
            "need to change",
            "need to update",
            "plan to change",
            "changes to make",
            "change requirements",
        ],
    );
    let english_change_question = contains_any(
        &normalized,
        &[
            "what changed",
            "what did you change",
            "what have you changed",
            "what you changed",
            "what files did you change",
            "which files did you change",
            "what did you update",
            "changes you made",
            "changes did you make",
            "summarize the changes",
            "summarise the changes",
            "summary of the changes",
            "show me the changes",
            "list the changes",
            "what was changed",
            "what got changed",
        ],
    );
    if english_change_question && !english_future_change {
        return Some(LiveMetaIntent::Changes);
    }

    let zh_progress = contains_any(&compact, &["进度", "進度", "进展", "進展"]);
    let zh_progress_question = contains_any(
        &compact,
        &[
            "怎么样",
            "怎麼樣",
            "如何",
            "到哪",
            "哪一步",
            "多少",
            "什么情况",
            "什麼情況",
            "啥情况",
            "嗎",
            "吗",
        ],
    );
    let zh_progress_where = contains_any(
        &compact,
        &[
            "做到哪",
            "进行到哪",
            "進行到哪",
            "处理到哪",
            "處理到哪",
            "弄到哪",
        ],
    );
    let zh_work_object = contains_any(
        &compact,
        &[
            "组件", "組件", "页面", "頁面", "代码", "代碼", "函数", "函數", "接口", "文件", "檔案",
            "配置", "测试", "測試",
        ],
    );
    if !zh_work_object
        && ((zh_progress && (zh_progress_question || zh_present)) || zh_progress_where)
    {
        return Some(LiveMetaIntent::Progress);
    }

    let english_progress = contains_any(
        &normalized,
        &[
            "how is it going",
            "how far along",
            "what's the progress",
            "what is the progress",
            "current progress",
            "progress update",
            "where are we",
            "where are you at",
            "what are you doing",
            "what are you working on",
            "current status",
            "status update",
            "what stage are we",
            "what stage are you",
            "what step are we on",
            "what step are you on",
            "tell me the current progress",
            "give me a progress update",
            "show me the current progress",
        ],
    );
    let english_progress_prompt = contains_any(
        &normalized,
        &[
            "what",
            "where",
            "how",
            "tell me",
            "show me",
            "give me",
            "can you",
            "could you",
            "please",
        ],
    );
    let english_work_object = contains_any(
        &normalized,
        &[
            " component",
            " page",
            " function",
            " endpoint",
            " widget",
            " source file",
            " code",
            " api",
        ],
    );
    (english_progress && english_progress_prompt && !english_work_object)
        .then_some(LiveMetaIntent::Progress)
}

/// One derived truth for the run's lifecycle, so the status label, the elapsed
/// timer, the mode marker and the input placeholder all read the SAME state
/// instead of independently inspecting the raw run flags (which drifted apart).
/// Derived by [`App::run_state`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RunState {
    /// Actively working — a live turn is thinking, OR a block's phase clock is
    /// running and we are not parked at a gate. The elapsed `[time]` timer runs
    /// IFF the state is `Running`.
    Running,
    /// Parked at a confirmation gate, awaiting the user's decision.
    PausedAtGate,
    /// Parked because the wall-clock budget was exhausted while resumable steps
    /// remained (Stage 1/2). Reads like a pause — the timer stops and the label is
    /// `[paused]`, never `[aborted]`/`[degraded]` — and the hint points at
    /// `/continue`. Distinct from [`Self::PausedAtGate`]: there is no gate card, just
    /// a resumable plan on disk.
    PausedAtBudget,
    /// A full run reached delivery cleanly (`[ok] delivered`).
    Delivered,
    /// A block ended TERMINALLY short of delivery because the base degraded to
    /// placeholder templates or failed a hard gate — fixable with `/redo`.
    Degraded,
    /// A block errored out before producing any phase (an honest hard abort).
    Aborted,
    /// No run in flight.
    Idle,
}

/// `true` when a progress `Note` carries one of the runner's STABLE,
/// language-independent DEGRADED / failed markers — the signal a terminal
/// [`EngineEvent::BlockCompleted`] short of delivery uses to tell a genuine
/// degrade/failure (→ `[degraded] … /redo`) apart from a CLEAN short-circuit
/// (docs-only / light pipeline / clean `/redo` → a plain finished stop).
///
/// These three markers are runner-internal tags (not translated prose — the
/// runner's own tests assert them as stable), so matching them never breaks
/// under i18n:
/// - `[WARN][降级]` — a phase fell back to an offline placeholder template;
/// - `UD-EVID-003` — the blocked-quality-gate hard-stop evidence code;
/// - `未产出任何真实代码文件` — the "no real code produced" hard failure.
#[must_use]
fn note_signals_degraded(note: &str) -> bool {
    note.contains("[WARN][降级]")
        || note.contains("UD-EVID-003")
        || note.contains("未产出任何真实代码文件")
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
    /// Stable id of one of the registered host backends.
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
/// first-launch picker so a user sees the runtime choices at a glance.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PickerGroup {
    /// First-run UI language choice (zh-CN / zh-TW / en), rendered first.
    Language,
    /// Drive one of the five supported logged-in host CLIs.
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
    /// Canonical role id of the seat that owns this step (`architect`,
    /// `frontend-engineer`, …), parsed from the `PlanPosted` summary's trailing
    /// `(seat)` token. Empty when the summary carried no resolvable seat — such a
    /// step simply doesn't join the live roster (anti-theater: no phantom seats).
    pub seat: String,
}

/// Plain-text status glyph for a plan step — the SAME marks the live `/plan`
/// transcript card renders (`done` `[x]`, `active` `[~]`, `blocked` `[!]`, any
/// other/pending `[ ]`). Shared by [`App::show_plan_status`] and the
/// build-completion card so the card's persisted task breakdown matches the
/// panel exactly and the glyph map lives in ONE place. Fail-open: an unknown
/// status falls through to `[ ]`. (The colored ratatui panel uses its own
/// styled `checklist_glyph` in `ui.rs`; this is the plain-markdown twin.)
#[must_use]
fn plan_step_glyph(status: &str) -> &'static str {
    match status {
        "done" => "[x]",
        "active" => "[~]",
        "blocked" => "[!]",
        // A step that was mid-flight when a transient abort froze the plan — shown
        // paused (`[=]`), a static state distinct from the live `[~]` spinner.
        "paused" => "[=]",
        _ => "[ ]",
    }
}

/// Lifecycle status of a background run task tracked by the [`App`] task
/// registry. A workspace-mutating run is single-writer, so at most one task is
/// [`Running`](Self::Running) at a time; the rest are settled history rows.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TaskStatus {
    /// The run is in flight (driving the base session / paused at a gate).
    Running,
    /// The run reached a clean terminal outcome (delivery / agentic build done).
    Done,
    /// The run ended by aborting / hard-stopping (an error, not a user cancel).
    Failed,
    /// The user cancelled the run (`/cancel`, `/tasks stop`, Esc/Ctrl-C).
    Stopped,
}

impl TaskStatus {
    /// i18n key for the localized one-word status label rendered by `/tasks`.
    #[must_use]
    pub fn label_key(self) -> &'static str {
        match self {
            TaskStatus::Running => "tasks.status.running",
            TaskStatus::Done => "tasks.status.done",
            TaskStatus::Failed => "tasks.status.failed",
            TaskStatus::Stopped => "tasks.status.stopped",
        }
    }

    /// Whether this is a live (non-settled) status — exactly [`Running`](Self::Running).
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, TaskStatus::Running)
    }

    /// Stable, language-agnostic id used to PERSIST this status to
    /// `.umadev/tasks.json` (distinct from [`label_key`](Self::label_key), which
    /// is an i18n key for display). Storage ids must never localize.
    #[must_use]
    fn persist_id(self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Stopped => "stopped",
        }
    }

    /// Inverse of [`persist_id`](Self::persist_id): parse a stored status id back
    /// to a [`TaskStatus`], or `None` for an unrecognized value (fail-open — the
    /// caller settles an unknown row to a safe terminal status).
    #[must_use]
    fn from_persist_id(s: &str) -> Option<Self> {
        match s {
            "running" => Some(TaskStatus::Running),
            "done" => Some(TaskStatus::Done),
            "failed" => Some(TaskStatus::Failed),
            "stopped" => Some(TaskStatus::Stopped),
            _ => None,
        }
    }
}

fn agent_task_status_label(
    lang: umadev_i18n::Lang,
    state: umadev_agent::task_lifecycle::AgentTaskState,
) -> &'static str {
    use umadev_agent::task_lifecycle::AgentTaskState;
    let key = match state {
        AgentTaskState::Queued => "tasks.agent.status.queued",
        AgentTaskState::Running => "tasks.status.running",
        AgentTaskState::Waiting => "tasks.agent.status.waiting",
        AgentTaskState::Succeeded => "tasks.status.done",
        AgentTaskState::Failed => "tasks.status.failed",
        AgentTaskState::Cancelled => "tasks.status.stopped",
        AgentTaskState::Unavailable => "tasks.agent.status.unavailable",
        AgentTaskState::Superseded => "tasks.agent.status.superseded",
        AgentTaskState::Interrupted => "tasks.agent.status.interrupted",
    };
    umadev_i18n::t(lang, key)
}

fn agent_run_status_label(
    lang: umadev_i18n::Lang,
    readiness: &umadev_agent::task_lifecycle::RunReadiness,
) -> &'static str {
    use umadev_agent::task_lifecycle::RunReadiness;
    let key = match readiness {
        RunReadiness::NotTracked => "tasks.agent.status.untracked",
        RunReadiness::InProgress => "tasks.status.running",
        RunReadiness::Succeeded => "tasks.status.done",
        RunReadiness::Blocked(_) => "tasks.status.failed",
    };
    umadev_i18n::t(lang, key)
}

/// Serde DTO for persisting one [`BackgroundTask`] row to `.umadev/tasks.json`.
/// Mirrors the in-memory task but stores `started_at` as a wall-clock unix stamp
/// (an `Instant` isn't serializable across launches) and the status as its
/// stable [`TaskStatus::persist_id`] string.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedTask {
    /// Short display id (`t1`, `t2`, …).
    id: String,
    /// One-line requirement summary.
    requirement: String,
    /// Stable status id (`running` | `done` | `failed` | `stopped`).
    status: String,
    /// Unix seconds at registration (`0` if unknown). All `#[serde(default)]` so
    /// a partially-written / older file still deserializes (fail-open).
    #[serde(default)]
    started_at_unix: u64,
    /// Completed plan steps.
    #[serde(default)]
    done: usize,
    /// Total plan steps.
    #[serde(default)]
    total: usize,
}

/// Serde DTO for the whole persisted task registry (`.umadev/tasks.json`): the
/// `t<n>` id sequence plus the bounded list of recent task rows.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct PersistedTasks {
    /// The last-minted `task_seq` so reloaded ids never collide with new ones.
    #[serde(default)]
    seq: u64,
    /// Recent task rows, newest last (bounded by [`TASKS_CAP`]).
    #[serde(default)]
    tasks: Vec<PersistedTask>,
}

/// Current wall-clock time in unix seconds, `0` if the clock is before the epoch
/// (never panics — fail-open).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Rebuild an [`std::time::Instant`] for a task registered at `started_at_unix`
/// wall-clock seconds, offset back from now by the task's age so the elapsed
/// readout stays roughly right after a relaunch. Fail-open: a future/garbage
/// stamp (or a checked-sub underflow) collapses to "now" (elapsed 0).
fn instant_from_age(started_at_unix: u64) -> std::time::Instant {
    let now = std::time::Instant::now();
    let age = unix_now().saturating_sub(started_at_unix);
    now.checked_sub(std::time::Duration::from_secs(age))
        .unwrap_or(now)
}

/// One background run in the task registry — the manageable surface that turns a
/// `/run` from a modal "pipeline running" lock-out into a steerable task the user
/// can list, stop, and resume via `/tasks` while they keep scrolling / chatting.
///
/// Single-writer (the run-lock) means only one mutating run is `Running` at a
/// time; finished tasks are kept as a bounded short history.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BackgroundTask {
    /// Stable, short display id (`t1`, `t2`, …) minted when the run registers.
    pub id: String,
    /// The run's requirement, trimmed to a one-line summary for the list.
    pub requirement: String,
    /// Current lifecycle status.
    pub status: TaskStatus,
    /// When the run was registered — for the live elapsed readout.
    pub started_at: std::time::Instant,
    /// Wall-clock unix-seconds stamp of registration. An [`std::time::Instant`]
    /// can't be serialized (no portable epoch), so this is what
    /// the app's internal task persistence writes; on reload the task loader turns it back
    /// into a `started_at` offset by the task's age. `0` when the clock is
    /// unavailable (fail-open).
    pub started_at_unix: u64,
    /// Completed plan steps (the `X` in `X/Y`); `0` until a plan posts.
    pub done: usize,
    /// Total plan steps (the `Y` in `X/Y`); `0` until a plan posts.
    pub total: usize,
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
    /// Suggested one-line FIX per blocking finding — the seat's "how to fix",
    /// index-aligned with `blocking`. Surfaced so a blocked run shows a concrete
    /// next-step, not just the problem. May be empty / shorter than `blocking`
    /// (a blocker with no suggestion carries none — fail-open).
    pub remediation: Vec<String>,
    /// Nice-to-have notes (may be empty).
    pub advisory: Vec<String>,
}

impl CriticRow {
    /// The suggested one-line fix for the blocking finding at `idx`, if the seat
    /// emitted one (`remediation` is index-aligned with `blocking`). `None` when no
    /// matching, non-blank suggestion exists — the caller then shows the blocker
    /// alone, never a fabricated fix (fail-open).
    #[must_use]
    pub fn fix_for(&self, idx: usize) -> Option<&str> {
        self.remediation
            .get(idx)
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

/// Live status of one convened teammate in the **team roster** (Wave C). Derived
/// deterministically from that seat's plan steps' statuses (never narrated): a
/// blocked step wins, else an active step (a doing seat is `Working`, a reviewing
/// seat is `Reviewing`), else all-done is `Done`, else `Idle`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SeatStatus {
    /// The seat is convened but none of its steps have started.
    Idle,
    /// A doing seat (frontend / backend / devops) has an active step.
    Working,
    /// A reviewing seat (pm / architect / designer / qa / security) has an active step.
    Reviewing,
    /// One of the seat's steps is blocked.
    Blocked,
    /// Every step the seat owns is done.
    Done,
}

impl SeatStatus {
    /// i18n key for the localized one-word status label rendered in the roster.
    #[must_use]
    pub fn label_key(self) -> &'static str {
        match self {
            SeatStatus::Idle => "team.status.idle",
            SeatStatus::Working => "team.status.working",
            SeatStatus::Reviewing => "team.status.reviewing",
            SeatStatus::Blocked => "team.status.blocked",
            SeatStatus::Done => "team.status.done",
        }
    }
}

/// One convened teammate in the live **team roster** panel (Wave C). Built only
/// from seats that own a real plan step — the anti-theater floor: a decorative
/// full roster is never shown, only the seats actually working this run.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RosterSeat {
    /// Canonical role id (e.g. `architect`) — the key for the localized name +
    /// for matching a [`CriticRow`] verdict.
    pub role: String,
    /// Live status aggregated from the seat's plan steps.
    pub status: SeatStatus,
    /// The seat's latest verdict, if a [`EngineEvent::CriticVerdict`] landed for
    /// it — `(accepts, blocking_count)`. `None` until the seat reviews.
    pub verdict: Option<(bool, usize)>,
}

/// One entry in the **handoff timeline** (Wave C) — recorded when a plan step
/// flips to `done`, i.e. a seat handed its finished deliverable downstream
/// ("architect → API contract → frontend / backend pick it up"). A handoff is a
/// real DONE transition, never a narration (anti-theater).
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Handoff {
    /// Canonical role id of the seat that completed the step (`""` when the step
    /// carried no resolvable seat).
    pub seat: String,
    /// The completed step's title (the deliverable that was handed off).
    pub title: String,
}

/// Hard cap on the retained handoff timeline so a long build can't grow the log
/// without bound; the oldest entries roll off (the panel/`/team` show the tail).
const HANDOFFS_CAP: usize = 24;

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

/// Map a canonical seat role id to the i18n key for its **short** display name
/// (the one-word label used in the live roster / handoff timeline, distinct from
/// the verbose role→deliverable line in [`TEAM_ROSTER`]). An unknown role falls
/// back to the raw id so a future seat still renders something legible.
#[must_use]
pub(crate) fn seat_name_key(role: &str) -> Option<&'static str> {
    match role {
        "product-manager" => Some("team.seat.pm"),
        "architect" => Some("team.seat.architect"),
        "uiux-designer" => Some("team.seat.designer"),
        "frontend-engineer" => Some("team.seat.frontend"),
        "backend-engineer" => Some("team.seat.backend"),
        "qa-engineer" => Some("team.seat.qa"),
        "security-engineer" => Some("team.seat.security"),
        "devops-engineer" => Some("team.seat.devops"),
        _ => None,
    }
}

/// The localized short name of a seat for the roster / handoff surfaces. Resolves
/// `role` (or a free-text alias) to its canonical seat first so `qa` and
/// `qa-engineer` print the same name; an unresolvable role prints verbatim.
#[must_use]
pub(crate) fn seat_display_name(lang: umadev_i18n::Lang, role: &str) -> String {
    let canonical = umadev_agent::Seat::from_alias(role).map_or(role, |s| s.role_id());
    match seat_name_key(canonical) {
        Some(key) => umadev_i18n::t(lang, key).to_string(),
        None => role.to_string(),
    }
}

/// Whether a canonical seat role is a **doing** seat (drives the main session) vs
/// a reviewing seat — reuses the agent's own `Seat::is_doer` so the doer set is
/// one source of truth. An unresolvable role is treated as a reviewer (the
/// conservative default; a non-doer never claims to be "working").
fn is_doer_role(role: &str) -> bool {
    umadev_agent::Seat::from_alias(role).is_some_and(umadev_agent::Seat::is_doer)
}

/// Map one plan step's `(seat, status)` to the live [`SeatStatus`] it implies: a
/// blocked step is `Blocked`, an active step is `Working` for a doing seat /
/// `Reviewing` for a reviewing seat, a done step is `Done`, and anything else
/// (pending / unknown) is `Idle`.
fn step_seat_status(role: &str, status: &str) -> SeatStatus {
    match status {
        "blocked" => SeatStatus::Blocked,
        "active" => {
            if is_doer_role(role) {
                SeatStatus::Working
            } else {
                SeatStatus::Reviewing
            }
        }
        "done" => SeatStatus::Done,
        // pending and any unrecognised status — the seat hasn't started this step.
        _ => SeatStatus::Idle,
    }
}

/// Precedence rank for aggregating a seat's many steps into ONE live status. A
/// blocked step dominates, then an in-flight (working/reviewing) step, then a
/// not-yet-started (idle) step, and only an all-done seat reads as `Done` — so a
/// seat with one done + one pending step correctly reads as `Idle`, not `Done`.
fn seat_status_rank(s: SeatStatus) -> u8 {
    match s {
        SeatStatus::Done => 0,
        SeatStatus::Idle => 1,
        SeatStatus::Working | SeatStatus::Reviewing => 2,
        SeatStatus::Blocked => 3,
    }
}

/// Fold one more step's status into a seat's accumulated live status, keeping the
/// higher-precedence of the two (see [`seat_status_rank`]). The accumulator is
/// seeded with [`SeatStatus::Done`] (the lowest rank) so an all-done seat stays
/// `Done`.
fn merge_seat_status(acc: SeatStatus, role: &str, status: &str) -> SeatStatus {
    let s = step_seat_status(role, status);
    if seat_status_rank(s) > seat_status_rank(acc) {
        s
    } else {
        acc
    }
}

/// Source of a chat message — used to colour the role label.
///
/// Serde-derived (Wave 3): the visible display transcript is persisted inside
/// the private persisted chat-session representation so a relaunch rebuilds the same screen; an unknown variant
/// in a newer file simply fails that one row's lenient parse (fail-open).
#[derive(Debug, Copy, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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
/// always stay expanded). Serde-derived so a persisted tool row round-trips
/// its terminal state (Wave 3 — display-transcript persistence).
#[derive(Debug, Copy, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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
/// Serde-derived (Wave 3) so a persisted display transcript keeps tool rows
/// structured across a relaunch instead of flattening them to prose.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    /// Stable base-protocol call id. Older persisted rows and legacy stream
    /// drivers have none; correlated ACP rows retain it so interleaved updates
    /// can never settle the wrong card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// The tool's name as the base reports it (`Read` / `Edit` / `Bash` …).
    pub name: String,
    /// The primary argument (a path, a query, a command) — already truncated
    /// to a sane width; rendered dim in parentheses after the name.
    pub arg: String,
    /// Current lifecycle state — drives the status glyph + auto-collapse.
    pub status: ToolStatus,
    /// The result summary once the call returns (`None` while in flight).
    pub result: Option<String>,
    /// Complete non-terminal status-title replacement reported by the base.
    /// Kept separate from `result`: it is card state, not stdout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
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
/// [`App::palette_matches`] from [`App::COMMANDS`] for the active language.
#[derive(Debug, Clone)]
pub struct PaletteEntry<'a> {
    /// The verb to insert on autocomplete (no leading slash).
    pub verb: &'a str,
    /// Localized one-line description (already resolved for the active language).
    pub desc: &'a str,
    /// Optional dim ghost-text argument hint.
    pub arg_hint: Option<&'a str>,
}

/// One rendered line of a diff card. The `tag` is the gutter marker; `line_no`
/// is the (1-based) line number in the AFTER file for an add/context line, or in
/// the BEFORE file for a deletion — whichever the row belongs to (so the number
/// column tracks the file you can actually open). `text` is the raw content
/// WITHOUT the +/-/space prefix (the gutter carries that), syntax-highlighted by
/// the renderer per the file extension. Serde-derived (Wave 3) so a persisted
/// diff card survives a relaunch line-for-line.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FileDiff {
    /// Stable base-protocol call id, when the edit came from a correlated tool
    /// stream. It is presentation-only routing metadata and renders nowhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
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
    /// groups the changes into hunks with a bounded number of surrounding lines
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
            call_id: None,
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

// ── I8 — fzf-style positional fuzzy scorer ──────────────────────────────────
// A dependency-free positional/boundary scorer shared by the `@`-mention
// typeahead and the slash-command palette. It replaces the old 3-tier
// subsequence rank: a boundary / path match (`@src/main.rs` → `src/main.rs`)
// now outranks an incidental subsequence hit, and an exact / prefix command
// still sorts first (the callers keep an explicit exact→prefix→fuzzy tier on
// top of the score). Matching is case-insensitive (ASCII-folded so char indices
// stay 1:1 and camelCase boundaries in the ORIGINAL case survive), and the
// greedy earliest-match scan doubles as the subsequence existence test.

/// Reward per matched char (the floor every hit contributes).
const FZ_MATCH: i32 = 16;
/// Bonus for a match at a word boundary (string start, after a `/._- ` / `\`
/// separator, or a camelCase lower→upper transition).
const FZ_BONUS_BOUNDARY: i32 = 18;
/// Bonus for a match adjacent to the previous matched char that does NOT itself
/// start at a boundary — the floor a contiguous run earns. A run that STARTED at
/// a boundary instead inherits the (larger) boundary bonus for its whole length,
/// so `main` matched contiguously at `src/main.rs` beats the same chars buried
/// mid-word (fzf's "consecutive inherits the run-start bonus" rule).
const FZ_BONUS_CONSECUTIVE: i32 = 14;
/// Extra nudge when the very first haystack char is the first match (a true
/// prefix start), on top of its boundary bonus.
const FZ_BONUS_FIRST: i32 = 8;
/// Penalty for opening a gap (the first skipped char between two matches).
const FZ_GAP_START: i32 = 3;
/// Penalty per additional skipped char inside a gap.
const FZ_GAP_EXT: i32 = 1;
/// Penalty per skipped char before the first match (a long leading skip is
/// worse than a hit near the start), capped by [`FZ_MAX_LEAD`].
const FZ_PENALTY_LEADING: i32 = 1;
/// Cap on the leading-skip penalty so one very deep path can't dominate.
const FZ_MAX_LEAD: i32 = 12;
/// Cap on a single gap's penalty so one long gap can't dominate.
const FZ_MAX_GAP_PENALTY: i32 = 12;

/// True when `cur` (at `pos`) starts a new "word" in `hay`: the string start,
/// just after a path/word separator, or a camelCase lower→upper transition.
/// `hay` is the ORIGINAL-case char slice so the camelCase signal survives.
fn fz_is_boundary(hay: &[char], pos: usize) -> bool {
    if pos == 0 {
        return true;
    }
    let prev = hay[pos - 1];
    if matches!(prev, '/' | '\\' | '_' | '-' | '.' | ' ') {
        return true;
    }
    // camelCase / PascalCase word start: a non-uppercase char followed by an
    // uppercase one (`mainComponent` → the `C`).
    !prev.is_uppercase() && hay[pos].is_uppercase()
}

/// fzf-style positional fuzzy score: `Some(score)` when every char of `needle`
/// appears in `haystack` in order (higher = better), `None` when `needle` is
/// not a subsequence. Case-insensitive (ASCII fold). An empty needle scores `0`
/// (callers short-circuit the empty query before ranking). Rewards
/// word-boundary / camelCase / consecutive-char hits, penalizes gaps + a long
/// leading skip, and applies a mild shorter-haystack bonus so a tight match on a
/// short path outranks a loose one on a long path.
fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    let nq: Vec<char> = needle.chars().map(|c| c.to_ascii_lowercase()).collect();
    if nq.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().collect();
    if hay.len() < nq.len() {
        return None;
    }
    // Bitmap-style pre-reject: every query char must be present at all (a cheap
    // set membership) before the ordered scan.
    let present: std::collections::HashSet<char> =
        hay.iter().map(char::to_ascii_lowercase).collect();
    if !nq.iter().all(|c| present.contains(c)) {
        return None;
    }
    let mut score: i32 = 0;
    let mut cursor = 0usize; // next haystack index to search from
    let mut prev_match: Option<usize> = None;
    // The positional bonus the PREVIOUS matched char earned, so a consecutive run
    // can inherit a strong run-start (boundary) bonus across its whole length.
    let mut prev_bonus: i32 = 0;
    for &nc in &nq {
        // Earliest match at-or-after the cursor. Earliest-match greedy is a
        // COMPLETE subsequence test, so a `None` here is an authoritative miss.
        let pos = (cursor..hay.len()).find(|&j| hay[j].to_ascii_lowercase() == nc)?;
        let boundary = fz_is_boundary(&hay, pos);
        let consecutive = matches!(prev_match, Some(p) if pos == p + 1);
        // The per-char bonus: a boundary always wins; otherwise a consecutive
        // char inherits the run-start bonus (≥ the bare consecutive floor); an
        // isolated interior char earns nothing.
        let bonus = if boundary {
            FZ_BONUS_BOUNDARY + i32::from(pos == 0) * FZ_BONUS_FIRST
        } else if consecutive {
            prev_bonus.max(FZ_BONUS_CONSECUTIVE)
        } else {
            0
        };
        score += FZ_MATCH + bonus;
        match prev_match {
            // A gap between this match and the previous one: a start penalty plus
            // a per-extra-char extension, capped so one long gap can't dominate.
            Some(p) if !consecutive => {
                let skipped = i32::try_from(pos - p - 1).unwrap_or(FZ_MAX_GAP_PENALTY);
                let gap = FZ_GAP_START + (skipped - 1).max(0) * FZ_GAP_EXT;
                score -= gap.min(FZ_MAX_GAP_PENALTY);
            }
            // Leading skip before the first match (capped).
            None if pos > 0 => {
                let lead = i32::try_from(pos).unwrap_or(FZ_MAX_LEAD);
                score -= lead.min(FZ_MAX_LEAD) * FZ_PENALTY_LEADING;
            }
            _ => {}
        }
        prev_match = Some(pos);
        prev_bonus = bonus;
        cursor = pos + 1;
    }
    // Shorter-path bonus: a mild length penalty so a tight match on a short
    // candidate edges out the same match buried in a longer one.
    score -= i32::try_from(hay.len()).unwrap_or(0) / 4;
    Some(score)
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

/// I9 — the repo file the first-run example tip names: the most recently
/// MODIFIED source file under `root`, as a repo-relative `/`-separated path, so
/// "重构 <file>" / "refactor <file>" points at something the user actually just
/// touched. Bounded walk reusing the @-mention scan's skip rules (hidden dirs,
/// `target`, `node_modules`) and caps. Restricted to common source extensions so
/// the example never suggests acting on a lockfile or binary. `None` when the
/// repo has no recognisable source file or is unreadable — the caller then falls
/// back to a generic token. Fail-open: never panics.
fn most_recently_modified_source_file(root: &std::path::Path) -> Option<String> {
    const SRC_EXTS: &[&str] = &[
        "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "rb", "php", "c", "cc",
        "cpp", "h", "hpp", "cs", "swift", "kt", "vue", "svelte", "css", "scss", "less", "html",
    ];
    let mut best: Option<(std::time::SystemTime, String)> = None;
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut scanned = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if scanned >= MENTION_FILE_CAP {
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
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            scanned += 1;
            let path = entry.path();
            let is_src = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| SRC_EXTS.contains(&e.to_ascii_lowercase().as_str()));
            if !is_src {
                continue;
            }
            let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            // Keep the newest; a lexicographic tie-break on the path so an
            // equal-mtime repo picks the same file deterministically each launch.
            let better = match &best {
                Some((t, cur)) => mtime > *t || (mtime == *t && rel.as_str() < cur.as_str()),
                None => true,
            };
            if better {
                best = Some((mtime, rel));
            }
        }
    }
    best.map(|(_, rel)| rel)
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

fn supported_image_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP")
}

fn image_extension_matches(path: &std::path::Path, bytes: &[u8]) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        Some("jpg" | "jpeg") => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        Some("gif") => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        Some("webp") => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

/// Normalise a pasted path token the way a terminal mangles a dragged file:
/// drop a `file://` scheme, strip one layer of matching outer quotes, and (on
/// unix) undo shell backslash-escapes (`my\ pic.png` → `my pic.png`). Leaves a
/// plain path untouched.
///
/// The backslash-unescape is **unix-only**: a unix terminal escapes spaces in a
/// dragged path with `\`, but on WINDOWS the backslash is the PATH SEPARATOR
/// (`C:\Users\…\shot.png`), so stripping it would corrupt every pasted path
/// (`C:\Users\…` → `C:Users…`) and the later `canonicalize` would fail — a real
/// bug for windows users dragging an image in, not just a test artefact. There
/// the path is passed through with its separators intact.
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
    // On windows the backslash is a path separator, never a shell escape — keep
    // the path verbatim so it stays canonicalisable.
    if cfg!(windows) {
        return s.to_string();
    }
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
/// construction. Serde-derived (Wave 3, externally tagged): the display
/// transcript persists these rows structured, and a variant a given binary
/// doesn't know fails only that row's lenient parse (fail-open).
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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
                let progress = t
                    .progress
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| format!(" · {value}"))
                    .unwrap_or_default();
                std::borrow::Cow::Owned(format!("{mark} {}{count}{arg}{progress}{result}", t.name))
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

/// One row in the chat history. Serde-derived (Wave 3) — the unit the
/// persisted display transcript stores and the
/// relaunch rebuild restores, so the reopened screen matches the closed one.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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

/// **Feature B — in-transcript search state.** A live incremental find over the
/// folded/visual transcript rows (the same `transcript_rows` cache the
/// drag-to-copy selection uses). Owned by [`App::search`]; while it is `Some`
/// the search bar is open and it is the active modal input mode.
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    /// The live query — matched case-insensitively as a substring of each row.
    pub query: String,
    /// Matches from the last scan over `transcript_rows`, in painted-row order.
    /// Recomputed on every query edit; read by the renderer to paint highlights.
    pub matches: Vec<SearchMatch>,
    /// Index into [`Self::matches`] of the focused match — the one scrolled into
    /// view and painted with the brighter "current" wash. `0` when no matches.
    pub current: usize,
}

/// One search hit: a char span on a single folded/visual transcript row, in
/// LOGICAL (gutter-stripped) coordinates — the same space `transcript_rows`
/// stores, so the renderer maps it to the decorated line by adding the row's
/// gutter width (exactly as the selection highlight does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    /// Visual-row index into the folded transcript (== `transcript_rows` index).
    pub row: usize,
    /// First char (inclusive) of the match in the logical row text.
    pub start: usize,
    /// One-past-the-last char (exclusive) of the match.
    pub end: usize,
}

/// State for the **reverse prompt-history search** (I3 — Ctrl+R): a modal,
/// incremental substring find over the deduplicated submitted-prompt ring,
/// newest-first, with a live preview of the focused entry. Distinct from the
/// transcript [`SearchState`] (Ctrl+F): this searches what the user *typed*, not
/// what's on screen. Enter loads the focused match into the input box; Esc
/// cancels without touching the prompt.
#[derive(Debug, Clone, Default)]
pub struct HistorySearchState {
    /// The live query — matched case-insensitively as a substring of each entry.
    pub query: String,
    /// Deduplicated prompt-history snapshot taken when the mode opened, NEWEST
    /// FIRST (the order matches step through, oldest last). Repeated prompts
    /// appear once, at their most-recent position.
    pub entries: Vec<String>,
    /// Indices into [`Self::entries`] that match the current query, newest-first.
    /// Recomputed on every query edit; an empty query matches everything.
    pub matches: Vec<usize>,
    /// Index into [`Self::matches`] of the focused entry (the live preview / the
    /// one Enter loads). `0` when there are no matches.
    pub current: usize,
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
    /// Set by the event loop when THIS key landed in a sub-`PASTE_BURST_GAP` burst — faster
    /// than any human types, i.e. part of a PASTE. On Windows the console delivers a bracketed
    /// paste as raw key events (never a crossterm `Event::Paste`), so a newline INSIDE a pasted
    /// multi-line requirement arrives as a bare Enter; this flag lets the Enter handler insert
    /// it instead of submitting (which truncated the paste at the first line). Never set in
    /// tests (they call `apply_key` directly), so submit is unaffected off the real loop.
    pub key_arrived_in_burst: bool,
    /// Two-step SOFT confirm for a base the login PROBE reports as NOT logged in: the id
    /// awaiting a SECOND select. The probe is a false negative for a base pointed at a LOCAL /
    /// third-party model (needs no `<base> auth login`), and the product contract is "drive
    /// whatever the base is configured with" — so a not-logged-in base warns once, then the
    /// same base selected again proceeds. `None` = no pending confirm.
    pub picker_login_confirm: Option<String>,

    /// Chat input buffer (UTF-8 String — mutate via cursor helpers,
    /// never via raw push/pop, so multi-byte chars stay intact).
    pub input: String,
    /// Caret position within `input`, measured in **characters** (not bytes).
    /// `0` = before first char; `chars().count()` = after last char.
    pub input_cursor: usize,
    /// Image attachments for the turn being composed. A dragged/pasted image path
    /// is stored here and shown in `input` as an `[图片 N]` chip. Submission turns
    /// the chip into a typed image block; the local path is never copied into text.
    pub attachments: Vec<std::path::PathBuf>,
    /// Generic files selected from the `@` picker. These use `[文件 N]` chips and
    /// become typed file blocks with explicit bounded-text fallback permission.
    /// Keeping them separate from images preserves their delivery contract.
    pub file_attachments: Vec<std::path::PathBuf>,
    /// Large-paste text stash for the turn being composed. A bracketed paste over
    /// the internal line/character thresholds is collapsed to a single
    /// `[粘贴 N 行]` chip in `input` and its full text parked here, so a bulky
    /// paste doesn't flood the box into unscrollable noise. On submit the chip is
    /// expanded back to the full text inline by the internal attachment expander.
    /// Cleared with the input. Same proven pattern as `attachments`, for text.
    pub text_stash: Vec<String>,
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
    /// `@`-token is typed, then built once by the internal mention-file indexer and
    /// reused (the filesystem scan is NOT re-run per keystroke). Interior-mutable
    /// so the pure `&App` renderer can populate it on first use.
    pub mention_files: std::cell::RefCell<Option<Vec<String>>>,

    /// Bounded scrolling chat history (older lines roll off). This is the
    /// VISIBLE rendered surface (prose + tool rows + diff cards + plan memos +
    /// notes) — distinct from the base-facing [`Self::conversation`] /
    /// [`Self::full_transcript`]. Wave 3: persisted as
    /// the persisted display transcript on the same cadence as the transcript and
    /// REBUILT by the internal chat loader on reopen, so a relaunch shows the same
    /// screen instead of an empty conversation.
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
    /// Previous frame's `MAX_RENDER_ROWS` front-trim amount (rows split off the
    /// front of the retained scrollback), published by the renderer. The stored
    /// selection / search-match rows index that trimmed window, so when this
    /// frame trims a DIFFERENT amount (a marathon 8000+ row session that keeps
    /// growing) the highlight must be re-based by the delta — else it paints a
    /// row off until the next mouse event re-syncs it. `0` until the first render.
    pub transcript_cut: std::cell::Cell<usize>,
    /// Previous frame's total folded VISUAL-row count (post-`MAX_RENDER_ROWS`
    /// trim), published by the renderer. Read next frame to detect a transcript
    /// **shrink** (a fold/collapse toggle, `/compact`, `/clear`, or the live
    /// activity indicator removed at settle) — rows below the new end were
    /// vacated, exactly where a diff-only console can leave orphaned stale rows,
    /// so the renderer requests a full repaint via
    /// [`Self::request_transcript_repaint`]. `0` until the first render (a first
    /// frame never counts as a shrink). See
    /// the crate's internal transcript-reflow detector.
    pub transcript_prev_total: std::cell::Cell<usize>,
    /// P3 — the **terminal-contamination flag**: a one-shot request, raised from
    /// anywhere (interior-mutable `Cell`, so the pure `&App` renderer, the scroll
    /// helpers, and the `&mut` handlers can all set it), for ONE full clear +
    /// redraw on the next frame. Set after any OUT-OF-BAND terminal write (the
    /// completion BEL, a terminal-mode reassert, an OSC 52 clipboard copy) and at
    /// the discrete transitions where ratatui's prev-vs-next diff can leave
    /// stale rows (a transcript reflow / re-base / scroll jump, a height-changing
    /// history recall or `/clear`, focus gain, a resize, a live turn settling).
    /// Drained by the event loop via [`Self::take_terminal_contaminated`] into
    /// its `force_full_repaint` gate: exactly one `terminal.clear()` + full
    /// repaint, then the flag resets. This is the PRIMARY heal on a terminal
    /// WITHOUT confirmed synchronized output (an every-frame clear would flicker
    /// there); under confirmed sync output every frame is already a full atomic
    /// repaint (P0) and the flag only guarantees the healing frame is drawn.
    /// Defaults `false`.
    pub terminal_contaminated: std::cell::Cell<bool>,
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

    /// Whether the one-time "how to copy" hint has been surfaced this session.
    /// The in-app drag-select layer only covers the TRANSCRIPT, so a drag in the
    /// input box (or any non-transcript region) selects nothing while mouse
    /// capture is on — which reads as "copy is broken". The first such drag
    /// surfaces [`Self::hint_native_copy_once`] (Shift+drag / `/mouse`), then this
    /// latches so the tip never nags again.
    native_copy_hint_shown: bool,

    /// **In-app text selection** over the transcript (the Claude-Code layer).
    /// `Some` while the user has a live / just-copied drag selection; `None`
    /// when there's nothing selected. Coordinates are `(content_row, col)` into
    /// [`Self::transcript_rows`]. Set on mouse-down, extended on drag, kept on
    /// mouse-up (so the copied span stays highlighted) and cleared on the next
    /// down outside the selection. See [`crate::selection`].
    pub selection: Option<crate::selection::Selection>,

    /// `true` while a left-button drag-selection is IN PROGRESS — set on a
    /// mouse-down that landed inside the transcript, cleared on mouse-up. The
    /// selection stays `Some` (highlighted) after the button releases, so this
    /// flag is the only thing that distinguishes "still dragging" from "drag
    /// finished, span just highlighted". A wheel notch consults it to decide
    /// whether to EXTEND the selection past the viewport (mid-drag) or merely
    /// scroll (drag over / no drag). See [`Self::mouse_wheel_select`].
    pub selection_dragging: bool,

    /// The last screen `(col, row)` reported during an active drag, set on the
    /// mouse-down and on every drag move. When a wheel notch arrives mid-drag
    /// the transcript scrolls and the SAME screen cell now sits over a different
    /// content row, so re-resolving the selection cursor at this position grows
    /// the span to include the freshly revealed rows (the reported "滚轮复制
    /// 更多" gap). `None` outside a drag.
    pub last_drag_mouse: Option<(u16, u16)>,

    /// `true` while a **Ctrl+click (open-link) gesture** is in flight — set on
    /// the Ctrl+Left-down that routed to [`Self::link_click_open`], cleared on
    /// the matching Left-up. While armed, the event loop suppresses the
    /// drag-selection layer for the rest of the gesture (no selection extend,
    /// no copy hint, and crucially no mouse-up re-copy of a stale highlighted
    /// span). A plain click (no ctrl) never sets this, so selection behavior
    /// is byte-for-byte unchanged.
    pub link_click_pending: bool,

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

    /// **Per-row soft-wrap flag** — `transcript_row_wraps[i] == true` marks cached
    /// row `i` as a soft-wrap CONTINUATION of row `i-1` (the renderer folded ONE
    /// logical line across both). A drag-copy reads this (via
    /// [`crate::selection::extract_wrapped`]) to rejoin a wrapped paragraph into a
    /// single line — no mid-line breaks at the fold points — while keeping the
    /// newline at every real logical break. In lockstep with `transcript_rows`; a
    /// length skew fails open (a missing flag ⇒ a real line break). `RefCell` for
    /// the same borrow reason as the rows.
    pub transcript_row_wraps: std::cell::RefCell<Vec<bool>>,

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

    /// **In-app text selection over the INPUT COMPOSER box** — a SEPARATE layer
    /// from the transcript-scoped [`Self::selection`] so the two highlights never
    /// collide (a down in either region clears the other). `Some` while a drag
    /// inside the input box is live or just-copied. Coordinates are `(visual_row,
    /// char_col)` into [`Self::input_rows`]; extraction resolves each endpoint to
    /// an absolute char index in [`Self::input`] (via `ui::offset_at_wrapped`) and
    /// slices, so a soft-wrapped line copies without spurious newlines and a hard
    /// `Ctrl+J` newline is preserved. This is what makes drag-select+copy work
    /// INSIDE the input box (Claude Code parity) without toggling `/mouse`.
    pub input_selection: Option<crate::selection::Selection>,

    /// `true` while a left-button drag-selection inside the INPUT box is in
    /// progress — set on a mouse-down that landed inside the published input rect,
    /// cleared on mouse-up (or when the input text changes under a keystroke, which
    /// invalidates the cached row coordinates). Distinguishes an input-box drag
    /// from a transcript drag in the event loop.
    pub input_selection_dragging: bool,

    /// The **input-box text rectangle** `(left, top, width, height)` published by
    /// `ui::render_prompt` every frame — the wrapped text rows ONLY (the underline
    /// border row is excluded), so a click on the border never maps onto text. The
    /// event loop maps a mouse `(col, row)` against this to decide inside/outside
    /// the input box. `(0,0,0,0)` until the first render.
    pub input_area: std::cell::Cell<(u16, u16, u16, u16)>,

    /// The **terminal caret cell** `(x, y)` the input box wants this frame, published
    /// by `ui::render_prompt`; `None` when this frame owns no caret (an overlay /
    /// `/help` is up, or the terminal is too small and the "make the window bigger"
    /// card bailed before layout).
    ///
    /// The caret is deliberately NOT set on the ratatui `Frame`. `Terminal::try_draw`
    /// would then emit `Show` **before** `MoveTo` — each an `execute!`, i.e. its own
    /// flush — so the caret becomes visible at the end of the last painted cell run
    /// one whole write-gap before it is moved back to the input box. A terminal that
    /// repaints on its own timer (Windows conhost) renders that window, and the caret
    /// visibly jumps. Leaving the frame caret `None` makes ratatui take its
    /// `hide_cursor()` arm instead, and `ui::place_caret` re-asserts the caret in the
    /// correct order (`MoveTo` **then** `Show`) once painting is done. `(0,0)` is a
    /// legal caret cell, hence `Option` rather than a sentinel.
    pub caret: std::cell::Cell<Option<(u16, u16)>>,

    /// **Per-frame cache of the wrapped input rows** (the logical text, no leading
    /// mode-prefix gutter) — one `String` per visual row, in render order, the
    /// same `wrap_input_rows` fold the box paints. The input-box selection maps a
    /// screen column onto these and the highlight repaints them. `RefCell` because
    /// the renderer borrows `&App`.
    pub input_rows: std::cell::RefCell<Vec<String>>,

    /// The **uniform leading-gutter width** (display columns) stripped from every
    /// cached input row — the `>_ ` / `[run] ` / `[gate] ` mode prefix. Row 0 and
    /// the continuation-indent rows share the same width (see `mode_prefix_width`),
    /// so one value covers all rows: the mouse mapping subtracts it and the
    /// highlight adds it back. Published every frame.
    pub input_gutter: std::cell::Cell<usize>,

    /// The vertical **scroll offset** the input box applies once it grows past
    /// `INPUT_MAX_ROWS` — the index of the first visible wrapped row. Combined with
    /// [`Self::input_area`] this turns a screen row into an absolute input visual
    /// row (the analogue of [`Self::transcript_first_visible`]). Published every
    /// frame.
    pub input_scroll: std::cell::Cell<usize>,

    /// **Conversation memory** — the multi-turn transcript handed to the base
    /// on every routed turn so chat is a real conversation, not a sequence of
    /// amnesiac one-shots. Holds ONLY genuine chat turns (user message + base
    /// reply), never pipeline progress noise, so the base sees a clean dialogue
    /// when it decides "chat vs. run" and when it answers conversationally.
    ///
    /// This is the **working view**: token-budgeted auto-compaction folds the
    /// older turns into one structured summary block here (the base then sees
    /// `[summary] + [recent verbatim tail]`), while the FULL transcript lives
    /// untouched in [`Self::full_transcript`] (persisted to disk). Bounded by
    /// bounded compaction trigger with a hard FIFO safety net.
    pub conversation: Vec<umadev_runtime::Message>,

    /// **Full, append-only transcript** — every recorded chat turn (user + base
    /// reply) in send order, NEVER folded or FIFO-dropped. This is the durable
    /// record persisted to `.umadev/chat/<id>.json`; compaction mutates only the
    /// [`Self::conversation`] working view, so the on-disk history is preserved
    /// in full and a `/resume` reopens the complete conversation.
    pub full_transcript: Vec<umadev_runtime::Message>,

    /// Circuit breaker for the auto-compaction summary call: after
    /// [`umadev_agent::compaction::Breaker::LIMIT`] consecutive summary failures
    /// it trips and auto-compaction stops being attempted (the deterministic FIFO
    /// floor takes over) until a later success resets it. Bounds wasted base calls
    /// when the base is down.
    pub(crate) compaction_breaker: umadev_agent::compaction::Breaker,

    /// `true` while a summary `complete()` is in flight, so a second compaction is
    /// never spawned concurrently (one folder at a time; the trigger is re-checked
    /// after each turn settles).
    pub(crate) compaction_in_flight: bool,

    /// Monotonic generation of [`Self::conversation`], bumped whenever the
    /// conversation identity changes (`/clear`, `/resume`, a fresh load). An
    /// in-flight compaction job carries the generation it started under; a result
    /// that arrives after a `/clear` or `/resume` (stale generation) is dropped so
    /// it can never splice a summary into the wrong conversation.
    pub(crate) conversation_generation: u64,

    /// `true` once a host-CLI base has handled at least one chat turn in the
    /// current session. Tells the next routed turn to **resume** that base's
    /// own conversation (`claude --continue` etc.) instead of starting cold —
    /// this is what gives chat real memory for `HostCli` bases. Reset when the
    /// session context breaks: `/clear`, switching backend, or a new
    /// pipeline run. Ignored by `Offline` and bases without a session id.
    pub host_chat_session_active: bool,

    /// The exact native session id returned by the base after a successful turn,
    /// or restored through an explicit `/resume <chat-id>`. UmaDev never derives
    /// this from the logical chat-file id and never synthesizes a Codex/OpenCode
    /// id. Reset (to `None`) with [`Self::host_chat_session_active`].
    pub chat_session_id: Option<String>,

    /// Immutable authority identity for [`Self::chat_session_id`]. Grok ids are
    /// resumable only when this contains a fully attested effective sandbox and
    /// the native resume preflight is active; legacy/unknown identities fall back
    /// to a fresh session plus transcript handoff.
    pub chat_resume_identity: Option<BaseResumeIdentity>,

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

    /// `true` once a director build has handed its finished native session back to
    /// chat (Wave 5 deliverable 2). The exact base session id is captured into
    /// [`Self::chat_session_id`]; the NEXT chat turn resumes that id so "why did you
    /// build it that way?" continues the SAME conversation, not whichever session
    /// happens to be newest in the directory. Consumed once that turn fires.
    /// Fail-open: a base without resume support gets the bounded transcript replay.
    pub(crate) run_session_handed_to_chat: bool,

    /// `true` while any **director build** (explicit `/run` or a model-promoted
    /// natural-language turn) is in flight. Set when the director boundary is
    /// crossed and cleared on any terminal turn.
    ///
    /// It NO LONGER decides the Wave-5 session hand-back: the chat surface classifies
    /// chat-vs-build INSIDE the spawned task (after the slow brain-router consult), so
    /// the event loop can't know the class before dispatch and can't set this flag
    /// truthfully pre-spawn. The build-ness now rides the terminal
    /// [`crate::RouteDecision::AgenticDone`]'s `director_build` field and is what
    /// [`Self::record_agentic_done`] keys the hand-back on. This flag is retained only
    /// as the explicit-`/run` in-flight marker.
    pub(crate) director_run_in_flight: bool,
    /// A DIRECTOR build is parked at a spec-MUST confirmation gate (A1-GAP1):
    /// set by [`Self::record_run_paused_at_gate`] when the run's terminal
    /// `RunPausedAtGate` decision lands, cleared when the gate resolves (the
    /// resume spawn, a cancel, or a fresh run). The event loop keys the gate
    /// approval / revision routing on it — a director pause resumes via
    /// `drive_director_loop_resume`, never a legacy gate block.
    pub(crate) director_gate_paused: bool,
    /// A DIRECTOR build is parked because its wall-clock budget was exhausted while
    /// resumable steps remained (Stage 1/2): set by
    /// [`Self::record_run_paused_at_budget`] when the terminal `RunPausedAtBudget`
    /// decision lands, cleared when a fresh run / resume begins
    /// ([`Self::clear_stale_terminal_on_live_turn`]). It reads as a PAUSE (never
    /// aborted / degraded) so the status label + timer stop honestly and the hint
    /// points at `/continue`; there is NO gate to approve, so it does not set
    /// `active_gate` or `director_gate_paused`.
    pub(crate) budget_paused: bool,
    /// A Director `GateOpened` event that arrived before the Director session was
    /// fully ended. The gate is staged here and becomes interactive only when the
    /// matching terminal `RunPausedAtGate` decision lands, preventing approval or
    /// picker input from racing a still-live writer session.
    pub(crate) pending_director_gate: Option<(Gate, Option<GateChoice>)>,
    /// A read-only model answer to a question at an open gate is in flight. The
    /// gate itself stays armed; additional submitted text remains in the editor
    /// until the answer lands so two gate-query sessions can never race.
    pub(crate) gate_query_in_flight: bool,
    /// Monotonic generation source for gate queries. It is intentionally never
    /// reset when a run/conversation is cleared, so an old async result can never
    /// alias a later query in the same process.
    pub(crate) gate_query_epoch: u64,
    /// The sole gate query whose terminal event may currently update app state.
    pub(crate) active_gate_query_epoch: Option<u64>,
    /// Number of chat turns already waiting before the current route dispatch.
    /// If the route becomes Director, only the later tail may be promoted into
    /// that task's steering; the older FIFO backlog remains independent work.
    pub(crate) route_backlog_len: usize,

    /// Currently active backend id (matches `config.backend`).
    /// `None` means offline / no host CLI.
    pub backend: Option<String>,
    /// Display label for the worker — `claude-code` / `codex` / `offline`.
    pub backend_label: String,
    /// The active base's displayed model: static config at launch, replaced by
    /// the base's live session report when one arrives.
    pub(crate) base_model: Option<String>,
    /// Whether [`Self::base_model`] came from a live base session report rather
    /// than a static config read.
    pub(crate) base_model_live: bool,
    /// Exact context window read from the base config when available.
    pub(crate) base_context_window: Option<u64>,
    /// Complete model catalog published by the current live base session.
    pub(crate) base_session_models: Vec<SessionModelInfo>,
    /// Current model id published by the live session, distinct from the static
    /// base-config fallback held in [`Self::base_model`].
    pub(crate) base_session_model: Option<String>,
    /// Current native interaction mode reported by the live base session.
    pub(crate) base_session_mode: Option<SessionMode>,
    /// Current independent thinking toggle reported by the live base session.
    pub(crate) base_session_thinking: Option<bool>,
    pub(crate) base_session_thinking_can_enable: bool,
    pub(crate) base_session_thinking_can_disable: bool,
    /// Complete slash-command catalog published by the current live session.
    pub(crate) base_session_commands: Vec<SessionCommandInfo>,
    /// Complete tool-name snapshot attached to the command catalog.
    pub(crate) base_session_tools: Vec<String>,
    /// Complete native plan snapshot published by the current live base.
    pub(crate) base_session_plan: Vec<SessionPlanEntry>,

    /// Workspace slug (filled in by the caller).
    pub slug: String,
    /// The active requirement once the pipeline starts.
    pub requirement: String,

    /// Phase progress, in `PHASE_CHAIN` order.
    pub phases: Vec<PhaseRow>,
    /// The gate the pipeline is currently paused at, if any.
    pub active_gate: Option<Gate>,
    /// The structured choice surfaced by the active gate, rendered as a picker
    /// (a question + 2–4 labeled options). `None` → the gate is free-form only
    /// (fail-open). Free-text input stays available alongside the picker.
    pub gate_choice: Option<GateChoice>,
    /// The highlighted option index in [`Self::gate_choice`] (0-based). Reset to
    /// 0 each time a fresh choice is set; meaningless when `gate_choice` is `None`.
    pub gate_choice_sel: usize,
    /// A base action PAUSED awaiting the user's decision, as `(action, target)`
    /// (e.g. `("Bash", "npm install")`). Mirrored each event-loop iteration from
    /// the shared approval holder (lib.rs) via [`Self::set_pending_approval`], so
    /// the renderer pins a STICKY approval bar directly above the input box and
    /// the internal submit path can classify a typed 「批准」/「拒绝」 as the decision
    /// (A2#5 — the pause used to surface only as one scrolling Note, with every
    /// key silently consumed and no visible approval entry point). `None` = no
    /// pause (the common case).
    pub pending_approval: Option<(String, String)>,
    /// Same-RPC typed input state, including contract-specific picker progress.
    /// Secret input is never copied into chat history or durable transcripts.
    pub pending_host_input: Option<host_input::PendingHostInputView>,
    /// Explicit Grok Build pre-session authentication state. It is separate from
    /// the ordinary composer, so a draft typed while the base was opening is
    /// preserved byte-for-byte and no trust/Auto mode can answer on the user's
    /// behalf. Sensitive fields have redacted `Debug` output and are never
    /// persisted.
    pub(crate) auth_ui: Option<crate::auth_ui::AuthUiState>,
    /// Ordinary chat draft displaced by [`Self::pending_host_input`]. Restored
    /// after the same-RPC response settles; never rendered or submitted while the
    /// host question owns the composer.
    host_input_draft: Option<host_input::HostInputDraft>,
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
    /// `true` when the LAST block ended TERMINALLY short of delivery because the
    /// base **degraded** to placeholder templates or failed a hard gate (base
    /// offline → `[WARN][降级]`, no real code produced, or a blocked quality gate).
    /// Unlike [`Self::aborted`] (a block that errored before producing any phase),
    /// a degraded block returns `Ok` with a non-`Delivery` `final_phase`, so it
    /// never trips `ABORT_SENTINEL`. Distinct from [`Self::finished`] so the status
    /// bar reads a terminal "降级/未完成 — /redo" state (timer STOPPED) instead of a
    /// misleading `[ok] delivered` or a forever-ticking `[run] 运行中`. Cleared when
    /// a new run / working turn starts.
    pub degraded: bool,
    /// Sticky within one run: set the moment a degraded/failed SIGNAL streams past
    /// (`[WARN][降级]` placeholder banner, the `UD-EVID-003` blocked-quality-gate
    /// code, or the "no real code produced" failure) so a terminal
    /// [`EngineEvent::BlockCompleted`] short of delivery can tell a genuine
    /// degrade/failure apart from a CLEAN short-circuit (a docs-only task, a light
    /// small-change pipeline, a clean single-phase `/redo`). Reset at each new run /
    /// working turn. Fail-open: an unseen signal reads the exit as a clean stop.
    pub run_degraded_seen: bool,

    /// Background-run **task registry**: the active mutating run (if any) plus a
    /// short history of recent finished/stopped ones. A `/run` registers a
    /// [`TaskStatus::Running`] task here so it reads as a manageable background
    /// task (listed / stopped / resumed via `/tasks`) instead of a modal
    /// lock-out. Single-writer (the run-lock) keeps at most one `Running` task;
    /// the rest are settled rows, capped to the internal task-history limit. Newest is last.
    pub tasks: Vec<BackgroundTask>,
    /// Monotonic counter minting the short display id (`t1`, `t2`, …) for each
    /// registered task. Never reset within a session.
    pub task_seq: u64,

    /// **Feature A — completion notification.** When `false`, the terminal bell
    /// is silenced (env `UMADEV_BELL=0` / `false` / `off` / `no`). Default `true`.
    /// Read once at construction so a test can flip the field directly without
    /// racing process env.
    pub bell_enabled: bool,
    /// A completion bell is armed and waiting to ride the next between-frames
    /// gap. Set by the internal completion-bell armer on a terminal transition (a run
    /// finished / aborted, a long agentic turn settled, or a gate paused needing
    /// the user) that took long enough that the user may have stepped away;
    /// drained + emitted by the event loop via [`Self::take_bell`] through the
    /// render's single backend writer (never mid-frame). Fail-open.
    pub bell_pending: bool,
    /// Monotonic count of bells armed this session — a cheap assertion handle for
    /// tests (a quick turn leaves it at `0`; a long one bumps it). Never reset.
    pub bell_count: usize,

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
    /// toggle; `trust_mode_override` is the richer three-tier control that
    /// supersedes it. The two stay consistent — flipping one updates the other.
    pub auto_approve_override: Option<bool>,

    /// Session-level trust / autonomy tier override (`/mode plan|guarded|auto`).
    /// `None` → derive from `.umadevrc` (`auto_approve_gates`). When `Some`, it
    /// takes precedence and also drives the legacy `auto_approve_override`.
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
    /// Renderer-published maximum reachable help scroll row. Key handling clamps
    /// against this so holding ↓/PgDn at the bottom cannot overshoot and make the
    /// next ↑ appear stuck.
    pub help_max_scroll: std::cell::Cell<u16>,
    /// A scrollable overlay (from `/spec` / `/verify` / `/doctor` /
    /// `/diff`). When `Some`, key input is routed to the overlay
    /// (scroll, close); when `None`, normal chat input.
    pub overlay: Option<Overlay>,
    /// **Feature B — in-transcript search.** `Some` while the Ctrl+F search bar
    /// is open. Its own modal mode: the chat key handler routes EVERY keystroke
    /// to the internal search-key handler while this is `Some`, so search never collides
    /// with the slash palette, the `@`-mention popover, history recall, or an
    /// overlay (each of those is checked/skipped while search owns the input).
    pub search: Option<SearchState>,
    /// **I3 — reverse prompt-history search.** `Some` while the Ctrl+R history
    /// search owns the input. Its own modal mode: the chat key handler routes
    /// EVERY keystroke to the internal history-search handler while this is `Some`, so it
    /// never collides with the transcript search, the slash palette, the
    /// `@`-mention popover, or `↑↓` history recall (each is skipped while it owns
    /// the keyboard). `None` when closed.
    pub history_search: Option<HistorySearchState>,
    /// Handle to a running dev-server subprocess spawned by `/preview`, so we
    /// can kill it on `/stop-preview` or quit. `None` when no preview is live.
    pub preview_server: std::sync::Arc<std::sync::Mutex<Option<tokio::process::Child>>>,
    /// Workspace root — surfaced in the status bar as a breadcrumb.
    pub project_root: std::path::PathBuf,
    /// When a pipeline is running and the user presses `q` / Esc, we
    /// stash a "press again to confirm" flag instead of quitting
    /// immediately. Cleared on any other keypress.
    pub pending_quit_confirm: bool,

    /// First-Esc-to-arm, second-Esc-to-fire for the **rewind** gesture: on an
    /// idle, EMPTY input with a prior user turn, a double-Esc re-loads the last
    /// user message into the box for editing and drops the turns after it (so a
    /// resend re-asks from that point). Set on the first Esc; the second Esc
    /// rewinds. Cleared on any typing. Quitting moved to `/quit`, which freed
    /// the idle double-Esc for this. `false` = not armed.
    pub pending_rewind: bool,

    /// First-Esc-to-arm, second-Esc-to-interrupt while a run is in flight, so a
    /// stray keypress can't nuke a long build. Set on the first Esc; a second Esc
    /// within a short window actually cancels. `None` = not armed.
    pub interrupt_armed_at: Option<std::time::Instant>,

    /// Kill-ring (I1): text removed by Ctrl+U / Ctrl+K / Ctrl+W is PUSHED here
    /// (most-recent at the front) instead of being destroyed, so Ctrl+Y can yank
    /// it back and Alt+Y can cycle older entries. Capped at the internal ring limit.
    pub kill_ring: VecDeque<String>,
    /// Direction of the LAST kill, or `None` when the last action was not a kill.
    /// Drives readline-style coalescing: a consecutive same-direction kill folds
    /// into the front ring entry instead of pushing a new one. Reset by any
    /// non-kill key.
    last_kill: Option<KillDir>,
    /// The span the most recent yank / yank-pop inserted, as `(char_start,
    /// char_len)`. `Some` ONLY immediately after a yank — Alt+Y (yank-pop) is
    /// valid only then and replaces this span with the next ring entry. Any other
    /// key clears it.
    yank_span: Option<(usize, usize)>,
    /// Rotation cursor into [`Self::kill_ring`] for Alt+Y yank-pop cycling.
    yank_ring_idx: usize,
    /// Undo stack (I2): snapshots of the input taken just before edits (most
    /// recent on top). Ctrl+Z pops one to restore. Bounded by [`UNDO_CAP`].
    undo_stack: Vec<EditSnapshot>,
    /// Redo stack: states popped by undo, replayable by Alt+Z. Truncated on any
    /// fresh edit so a new edit forks a clean future.
    redo_stack: Vec<EditSnapshot>,
    /// When the last undo snapshot was pushed, for [`UNDO_COALESCE`] debouncing.
    /// `None` forces the next edit to snapshot (set after undo / redo so the next
    /// edit always opens a clean step).
    last_snapshot_at: Option<std::time::Instant>,

    /// Typed exact/lower-bound/unknown usage state for this conversation.
    pub(crate) session_usage: SessionUsageMeter,

    /// Whether the proactive `/compact` nudge has already fired for the current
    /// threshold crossing, so the one-line hint surfaces ONCE when context first
    /// crosses [`CONTEXT_NUDGE_PCT`] — not every turn. Re-armed (set `false`) when
    /// occupancy drops back below the threshold (e.g. after a `/compact`) or on
    /// `/clear`. Deterministic, bounded: at most one nudge per crossing.
    pub(crate) context_nudge_shown: bool,

    /// One-line status shown in the top bar.
    pub status: String,
    pub(crate) copy_toast: Option<crate::selection::CopyToast>,
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
    /// Process-log visibility (`/logs`): when `true`, a long-running command row
    /// (a Maven / Gradle build, `spring-boot:run`, a dependency install) keeps its
    /// FULL captured output and stays EXPANDED in the transcript so the user sees
    /// the build progressing — instead of a 200-char clip that auto-collapses to a
    /// checkmark. The renderer reads THIS field (not the shared flag) so the
    /// behaviour is deterministic + testable; the matching thread-safe shared flag
    /// (`umadev_host::process_logs`) is what the out-of-process base drivers read.
    /// Seeded at construction from the saved preference / an external env override,
    /// flipped live by `/logs`. Default `false`.
    pub show_process_logs: bool,
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

    /// Explicit adjustments to the CURRENT task typed while a pipeline is
    /// mid-phase. They wait FIFO for the next step/gate boundary. Questions,
    /// later tasks and ambiguity belong to [`Self::queued_chat`] instead.
    pub queued_steer: VecDeque<String>,

    /// Set by the gate handler when a `queued_steer` message is ready to fire
    /// at the just-opened gate. The event loop consumes it and re-runs the
    /// producing block with the queued text folded in as a revision.
    pub pending_steer: Option<String>,

    /// Ordinary model turns waiting behind an active writer/route: questions,
    /// later tasks, ambiguous input, or any turn typed while a chat response is
    /// in flight. They stay FIFO and run only after the active turn/run settles,
    /// so one base session is never resumed concurrently. Kept distinct from
    /// [`Self::queued_steer`], which may alter the CURRENT pipeline at a boundary.
    pub queued_chat: std::collections::VecDeque<String>,

    /// Dispatch kind positionally aligned with [`Self::queued_chat`]. Legacy
    /// hand-built state without tags is normalized to routed chat before use.
    queued_dispatch_kinds: VecDeque<QueuedResidentKind>,

    /// Published by the event loop immediately before key handling. Tests and
    /// pre-classification turns default false, preserving serial queue semantics;
    /// a live resident endpoint enables `Action::LiveInput`.
    pub(crate) live_input_ready: bool,

    /// Grok Build's native prompt-queue pane/editor. This is a mirror of base
    /// snapshots, never a second client-owned queue.
    pub(crate) prompt_queue: PromptQueueUi,

    /// Structured snapshots for every queued chat turn, positionally aligned
    /// with [`Self::queued_chat`]. Keeping text-only snapshots too is essential:
    /// two queued turns may have the same display text while only one carries an
    /// attachment, so matching an attachment snapshot by text can deliver it to
    /// the wrong turn.
    queued_turn_inputs: VecDeque<SubmittedTurn>,

    /// Structured snapshot belonging to the next `Action::Route(String)`. The
    /// action payload stays source-compatible for the large command/test surface;
    /// the event loop takes this immediately before spawning the route task.
    pending_route_input: Option<SubmittedTurn>,

    /// The EXACT text of the chat turn most recently dispatched to the base (a fresh
    /// `Action::Route`, or a drained queued turn). On an ordinary non-Director route
    /// failure this is the authoritative "what just failed" key —
    /// the internal failed-route deduplicator drops queued turns equal to
    /// THIS (not the fragile "last user turn in conversation" heuristic, which a
    /// relayed / reframed turn or an intervening record could skew). A Director
    /// terminal outcome clears it because `/run` has no corresponding chat dispatch
    /// and a model promotion makes this key stale. `None` until the first dispatch.
    pub last_dispatched_chat: Option<String>,

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

    /// **R1 — settled-message render cache.** Holds the fully folded
    /// `Vec<Line>` (markdown-parsed + width-folded) for each SETTLED (non-live,
    /// non-animating) transcript message, keyed on its content + render context
    /// (width / theme / lang / collapse). A settled message reuses its cached
    /// rows verbatim instead of re-parsing pulldown-cmark and re-folding every
    /// frame — the per-frame transcript cost drops from "re-parse + re-fold ALL
    /// history" to "clone the cached rows". The cache whole-invalidates on a
    /// width or theme change, per-entry-invalidates on a content change (the key
    /// carries a content hash), and self-bounds by dropping any entry not touched
    /// in the current frame. Fully fail-open: a borrow conflict or a miss simply
    /// re-folds, byte-for-byte identical to the uncached path. See
    /// [`crate::ui::MsgFoldCache`].
    pub(crate) msg_fold_cache: std::cell::RefCell<crate::ui::MsgFoldCache>,

    /// **R7 — whole-transcript assembly cache.** One level above
    /// [`Self::msg_fold_cache`]: the fully ASSEMBLED folded rows of the settled
    /// message prefix (welcome banner + gaps + messages, in order) plus the
    /// derived selection-layer text, validated by a single signature over every
    /// render input. A frame whose signature matches — every scroll frame on a
    /// settled chat — skips the per-message walk, the cache-hit row clones, and
    /// the O(total) selection-text re-publish; the paint then materializes only
    /// the visible window, so a wheel tick costs O(viewport) instead of
    /// O(history) (the reported VS Code scroll lag). Fully fail-open: a borrow
    /// conflict rebuilds fresh, byte-for-byte identical to the uncached path.
    /// See [`crate::ui::TranscriptCache`].
    pub(crate) transcript_cache: std::cell::RefCell<crate::ui::TranscriptCache>,

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
    /// `true` when the checklist was rehydrated from disk after a **transient**
    /// abort — a FROZEN, interrupted view of the saved plan, not a live run. The
    /// renderer keys off this to draw a static "interrupted — /continue to resume"
    /// header instead of the live "· running" spinner. Set only by
    /// `rehydrate_frozen_plan_if_transient`; cleared by every reset path and by a
    /// fresh plan post, so it never bleeds into a new build. Default `false`.
    pub plan_frozen: bool,

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

    /// **Handoff timeline** (Wave C) — one entry per plan step that flipped to
    /// `done`, in completion order (a seat handing its finished deliverable
    /// downstream). Recorded from real DONE transitions (anti-theater: never a
    /// narration), bounded to the most recent handoff-history limit. Surfaced by
    /// `/team`; cleared with the rest of the live-run panel state.
    pub handoffs: Vec<Handoff>,

    /// The last routed intent (Wave 1 deliverable 1) — the class id the router
    /// decided for the in-flight turn (`chat` / `build` / …). Drives the status
    /// chip so the user sees fast-vs-deliberate at a glance. `None` until the
    /// first route. Set deterministically (Tier-0) the instant a turn is
    /// submitted, then refined by the async Tier-1 consult.
    pub last_intent_class: Option<String>,

    /// I9 — how many prompts the user has submitted *this session* (any submit:
    /// chat / slash / bang). `0` until the first interaction, which is exactly
    /// the "first-run" window in which the rotating example tip
    /// (through the internal first-run tip renderer) is offered above the idle placeholder.
    /// Incremented in [`Self::remember_submission`]; NOT persisted (a fresh
    /// session re-offers the tip, with a rotated example).
    pub session_turns: usize,

    /// I9 — cached resolution of the repo file named by the first-run example
    /// tip (the most recently modified source file, or `None`). Interior-mutable
    /// so the pure `&App` renderer can populate it on first use; the bounded FS
    /// walk then runs at most once per session. Outer `None` = not yet computed.
    pub example_file: std::cell::RefCell<Option<Option<String>>>,
}

fn memory_state_label(lang: umadev_i18n::Lang, value: Option<bool>) -> &'static str {
    match value {
        Some(true) => umadev_i18n::t(lang, "memory.state.on"),
        Some(false) => umadev_i18n::t(lang, "memory.state.off"),
        None => "—",
    }
}

fn memory_retention_label(
    lang: umadev_i18n::Lang,
    entry: &umadev_agent::memory_control::MemoryInventoryEntry,
) -> String {
    use umadev_agent::memory_control::RetentionEnforcement;
    match entry.retention_enforcement {
        RetentionEnforcement::Fixed => umadev_i18n::t(lang, "memory.retention.fixed").to_string(),
        RetentionEnforcement::PolicyOnly => entry.retention_days.map_or_else(
            || umadev_i18n::t(lang, "memory.retention.not_configured").to_string(),
            |days| umadev_i18n::tf(lang, "memory.retention.configured", &[&days.to_string()]),
        ),
        RetentionEnforcement::Unsupported => {
            umadev_i18n::t(lang, "memory.retention.unsupported").to_string()
        }
    }
}

fn format_memory_inventory(
    lang: umadev_i18n::Lang,
    inventory: &umadev_agent::memory_control::MemoryInventory,
    scope: umadev_agent::memory_control::MemoryScope,
    retention_only: bool,
    store: Option<umadev_agent::memory_control::MemoryStore>,
) -> String {
    let mut out = umadev_i18n::tf(lang, "memory.inventory.title", &[scope.id()]);
    if inventory.policy_error.is_some() {
        out.push_str("\n[warn] ");
        out.push_str(umadev_i18n::t(lang, "memory.policy_unavailable"));
    }
    let entries: Vec<_> = inventory
        .entries
        .iter()
        .filter(|entry| store.is_none_or(|selected| entry.store == selected))
        .collect();
    if entries.is_empty() {
        out.push('\n');
        out.push_str(umadev_i18n::t(lang, "memory.inventory.empty"));
        return out;
    }
    for entry in entries {
        let retention = memory_retention_label(lang, entry);
        out.push('\n');
        if retention_only {
            out.push_str(&umadev_i18n::tf(
                lang,
                "memory.retention.entry",
                &[entry.store.id(), &retention],
            ));
            continue;
        }
        out.push_str(&umadev_i18n::tf(
            lang,
            "memory.inventory.entry",
            &[
                entry.store.id(),
                &entry.files.to_string(),
                &entry.bytes.to_string(),
                memory_state_label(lang, entry.capture),
                memory_state_label(lang, entry.recall),
                &retention,
            ],
        ));
        if !entry.locations.is_empty() {
            out.push('\n');
            out.push_str(&umadev_i18n::tf(
                lang,
                "memory.inventory.paths",
                &[&entry.locations.join(", ")],
            ));
        }
    }
    out
}

fn format_memory_parse_error(lang: umadev_i18n::Lang, error: &MemoryParseError) -> String {
    match error {
        MemoryParseError::Usage => umadev_i18n::t(lang, "memory.error.usage").to_string(),
        MemoryParseError::UnclosedQuote => {
            umadev_i18n::t(lang, "memory.error.unclosed_quote").to_string()
        }
        MemoryParseError::InvalidArgument(value) => {
            umadev_i18n::tf(lang, "memory.error.invalid_argument", &[value])
        }
        MemoryParseError::MissingScope => {
            umadev_i18n::t(lang, "memory.error.missing_scope").to_string()
        }
        MemoryParseError::OneScopeRequired => {
            umadev_i18n::t(lang, "memory.error.one_scope").to_string()
        }
        MemoryParseError::ProjectScopeRequired => {
            umadev_i18n::t(lang, "memory.error.project_scope").to_string()
        }
        MemoryParseError::MissingStore => {
            umadev_i18n::t(lang, "memory.error.missing_store").to_string()
        }
        MemoryParseError::ExactStoreRequired => {
            umadev_i18n::t(lang, "memory.error.exact_store").to_string()
        }
        MemoryParseError::UnknownSelector(value) => {
            umadev_i18n::tf(lang, "memory.error.unknown_selector", &[value])
        }
        MemoryParseError::MissingOutput => {
            umadev_i18n::t(lang, "memory.error.missing_output").to_string()
        }
        MemoryParseError::AbsoluteOutputRequired => {
            umadev_i18n::t(lang, "memory.error.absolute_output").to_string()
        }
        MemoryParseError::InvalidDays => {
            umadev_i18n::t(lang, "memory.error.invalid_days").to_string()
        }
    }
}

fn memory_store_summary(
    lang: umadev_i18n::Lang,
    stores: &[umadev_agent::memory_control::MemoryStore],
) -> String {
    const DISPLAY_CAP: usize = 8;
    let mut summary = stores
        .iter()
        .take(DISPLAY_CAP)
        .map(|store| store.id())
        .collect::<Vec<_>>()
        .join(",");
    if stores.len() > DISPLAY_CAP {
        summary.push_str(&umadev_i18n::tf(
            lang,
            "memory.stores.more",
            &[&(stores.len() - DISPLAY_CAP).to_string()],
        ));
    }
    summary
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
        // A config value is allowed into live TUI state only when it is one of the
        // four product-supported bases. `offline` remains an explicit internal
        // fallback; retired/unknown ids reopen setup instead of silently driving a
        // transport that is not exposed by the product.
        let explicit_offline = config.backend.as_deref() == Some("offline");
        let backend = config
            .backend
            .clone()
            .filter(|b| crate::FIRST_CLASS_BACKEND_IDS.contains(&b.as_str()));
        let backend_label = backend.clone().unwrap_or_else(|| "offline".to_string());
        let base_model = backend
            .as_deref()
            .and_then(|b| crate::detect_base_model(b, &project_root));
        let base_context_window = backend
            .as_deref()
            .and_then(|b| crate::detect_base_context_window(b, &project_root));
        let lang = config.resolved_lang();
        umadev_i18n::set_lang(lang);
        // Publish the saved process-log preference (`/logs`) into the base drivers'
        // thread-safe shared flag, so a build's long-running command output is
        // surfaced from the first turn. Off by default; an external env override
        // (seeded into the flag at launch) wins.
        config.apply_process_logs();
        // Publish the approval-question presentation preference (`/questions`) into
        // the agent crate's shared flag, so the base `AskUserQuestion` notes honor
        // it from the first turn. Default picker; opt into text via config/`/questions`.
        config.apply_question_form();
        // Publish the project's Codex launch-sandbox choice (`.umadevrc`
        // `[codex] sandbox_mode`) into the codex driver's thread-safe shared
        // override so the driver honors it, mirroring the model-tier publish above.
        // Main-worker default is `danger-full-access`; an explicit project or
        // launch override can restrict it. Plan mode is forced read-only by the
        // session driver regardless of this execution default.
        let codex_sandbox = resolve_and_publish_codex_sandbox(&project_root);
        let mode = if backend.is_some() || explicit_offline {
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
            picker_login_confirm: None,
            key_arrived_in_burst: false,
            input: String::new(),
            input_cursor: 0,
            attachments: Vec::new(),
            file_attachments: Vec::new(),
            text_stash: Vec::new(),
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
            transcript_cut: std::cell::Cell::new(0),
            transcript_prev_total: std::cell::Cell::new(0),
            terminal_contaminated: std::cell::Cell::new(false),
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
            native_copy_hint_shown: false,
            selection: None,
            selection_dragging: false,
            last_drag_mouse: None,
            link_click_pending: false,
            transcript_rows: std::cell::RefCell::new(Vec::new()),
            transcript_gutters: std::cell::RefCell::new(Vec::new()),
            transcript_row_wraps: std::cell::RefCell::new(Vec::new()),
            transcript_area: std::cell::Cell::new((0, 0, 0, 0)),
            transcript_first_visible: std::cell::Cell::new(0),
            input_selection: None,
            input_selection_dragging: false,
            input_area: std::cell::Cell::new((0, 0, 0, 0)),
            caret: std::cell::Cell::new(None),
            input_rows: std::cell::RefCell::new(Vec::new()),
            input_gutter: std::cell::Cell::new(0),
            input_scroll: std::cell::Cell::new(0),
            conversation: Vec::new(),
            full_transcript: Vec::new(),
            compaction_breaker: umadev_agent::compaction::Breaker::new(),
            compaction_in_flight: false,
            conversation_generation: 0,
            host_chat_session_active: false,
            chat_session_id: None,
            chat_resume_identity: None,
            chat_session_dirty: false,
            // A genuinely fresh logical chat id for every launch. Saved chats remain
            // available through `/sessions` + explicit `/resume <id>`; startup never
            // silently imports an old task or its native base-session pointer.
            chat_id: new_chat_session_id(),
            run_session_handed_to_chat: false,
            director_run_in_flight: false,
            director_gate_paused: false,
            budget_paused: false,
            pending_director_gate: None,
            gate_query_in_flight: false,
            gate_query_epoch: 0,
            active_gate_query_epoch: None,
            route_backlog_len: 0,
            backend,
            backend_label,
            base_model,
            base_model_live: false,
            base_context_window,
            base_session_models: Vec::new(),
            base_session_model: None,
            base_session_mode: None,
            base_session_thinking: None,
            base_session_thinking_can_enable: false,
            base_session_thinking_can_disable: false,
            base_session_commands: Vec::new(),
            base_session_tools: Vec::new(),
            base_session_plan: Vec::new(),
            slug: slug.into(),
            requirement: String::new(),
            phases,
            active_gate: None,
            gate_choice: None,
            gate_choice_sel: 0,
            pending_approval: None,
            pending_host_input: None,
            auth_ui: None,
            host_input_draft: None,
            finished: false,
            run_started: false,
            aborted: false,
            degraded: false,
            run_degraded_seen: false,
            tasks: Vec::new(),
            task_seq: 0,
            bell_enabled: bell_enabled_from_env(std::env::var("UMADEV_BELL").ok().as_deref()),
            bell_pending: false,
            bell_count: 0,
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
            help_max_scroll: std::cell::Cell::new(0),
            overlay: None,
            search: None,
            history_search: None,
            preview_server: std::sync::Arc::new(std::sync::Mutex::new(None)),
            project_root,
            pending_quit_confirm: false,
            pending_rewind: false,
            interrupt_armed_at: None,
            kill_ring: VecDeque::new(),
            last_kill: None,
            yank_span: None,
            yank_ring_idx: 0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_snapshot_at: None,
            session_usage: SessionUsageMeter::default(),
            context_nudge_shown: false,
            status: String::new(),
            copy_toast: None,
            tick: 0,
            animations: animations_enabled_default(),
            verbose: false,
            // Seed from the shared flag `apply_process_logs()` just published —
            // captures BOTH the saved `/logs` preference and an external
            // `UMADEV_SHOW_PROCESS_LOGS` launch override (seeded into the flag), so
            // the renderer agrees with the base drivers from turn one.
            show_process_logs: umadev_host::process_logs::show_process_logs(),
            should_quit: false,
            run_started_at: None,
            phase_started_at: None,
            pending_auto_continue: None,
            queued_steer: VecDeque::new(),
            pending_steer: None,
            queued_chat: std::collections::VecDeque::new(),
            queued_dispatch_kinds: VecDeque::new(),
            live_input_ready: false,
            prompt_queue: PromptQueueUi::default(),
            queued_turn_inputs: VecDeque::new(),
            pending_route_input: None,
            last_dispatched_chat: None,
            stream_tool_batch: None,
            stream_text_active: false,
            stream_md_cache: std::cell::RefCell::new(crate::ui::StreamMarkdownCache::default()),
            msg_fold_cache: std::cell::RefCell::new(crate::ui::MsgFoldCache::new()),
            transcript_cache: std::cell::RefCell::new(crate::ui::TranscriptCache::new()),
            last_output_at: None,
            tool_in_progress: false,
            long_op_in_progress: false,
            transient_status: None,
            plan_steps: Vec::new(),
            plan_collapsed: false,
            plan_frozen: false,
            critic_verdicts: Vec::new(),
            critics_collapsed: false,
            critic_round_open: false,
            handoffs: Vec::new(),
            last_intent_class: None,
            session_turns: 0,
            example_file: std::cell::RefCell::new(None),
        };
        app.load_history();
        // Reload the persisted background-task registry so recent / interrupted
        // runs survive a relaunch (fail-open: missing/corrupt file → no-op).
        app.load_tasks();
        if app.mode == AppMode::Chat {
            app.push_greeting();
            // Launch is a NEW conversation. Persistence is still durable and
            // discoverable, but only an explicit `/resume <id>` may restore its
            // transcript/native thread. This keeps closing/reopening UmaDev from
            // turning an old project task into authority for the user's new ask.
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

    fn memory_capture_enabled(&self, store: umadev_agent::memory_control::MemoryStore) -> bool {
        umadev_agent::memory_control::capture_enabled(
            &self.project_root,
            umadev_agent::memory_control::MemoryScope::Project,
            store,
        )
    }

    fn memory_recall_enabled(&self, store: umadev_agent::memory_control::MemoryStore) -> bool {
        umadev_agent::memory_control::recall_enabled(
            &self.project_root,
            umadev_agent::memory_control::MemoryScope::Project,
            store,
        )
    }

    fn existing_project_umadev_dir(&self) -> Option<std::path::PathBuf> {
        let root = std::fs::canonicalize(&self.project_root).ok()?;
        if !umadev_state::fs::real_dir(&root) {
            return None;
        }
        let umadev = root.join(".umadev");
        umadev_state::fs::real_dir(&umadev).then_some(umadev)
    }

    fn ensure_project_umadev_dir(&self) -> Option<std::path::PathBuf> {
        let root = std::fs::canonicalize(&self.project_root).ok()?;
        if !umadev_state::fs::real_dir(&root) {
            return None;
        }
        umadev_state::fs::ensure_real_child_dir(&root, ".umadev").ok()
    }

    fn history_path(&self) -> std::path::PathBuf {
        self.project_root.join(".umadev").join("input-history.txt")
    }

    fn load_history(&mut self) {
        if !self.memory_recall_enabled(umadev_agent::memory_control::MemoryStore::InputHistory) {
            return;
        }
        let Some(umadev) = self.existing_project_umadev_dir() else {
            return;
        };
        let path = umadev.join("input-history.txt");
        let Ok(bytes) = umadev_state::fs::read_bounded(&path, MAX_INPUT_HISTORY_BYTES) else {
            return;
        };
        let Ok(body) = String::from_utf8(bytes) else {
            return;
        };
        // Prefer the JSON-array form so a MULTI-LINE submitted entry (a wrapped
        // requirement built with Ctrl+J) survives a restart as ONE entry. The old
        // newline-joined format split such an entry into several one-line entries
        // on load. Fail open to the legacy line-delimited form so an existing
        // history file — or a hand-edited one — still loads (each line an entry).
        let entries: Vec<String> = serde_json::from_str::<Vec<String>>(&body)
            .unwrap_or_else(|_| body.lines().map(str::to_string).collect());
        for entry in entries.into_iter().rev().take(INPUT_HISTORY_CAP) {
            if !entry.is_empty() {
                self.input_history.push_front(entry);
            }
        }
    }

    fn persist_history(&self) {
        if !self.memory_capture_enabled(umadev_agent::memory_control::MemoryStore::InputHistory) {
            return;
        }
        let Some(parent) = self.ensure_project_umadev_dir() else {
            return;
        };
        let path = parent.join("input-history.txt");
        // Serialize the ring as a JSON array so a multi-line entry round-trips as
        // a single entry (the newline-join would otherwise re-split it on load).
        // Fail-open: a serialize error skips the write rather than corrupting the
        // file. The ring is already capped at `HISTORY_CAP_PROMPTS` on submit.
        let entries: Vec<&String> = self.input_history.iter().collect();
        if let Ok(json) = serde_json::to_string(&entries) {
            if json.len() <= usize::try_from(MAX_INPUT_HISTORY_BYTES).unwrap_or(usize::MAX) {
                let _ = umadev_state::fs::atomic_write(&path, json.as_bytes());
            }
        }
    }

    /// Directory holding this project's persisted chats (Wave 5 / G11):
    /// `.umadev/chat/`. One `<id>.json` per saved chat so `/sessions` can list it
    /// and an explicit `/resume <id>` can reopen it after a restart.
    #[cfg(test)]
    fn chat_dir(&self) -> std::path::PathBuf {
        self.project_root.join(".umadev").join("chat")
    }

    fn ensure_chat_dir(&self) -> Option<std::path::PathBuf> {
        let umadev = self.ensure_project_umadev_dir()?;
        umadev_state::fs::ensure_real_child_dir(&umadev, "chat").ok()
    }

    fn existing_chat_dir(&self) -> Option<std::path::PathBuf> {
        let umadev = self.existing_project_umadev_dir()?;
        let chat = umadev.join("chat");
        umadev_state::fs::real_dir(&chat).then_some(chat)
    }

    fn valid_chat_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }

    /// The on-disk path for a chat by id: `.umadev/chat/<id>.json`.
    #[cfg(test)]
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
        if self.full_transcript.is_empty()
            || !Self::valid_chat_id(&self.chat_id)
            || !self.memory_capture_enabled(umadev_agent::memory_control::MemoryStore::ChatSessions)
        {
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
            base_resume_identity: self.chat_resume_identity.clone(),
            // The FULL, append-only transcript — never the compacted working view —
            // so the on-disk history survives in full and is never mutated by
            // compaction. `/resume` reopens the complete conversation.
            messages: self.full_transcript.clone(),
            // Wave 3 — the VISIBLE rendered transcript (prose + tool rows + diff
            // cards + notes), already bounded by HISTORY_CAP, so a relaunch
            // rebuilds the exact screen the user left instead of an empty one.
            display: Some(self.history.iter().cloned().collect()),
        };
        let Ok(body) = serde_json::to_string_pretty(&session) else {
            return;
        };
        if body.len() > usize::try_from(MAX_CHAT_FILE_BYTES).unwrap_or(usize::MAX) {
            return;
        }
        let Some(dir) = self.ensure_chat_dir() else {
            return;
        };
        let final_path = dir.join(format!("{}.json", self.chat_id));
        let _ = umadev_state::fs::atomic_write(&final_path, body.as_bytes());
    }

    /// Remove this chat's persisted file (best-effort, **fail-open**). Used when a
    /// transcript that was previously saved becomes EMPTY (a rewind of the first
    /// user turn): [`Self::persist_chat`] deliberately skips an empty transcript to
    /// avoid empty-file litter, so without an explicit delete the stale, un-rewound
    /// chat would survive on disk and a relaunch `/resume` would restore the very
    /// conversation the rewind dropped. A missing file / IO error is swallowed.
    fn discard_persisted_chat(&self) {
        if Self::valid_chat_id(&self.chat_id) {
            if let Some(dir) = self.existing_chat_dir() {
                let path = dir.join(format!("{}.json", self.chat_id));
                let _ = umadev_state::fs::remove_regular_file(&path);
            }
        }
    }

    /// List persisted chats for this project, most-recently-updated first (Wave 5).
    /// Returns `(id, updated_at, turn_count, preview)` tuples. Fail-open: a missing
    /// dir / unreadable / corrupt file yields an empty list (never an error).
    pub(crate) fn list_chats(&self) -> Vec<(String, String, usize, String)> {
        let mut out: Vec<(String, String, usize, String)> = Vec::new();
        let Some(chat_dir) = self.existing_chat_dir() else {
            return out;
        };
        let Ok(entries) = std::fs::read_dir(chat_dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(file_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !Self::valid_chat_id(file_id)
                || !std::fs::symlink_metadata(&path)
                    .is_ok_and(|metadata| umadev_state::fs::metadata_is_real_file(&metadata))
            {
                continue;
            }
            let Ok(bytes) = umadev_state::fs::read_bounded(&path, MAX_CHAT_FILE_BYTES) else {
                continue;
            };
            let Ok(session) = serde_json::from_slice::<ChatSession>(&bytes) else {
                continue;
            };
            if session.id != file_id {
                continue;
            }
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
    ///
    /// Wave 3: also REBUILDS the visible display transcript ([`Self::history`])
    /// — from the persisted rich rows when the file has them, else seeded as
    /// plain prose from the durable transcript — ending with a restore divider
    /// and re-pinned to the bottom, so reopening shows the conversation, never
    /// an empty screen.
    pub(crate) fn load_chat(&mut self, id: &str) -> bool {
        if !Self::valid_chat_id(id) {
            return false;
        }
        let Some(chat_dir) = self.existing_chat_dir() else {
            return false;
        };
        let path = chat_dir.join(format!("{id}.json"));
        let Ok(bytes) = umadev_state::fs::read_bounded(&path, MAX_CHAT_FILE_BYTES) else {
            return false;
        };
        let Ok(session) = serde_json::from_slice::<ChatSession>(&bytes) else {
            return false;
        };
        if session.id != id || session.messages.is_empty() {
            return false;
        }
        let saved_backend = session.backend.trim().to_string();
        let current_backend = self
            .backend
            .as_deref()
            .filter(|backend| !backend.is_empty())
            .unwrap_or("offline")
            .to_string();
        let cross_backend = saved_backend != current_backend;
        // Restore the full durable transcript AND the working view from it. A new
        // generation invalidates any in-flight compaction from the prior chat, and
        // the safety net bounds the working view; the next turn re-triggers
        // token-budgeted compaction if the restored conversation is over budget.
        self.full_transcript = session.messages.clone();
        self.conversation = session.messages;
        self.enforce_conversation_safety_net();
        self.conversation_generation = self.conversation_generation.wrapping_add(1);
        self.compaction_in_flight = false;
        self.chat_id = session.id;
        // Restore the base's OWN resumable session id so the NEXT host chat turn
        // RESUMES the base's deep context (the resident pre-load opens it via
        // `--resume <id>` / `thread/resume`) instead of cold-starting and replaying
        // only the ≤16-message transcript. Fail-open: an old chat file (or an
        // opencode / offline chat) has `None` here and cleanly takes the fresh-session
        // path.
        let requested_identity = crate::session_slot::requested_resume_identity(
            &current_backend,
            &self.project_root,
            self.effective_trust_mode().base_permissions(),
        );
        let resume_allowed = chat_resume_identity_allows_load(
            &saved_backend,
            &current_backend,
            session.base_resume_identity.as_ref(),
            requested_identity.as_ref(),
        );
        self.chat_session_id = (!cross_backend && resume_allowed)
            .then_some(session.base_session_id)
            .flatten()
            .filter(|id| !id.trim().is_empty());
        self.chat_resume_identity = self
            .chat_session_id
            .as_ref()
            .and(session.base_resume_identity);
        self.host_chat_session_active = self.chat_session_id.is_some();
        // Loading replaces the logical conversation even when the backend id is
        // unchanged. The resident process may still hold the chat we are leaving,
        // so it must be closed before the restored context is driven.
        self.chat_session_dirty = true;
        self.reset_base_session_state();
        self.clear_transient_routing_state();
        // Wave 3 — rebuild the VISIBLE display transcript. Prefer the persisted
        // rich rows (tool rows / diff cards / notes survive verbatim; any row
        // saved mid-flight settles to Aborted so nothing spins forever); an old
        // file without the field — or one whose display rows were all corrupt
        // (`lenient_display_rows` → `None`) — falls back to plain prose seeded
        // from the durable transcript, so even a legacy chat reopens visible.
        let rows: Vec<ChatMessage> = session.display.map_or_else(
            || seed_display_from_transcript(&self.full_transcript),
            |d| d.into_iter().map(settle_restored_row).collect(),
        );
        self.history.clear();
        self.history.extend(rows);
        while self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
        // The replaced transcript invalidates every display-derived live pointer
        // (indices into the OLD `history`, the streaming caches, a drag selection
        // or search over rows that no longer exist) — mirror the `/clear` resets.
        self.thinking_block_idx = None;
        self.thinking_block_start = None;
        self.stream_tool_batch = None;
        self.stream_text_active = false;
        self.reset_stream_md_cache();
        self.selection = None;
        self.selection_dragging = false;
        self.search = None;
        if cross_backend {
            let from = if saved_backend.is_empty() {
                "unknown"
            } else {
                saved_backend.as_str()
            };
            let handoff = umadev_i18n::tf(self.lang, "backend.handoff", &[from, &current_backend]);
            self.record_turn("system", handoff.clone());
            self.push(ChatRole::System, handoff);
            self.persist_chat();
        }
        // The restore boundary: the reopened conversation ends here and new turns
        // continue below — the same affordance as the run-resume separator, so
        // the transcript reads as one continuous history to scroll back through.
        self.push(
            ChatRole::System,
            umadev_i18n::t(self.lang, "chat.restored_divider"),
        );
        // Land pinned to the bottom (the newest rows + the divider on screen) and
        // heal the frame in full — the whole viewport just changed.
        self.transcript_scroll.set(0);
        self.transcript_prev_hidden.set(0);
        self.request_transcript_repaint();
        true
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
        } else if cfg!(windows)
            && self.backend.as_deref() == Some("codex")
            && !matches!(mode, umadev_agent::config::CodexSandbox::DangerFullAccess)
        {
            // Windows + codex + a RESTRICTIVE sandbox is doubly problematic and worth flagging
            // UP FRONT (before the user hits a native error dialog UmaDev can't intercept):
            // (1) codex's `workspace-write` OS-sandbox launches `codex-windows-sandbox-setup.exe`,
            // which fails with "找不到指定的模块" (a missing runtime) on some machines, and
            // (2) even when it loads, this sandbox BLOCKS the network, local dev ports and git —
            // so a full-stack build (npm install / a dev server / git) cannot complete. Point at
            // the one-command fix.
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "codex.sandbox.windows_hint").to_string(),
            );
        }
    }

    /// Show a workspace-integrity note in the transcript as a system row.
    ///
    /// The recovery paths that raise these (a run killed inside a temporary evidence
    /// rewind → the tree put back, or a tree we could NOT put back) run before any UI
    /// exists, so they can only leave the note behind them
    /// (`umadev_agent::checkpoint::take_workspace_notices`). Under the TUI a
    /// `tracing::warn!` goes to a log file and a startup `eprintln!` is wiped by the
    /// alternate screen — this is the surface the user actually reads.
    pub fn push_workspace_notice(&mut self, note: impl Into<String>) {
        self.push(ChatRole::System, note);
    }

    /// Surface one localized clipboard-image result in the transcript. Kept as
    /// a narrow crate-visible door so the event loop does not need access to the
    /// general private `push` primitive.
    pub(crate) fn push_clipboard_image_notice(&mut self, key: &str, args: &[&str]) {
        self.push(ChatRole::System, umadev_i18n::tf(self.lang, key, args));
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
        if let Some((label, false)) = claude_subagent_row(name) {
            let working = self.history.iter_mut().rev().find_map(|message| {
                let MessageBody::Tool(tool) = &mut message.kind else {
                    return None;
                };
                let Some((candidate, true)) = claude_subagent_row(&tool.name) else {
                    return None;
                };
                (message.role == ChatRole::Host
                    && tool.status == ToolStatus::Running
                    && candidate == label)
                    .then_some(tool)
            });
            if let Some(tool) = working {
                tool.name = name.to_string();
                tool.arg = arg;
                tool.result = None;
                tool.merged = false;
                tool.count = 1;
                tool.collapsed = false;
                self.stream_tool_batch = None;
                return;
            }
        }
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
                    call_id: None,
                    name: name.to_string(),
                    arg: summary,
                    status: ToolStatus::Running,
                    result: None,
                    progress: None,
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
                call_id: None,
                name: name.to_string(),
                arg,
                status: ToolStatus::Running,
                result: None,
                progress: None,
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

    /// Push one independently addressable tool row. Correlated calls are never
    /// merged into the low-signal batch: retaining a one-id/one-row mapping is
    /// required for correct interleaved progress and terminal updates.
    fn push_tool_use_correlated(&mut self, call_id: &str, name: &str, detail: &str) {
        self.stream_tool_batch = None;
        self.history.push_back(ChatMessage {
            role: ChatRole::Host,
            kind: MessageBody::Tool(ToolCall {
                call_id: Some(call_id.to_string()),
                name: name.to_string(),
                arg: detail.chars().take(80).collect(),
                status: ToolStatus::Running,
                result: None,
                progress: None,
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
        self.push_diff_with_call_id(edit, None);
    }

    fn push_diff_correlated(&mut self, call_id: &str, edit: &umadev_runtime::ToolEdit) {
        self.push_diff_with_call_id(edit, Some(call_id));
    }

    fn push_diff_with_call_id(&mut self, edit: &umadev_runtime::ToolEdit, call_id: Option<&str>) {
        let mut diff = FileDiff::from_tool_edit(edit);
        diff.call_id = call_id.map(str::to_string);
        if diff.hunks.is_empty() {
            // Nothing actually changed (or unreadable) — keep the plain row so the
            // activity is still visible, never an empty card.
            let name = if edit.before.is_empty() {
                "Write"
            } else {
                "Edit"
            };
            if let Some(call_id) = call_id {
                self.push_tool_use_correlated(call_id, name, &edit.path);
            } else {
                self.push_tool_use(name, &edit.path);
            }
            return;
        }
        // Defensive: never render the SAME diff card twice in a row. UmaDev's own
        // pipeline emits one card per tool call, but a base can surface an edit both in
        // its streamed narration AND as the structured tool call (or an opencode tool
        // part can arrive under two ids), landing a byte-identical card right after the
        // last one - the reported duplicate. Two genuinely-distinct edits never produce
        // byte-identical before/after (a re-edit sees a changed before), so collapsing
        // an exact consecutive duplicate is safe.
        if let Some(last) = self.history.back() {
            if let MessageBody::Diff(prev) = &last.kind {
                if *prev == diff {
                    return;
                }
            }
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
        self.attach_tool_result_for(None, ok, summary);
    }

    fn attach_tool_result_correlated(&mut self, call_id: &str, ok: bool, summary: &str) {
        self.attach_tool_result_for(Some(call_id), ok, summary);
    }

    fn attach_tool_result_for(&mut self, call_id: Option<&str>, ok: bool, summary: &str) {
        let lang = self.lang;
        let status = if ok { ToolStatus::Ok } else { ToolStatus::Fail };
        // Update the trailing tool row, then carry whether it was a merged batch
        // out of the borrow so the (separate) `stream_tool_batch` field can be
        // set without overlapping the `&mut self.history` borrow.
        let mut batch: Option<(String, u32)> = None;
        let mut handled = false;
        // Process-log visibility (`/logs`): for a long-running command row, keep the
        // FULL captured output (the renderer still folds it to a head preview) and
        // leave the row EXPANDED so the streamed build log stays visible instead of
        // auto-collapsing to a checkmark. OFF (the default) keeps the tight 200-char
        // clip + auto-collapse, exactly as before.
        let show_logs = self.show_process_logs;
        let target = if let Some(call_id) = call_id {
            self.history.iter().rposition(|message| {
                message.role == ChatRole::Host
                    && matches!(
                        &message.kind,
                        MessageBody::Tool(tool)
                            if tool.status == ToolStatus::Running
                                && tool.call_id.as_deref() == Some(call_id)
                    )
            })
        } else if is_claude_subagent_result(summary) {
            self.history.iter().rposition(|message| {
                message.role == ChatRole::Host
                    && matches!(
                        &message.kind,
                        MessageBody::Tool(tool)
                            if tool.status == ToolStatus::Running
                                && claude_subagent_row(&tool.name)
                                    .is_some_and(|(_, working)| !working)
                    )
            })
        } else {
            self.history.len().checked_sub(1)
        };
        if let Some(last) = target.and_then(|idx| self.history.get_mut(idx)) {
            if last.role == ChatRole::Host {
                if let MessageBody::Tool(t) = &mut last.kind {
                    t.status = status;
                    t.progress = None;
                    let verbose_cmd = show_logs && t.name == "Bash";
                    let cap = if verbose_cmd {
                        PROCESS_LOG_PREVIEW_CHARS
                    } else {
                        200
                    };
                    let preview: String = summary.chars().take(cap).collect();
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
                    // Auto-collapse a finished OK call; a failure stays open. With
                    // process logs on, a command row stays EXPANDED so its live /
                    // full output is visible (the user asked to see it).
                    t.collapsed = ok && !verbose_cmd;
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
        let matching_diff = call_id.map_or_else(
            || {
                self.history
                    .back()
                    .is_some_and(|last| matches!(last.kind, MessageBody::Diff(_)))
            },
            |call_id| {
                self.history.iter().any(|message| {
                    matches!(
                        &message.kind,
                        MessageBody::Diff(diff) if diff.call_id.as_deref() == Some(call_id)
                    )
                })
            },
        );
        if ok && matching_diff {
            return;
        }
        // No trailing tool row — fail-open to a plain status line (old look).
        let mark = if ok { "[ok]" } else { "[fail]" };
        let preview: String = summary.chars().take(100).collect();
        if !preview.trim().is_empty() {
            self.push(ChatRole::Host, format!("  {mark} {preview}"));
        }
    }

    /// Append live output to the newest running tool row without settling it.
    ///
    /// Unlike [`Self::attach_tool_result`], this never changes the status,
    /// collapses the row, or clears the in-flight marker. The rolling tail is
    /// bounded so a noisy long-running build cannot grow the transcript without
    /// limit. A missing start frame degrades to a plain running Bash row; the
    /// later terminal result will still settle that row normally.
    fn attach_tool_output_delta(&mut self, delta: &str) {
        self.attach_tool_output_delta_for(None, delta);
    }

    fn attach_tool_output_delta_correlated(&mut self, call_id: &str, delta: &str) {
        self.attach_tool_output_delta_for(Some(call_id), delta);
    }

    fn attach_tool_output_delta_for(&mut self, call_id: Option<&str>, delta: &str) {
        if delta.trim().is_empty() {
            return;
        }
        let target = self.history.iter().rposition(|message| {
            message.role == ChatRole::Host
                && matches!(
                    &message.kind,
                    MessageBody::Tool(tool)
                        if tool.status == ToolStatus::Running
                            && call_id.is_none_or(|id| tool.call_id.as_deref() == Some(id))
                )
        });
        let target = target.unwrap_or_else(|| {
            if let Some(call_id) = call_id {
                self.push_tool_use_correlated(call_id, "Bash", "");
            } else {
                self.push_tool_use("Bash", "");
            }
            self.history.len().saturating_sub(1)
        });
        let Some(message) = self.history.get_mut(target) else {
            return;
        };
        let MessageBody::Tool(tool) = &mut message.kind else {
            return;
        };
        let output = tool.result.get_or_insert_with(String::new);
        output.push_str(delta);
        let chars = output.chars().count();
        if chars > PROCESS_LOG_PREVIEW_CHARS {
            let drop_chars = chars - PROCESS_LOG_PREVIEW_CHARS;
            if let Some((byte, _)) = output.char_indices().nth(drop_chars) {
                output.drain(..byte);
            }
        }
        tool.collapsed = false;
    }

    /// Replace the newest running tool's visible output with a complete buffer.
    /// This is distinct from a delta: ACP peers use snapshots to reconcile a
    /// truncated terminal buffer and an empty snapshot explicitly clears it.
    fn attach_tool_output_snapshot(&mut self, snapshot: &str) {
        self.attach_tool_output_snapshot_for(None, snapshot);
    }

    fn attach_tool_output_snapshot_correlated(&mut self, call_id: &str, snapshot: &str) {
        self.attach_tool_output_snapshot_for(Some(call_id), snapshot);
    }

    fn attach_tool_output_snapshot_for(&mut self, call_id: Option<&str>, snapshot: &str) {
        let target = self.history.iter().rposition(|message| {
            message.role == ChatRole::Host
                && matches!(
                    &message.kind,
                    MessageBody::Tool(tool)
                        if tool.status == ToolStatus::Running
                            && call_id.is_none_or(|id| tool.call_id.as_deref() == Some(id))
                )
        });
        let Some(target) = target else {
            if snapshot.is_empty() {
                return;
            }
            if let Some(call_id) = call_id {
                self.push_tool_use_correlated(call_id, "Bash", "");
            } else {
                self.push_tool_use("Bash", "");
            }
            return self.attach_tool_output_snapshot_for(call_id, snapshot);
        };
        let Some(message) = self.history.get_mut(target) else {
            return;
        };
        let MessageBody::Tool(tool) = &mut message.kind else {
            return;
        };
        let mut output = snapshot.to_string();
        let chars = output.chars().count();
        if chars > PROCESS_LOG_PREVIEW_CHARS {
            let drop_chars = chars - PROCESS_LOG_PREVIEW_CHARS;
            if let Some((byte, _)) = output.char_indices().nth(drop_chars) {
                output.drain(..byte);
            }
        }
        tool.result = (!output.is_empty()).then_some(output);
        tool.collapsed = false;
    }

    /// Replace the non-terminal status title on the exact running tool card.
    /// Unknown/settled ids are ignored fail-open: creating a second card from a
    /// late progress frame would be more misleading than omitting that frame.
    fn attach_tool_progress_correlated(&mut self, call_id: &str, title: &str) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        if let Some(tool) = self.history.iter_mut().rev().find_map(|message| {
            let MessageBody::Tool(tool) = &mut message.kind else {
                return None;
            };
            (message.role == ChatRole::Host
                && tool.status == ToolStatus::Running
                && tool.call_id.as_deref() == Some(call_id))
            .then_some(tool)
        }) {
            tool.progress = Some(title.chars().take(160).collect());
            tool.collapsed = false;
        }
    }

    fn refresh_running_tool_flags(&mut self) {
        let mut running = false;
        let mut long_running = false;
        for message in &self.history {
            let MessageBody::Tool(tool) = &message.kind else {
                continue;
            };
            if tool.status != ToolStatus::Running {
                continue;
            }
            running = true;
            long_running |= tool.name == "Bash" && is_long_running_command(&tool.arg);
        }
        self.tool_in_progress = running;
        self.long_op_in_progress = long_running;
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
        // The single derived truth (see `run_state`) drives BOTH the terminal
        // label and the elapsed timer, so they can never disagree: a degraded exit
        // reads `[degraded]` with the timer STOPPED (never `[run] 运行中` still
        // ticking), and a live re-drive reads Running (never a stale `[aborted]`).
        let state = self.run_state();
        let done_label = match state {
            RunState::Delivered => " · [ok] delivered".to_string(),
            RunState::Aborted => {
                // Explicit terminal state — never let an aborted run read as idle.
                format!(
                    " · [aborted] {}",
                    umadev_i18n::t(self.lang, "status.aborted")
                )
            }
            RunState::Degraded => {
                // Terminal, but NOT a delivery: the base degraded to placeholder
                // templates / failed a hard gate. Show the honest `/redo` state
                // instead of a false `[ok] delivered` or a live `[run]`.
                format!(
                    " · [degraded] {}",
                    umadev_i18n::t(self.lang, "status.degraded")
                )
            }
            RunState::PausedAtBudget => {
                // Resumable budget pause — a `[paused]` label (timer stopped), never
                // `[aborted]`. The `/continue` hint carries the done/total detail.
                format!(
                    " · [paused] {}",
                    umadev_i18n::t(self.lang, "status.budget_paused")
                )
            }
            RunState::Running | RunState::PausedAtGate | RunState::Idle => String::new(),
        };
        let ds_short = self
            .config
            .design_system
            .as_deref()
            .map(|s| format!(" · [design] {s}"))
            .unwrap_or_default();
        // Total wall-clock since the block started, shown ONLY while genuinely
        // Running so the user has a clear "it's been N minutes" signal — a paused,
        // delivered, aborted, degraded or idle state stops the counter.
        let total_elapsed = self
            .run_started_at
            .filter(|_| state == RunState::Running)
            .map(|t| format!(" · [time] {}", fmt_elapsed(t.elapsed().as_secs())))
            .unwrap_or_default();
        self.status = format!(
            "● {}{}{}{}{}{}{}",
            self.backend_label, dots, running, gate_label, done_label, ds_short, total_elapsed
        );
    }

    /// One derived truth for the run's lifecycle so the status LABEL, the elapsed
    /// TIMER, the mode marker and the input placeholder can never disagree with the
    /// real run state. Every status surface derives from this instead of reading the
    /// raw `thinking` / `run_started_at` / `active_gate` / `finished` / `aborted` /
    /// `degraded` flags independently (which drifted out of sync: a degraded build
    /// kept `[run] 运行中` with the `[time]` timer ticking, and a base-failure
    /// re-drive read `[aborted]` while it was demonstrably thinking).
    ///
    /// **Live activity wins over any stale terminal flag.** A re-drive on the
    /// director/chat path sets `thinking` WITHOUT re-emitting `PipelineStarted` /
    /// `PhaseStarted`, so a leftover `aborted` / `degraded` / `finished` from the
    /// PRIOR block must never mask a turn that is provably working — hence the
    /// `thinking` check comes first.
    #[must_use]
    pub fn run_state(&self) -> RunState {
        if self.thinking {
            // Live activity wins over any stale terminal/pause flag (a re-drive).
            RunState::Running
        } else if self.active_gate.is_some() {
            RunState::PausedAtGate
        } else if self.budget_paused {
            // A budget pause reads as a PAUSE (timer stopped, not aborted). Checked
            // before the `run_started_at` Running fallback so a parked run whose
            // `run_started_at` was already cleared can't read Running/Idle.
            RunState::PausedAtBudget
        } else if self.run_started_at.is_some() && self.active_gate.is_none() {
            RunState::Running
        } else if self.finished {
            RunState::Delivered
        } else if self.degraded {
            RunState::Degraded
        } else if self.aborted {
            RunState::Aborted
        } else {
            RunState::Idle
        }
    }

    /// Clear the TERMINAL run flags (`aborted` / `finished` / `degraded`) because a
    /// genuinely new working turn just began on the director/chat path (a posted
    /// plan, a ticking plan step). That path emits no `PipelineStarted` /
    /// `PhaseStarted`, so without this a leftover terminal flag from the PRIOR block
    /// would survive and a status surface reading the raw flag could paint
    /// `[aborted]` / `[degraded]` / `[ok]` over a turn that is demonstrably working
    /// (Scenario B). Leaves the per-run `run_degraded_seen` latch alone — callers
    /// that start a brand-new run reset it explicitly.
    fn clear_stale_terminal_on_live_turn(&mut self) {
        self.aborted = false;
        self.finished = false;
        self.degraded = false;
        // A resumed / fresh working turn clears a prior budget pause too, so its
        // `[paused]` label / hint can't paint over a turn that is demonstrably working.
        self.budget_paused = false;
    }

    /// `true` while the user has started a run that hasn't reached
    /// delivery yet. Used by the Esc-to-quit confirmation.
    ///
    /// An **aborted** OR **degraded** block is NOT active: the run is over (it
    /// bailed / stopped short of delivery), so it must not keep claiming the
    /// workspace or block a fresh `/run`. Without this, such a run would stay
    /// "active" forever and a retry would be wrongly refused as "a pipeline is
    /// already running" — and the header would keep painting `[run] 运行中`.
    #[must_use]
    pub fn is_pipeline_active(&self) -> bool {
        // A budget pause is terminal-for-now (the run stopped, awaiting `/continue`),
        // so it is NOT active — a fresh `/run` or a `/continue` must not be refused as
        // "a pipeline is already running", exactly like an aborted/degraded round.
        self.run_started && !self.finished && !self.aborted && !self.degraded && !self.budget_paused
    }

    /// Shared truth for Esc, Ctrl-C, and `/cancel`. A Director paused at a gate
    /// has already ended its writer process, so `thinking` and `agentic_in_flight`
    /// are false even though the run remains live and cancellable.
    #[must_use]
    fn has_interruptible_work(&self) -> bool {
        self.has_active_run()
            || self.active_gate.is_some()
            || self.director_gate_paused
            || self.gate_query_in_flight
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
        // Feature A — an honest abort/hard-stop (the run errored out, not a user
        // cancel) is a terminal state the away user should hear. Arm before the
        // timers are cleared, gated on how long the run had been going.
        self.arm_completion_bell(self.run_started_at.or(self.thinking_started));
        self.aborted = true;
        // `aborted` and `degraded` are mutually-exclusive terminal states — an
        // honest hard abort clears any prior degraded flag so `run_state` reads a
        // single unambiguous terminal.
        self.degraded = false;
        // The run errored out (not a user cancel) → its task is a Failed row.
        self.mark_active_task(TaskStatus::Failed);
        self.active_gate = None;
        self.gate_choice = None;
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
        // ...but a RESUMABLE stop (a transient abort OR a run-time-budget pause that
        // reached this path) with a resumable plan brings the checklist back FROZEN
        // (see the method) so the saved plan doesn't vanish; fail-open.
        self.rehydrate_frozen_plan_if_resumable(&body);
        self.push(ChatRole::System, body);
        // M2 — an honest abort fires no further gate/completion, so a steer
        // message parked in `queued_steer` (the pipeline-run queue) would stay
        // stuck forever: the "queued N" chip stays falsely lit and no key path
        // recovers it. Drain it here and surface it so the user knows to resend,
        // clearing the chip. (The clean-completion path does the same at
        // `BlockCompleted`.)
        if !self.queued_steer.is_empty() {
            let text = self.queued_steer.drain(..).collect::<Vec<_>>().join("\n");
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "run.queued_dropped", &[&text]),
            );
        }
        self.refresh_status();
    }

    /// Push a visible "— continued —" divider into the transcript when a
    /// blocked / interrupted run is resumed (`/continue`, `/tasks resume`).
    ///
    /// The resumed run APPENDS its output to the SAME durable [`Self::history`],
    /// so the earlier steps' per-step notes (plan-posted memo, `push_critic_note`
    /// verdicts, tool rows) stay in scrollback — a block only clears the LIVE
    /// PANEL state ([`Self::clear_live_panels`]), never the transcript. Without a
    /// marker, though, the transcript auto-sticks to the bottom on resume and the
    /// user sees only the newest (resumed) steps, so the run *reads* as if the
    /// earlier steps vanished (user-reported). This divider is the affordance: it
    /// marks where the run picked back up AND points the user to the earlier steps
    /// above, so the whole run reads as one continuous history to scroll back
    /// through. Fail-open: a pure `push`, never panics.
    fn push_resume_separator(&mut self) {
        self.push(
            ChatRole::System,
            umadev_i18n::t(self.lang, "continue.separator"),
        );
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
        let before = self.transcript_scroll.get();
        self.transcript_scroll
            .set(before.saturating_add(rows).min(max));
    }

    /// Scroll the transcript DOWN by `rows` (toward the newest content). Hitting
    /// `0` re-pins to the bottom and re-enables auto-stick.
    pub fn transcript_scroll_down(&mut self, rows: usize) {
        let before = self.transcript_scroll.get();
        self.transcript_scroll.set(before.saturating_sub(rows));
    }

    /// Jump to the very top of the transcript (oldest content on screen).
    pub fn transcript_scroll_to_top(&mut self) {
        self.transcript_scroll.set(self.transcript_max_scroll.get());
    }

    /// Jump back to the bottom (newest content) and re-enable auto-stick.
    pub fn transcript_scroll_to_bottom(&mut self) {
        self.transcript_scroll.set(0);
    }

    /// Scroll the help overlay DOWN by `rows`, clamped to the renderer-published
    /// bottom row. Without this clamp, holding ↓ at the bottom could overshoot
    /// `help_scroll` far past the visible range; the render stayed pinned to the
    /// bottom, but the next ↑ appeared to do nothing until the hidden overshoot
    /// counted back down.
    fn help_scroll_down(&mut self, rows: u16) {
        let max = self.help_max_scroll.get();
        self.help_scroll = self.help_scroll.saturating_add(rows).min(max);
    }

    /// Scroll the help overlay UP by `rows`.
    fn help_scroll_up(&mut self, rows: u16) {
        self.help_scroll = self.help_scroll.saturating_sub(rows);
    }

    /// Jump to the renderer-published bottom of the help overlay.
    fn help_scroll_to_bottom(&mut self) {
        self.help_scroll = self.help_max_scroll.get();
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

    /// Mirror the renderer's `transcript_first_visible` from the CURRENT scroll
    /// offset, without waiting for the next frame. The renderer publishes
    /// `first_visible = hidden_above − user_offset` once per draw; after a
    /// programmatic scroll inside one event (wheel-during-drag, edge
    /// auto-scroll) the cached value is stale, so a same-event re-resolve would
    /// map the screen cell to the OLD content row. Recompute it here so the
    /// re-resolve sees the new geometry immediately. Pure + fail-open.
    fn sync_first_visible(&self) {
        let hidden_above = self.transcript_max_scroll.get();
        let user_offset = self.transcript_scroll.get().min(hidden_above);
        self.transcript_first_visible
            .set(hidden_above.saturating_sub(user_offset));
    }

    /// Re-resolve the live selection's cursor at screen `(col, row)`, CLAMPING
    /// the point into the transcript rectangle first so a position dragged off
    /// an edge pins the end to the nearest visible row/col (rather than freezing
    /// the cursor as the raw mapping would). No-op without a live selection or a
    /// zero-size area. Used by both the wheel-during-drag and edge-auto-scroll
    /// extension paths after they have applied their scroll.
    fn resolve_cursor_clamped(&mut self, col: u16, row: u16) {
        let (left, top, width, height) = self.transcript_area.get();
        if width == 0 || height == 0 {
            return;
        }
        let cc = col.clamp(left, left.saturating_add(width).saturating_sub(1));
        let cr = row.clamp(top, top.saturating_add(height).saturating_sub(1));
        if let Some(p) = self.map_mouse_point(cc, cr) {
            if let Some(sel) = self.selection.as_mut() {
                sel.cursor = p;
            }
        }
    }

    /// Surface the copy/paste escape-hatch hint **once** per session.
    ///
    /// Called when the user drags to select but no in-app selection is active —
    /// i.e. the drag began OUTSIDE the transcript (the input box / padding), where
    /// the in-app drag-copy layer can't reach and, with mouse capture on, the
    /// terminal's own click-drag is suppressed. That moment reads exactly as "I
    /// can't copy", so point at the working paths: Shift+drag for native selection
    /// (which DOES cover the input box) or `/mouse` to release capture entirely.
    /// Latches after the first show so it never nags; no-op once shown or when a
    /// real transcript selection is in progress. Fail-open: pure state + a toast.
    pub fn hint_native_copy_once(&mut self) {
        if self.native_copy_hint_shown || self.selection.is_some() {
            return;
        }
        self.native_copy_hint_shown = true;
        self.push(
            ChatRole::System,
            umadev_i18n::t(self.lang, "tui.copy_hint").to_string(),
        );
    }

    /// Mouse-down (left button) at screen `(col, row)`: begin a fresh selection
    /// anchored at the mapped content point. A click OUTSIDE the transcript
    /// clears any existing selection (so a copied span un-highlights once the
    /// user clicks away). A down inside the transcript also opens a drag
    /// (`selection_dragging`) and records the position, so a wheel notch before
    /// the first drag move can already extend the selection past the viewport.
    pub fn selection_begin(&mut self, col: u16, row: u16) {
        // A transcript down retires any INPUT-box selection so the two highlight
        // layers never coexist (clear the other when one begins).
        self.input_selection = None;
        self.input_selection_dragging = false;
        if let Some(p) = self.map_mouse_point(col, row) {
            self.selection = Some(crate::selection::Selection::at(p));
            self.selection_dragging = true;
            self.last_drag_mouse = Some((col, row));
        } else {
            self.selection = None;
            self.selection_dragging = false;
            self.last_drag_mouse = None;
        }
    }

    /// Mouse-drag (left button held) at screen `(col, row)`: extend the live
    /// selection's cursor toward the cursor. Records the position (so a wheel
    /// notch can re-resolve there) and, when the drag has gone PAST the top or
    /// bottom edge of the transcript, auto-scrolls one step in that direction
    /// and pins the end to the newly revealed edge row — the standard "drag past
    /// the edge to keep selecting" behavior. Inside the area it just moves the
    /// end to the mapped point. No-op when there's no active selection.
    pub fn selection_extend(&mut self, col: u16, row: u16) {
        self.last_drag_mouse = Some((col, row));
        // Edge auto-scroll: a drag STRICTLY beyond the top/bottom edge pulls one
        // off-screen row into view per drag event (a drag that stays inside,
        // even on the boundary row, never auto-scrolls — that would make
        // selecting the last visible line jitter the viewport).
        let (_, top, _, height) = self.transcript_area.get();
        if height > 0 {
            let bottom = top.saturating_add(height); // first row BELOW the area
            if row < top {
                self.transcript_scroll_up(1);
                self.sync_first_visible();
            } else if row >= bottom {
                self.transcript_scroll_down(1);
                self.sync_first_visible();
            }
        }
        self.resolve_cursor_clamped(col, row);
    }

    /// Route one wheel notch while a drag-selection MAY be active. Scrolls the
    /// transcript exactly as [`Self::mouse_wheel`] does, and — when a drag is in
    /// progress and no overlay owns the wheel — additionally re-resolves the
    /// selection's cursor at the last drag position so the selection GROWS to
    /// include the rows the scroll just revealed. The anchor is left fixed, so
    /// the user can wheel up/down mid-drag to extend the copy span beyond the
    /// visible viewport (the reported "复制文字没法滚轮复制更多" gap). With no
    /// active drag this is a plain wheel-scroll. Returns `true` if consumed.
    pub fn mouse_wheel_select(&mut self, up: bool, step: usize) -> bool {
        let consumed = self.mouse_wheel(up, step);
        if consumed && self.selection_dragging && self.overlay.is_none() {
            // The scroll moved which content row sits under the cursor; mirror
            // the renderer's first-visible NOW so the re-resolve sees it, then
            // extend to the last drag position over the freshly revealed rows.
            self.sync_first_visible();
            if let Some((col, row)) = self.last_drag_mouse {
                self.resolve_cursor_clamped(col, row);
            }
        }
        consumed
    }

    /// Mouse-up (left button) — finish the selection and copy it. When there is
    /// a non-empty selection, extracts its text, shows a "copied N chars" toast
    /// (kept private to this module) and returns `Some(text)` for the caller to
    /// hand to `copy_to_clipboard` (which owns the native-command-vs-OSC 52
    /// decision). The selection is KEPT highlighted so the user sees what was
    /// copied; a later mouse-down elsewhere clears it. Returns `None` (leaving
    /// any single-click selection in place) when nothing is selected — fail-open.
    #[must_use]
    pub fn selection_finish_copy(&mut self) -> Option<String> {
        // The button released: the drag is over (the span stays highlighted, but
        // a later wheel notch must scroll, not extend). Clear unconditionally.
        self.selection_dragging = false;
        self.last_drag_mouse = None;
        let sel = self.selection?;
        if sel.is_empty() {
            return None;
        }
        let text = {
            let rows = self.transcript_rows.borrow();
            let wraps = self.transcript_row_wraps.borrow();
            // Rejoin soft-wrapped visual rows so a wrapped paragraph copies as one
            // line; `wraps` is in lockstep with `rows` and fails open to a plain
            // newline-per-row extract when empty/short.
            crate::selection::extract_wrapped(&rows, &wraps, &sel)
        };
        if text.is_empty() {
            return None;
        }
        let count = text.chars().count();
        self.show_copy_toast(count);
        Some(text)
    }

    // ---- in-app text selection INSIDE the input composer box -------------
    //
    // A SEPARATE layer from the transcript selection above so a drag over the
    // text the user is composing selects + copies it too (Claude Code parity),
    // without `/mouse`. The geometry (`input_area` / `input_rows` /
    // `input_gutter` / `input_scroll`) is published every frame by
    // `ui::render_prompt`. All three entry points are fail-open: a point outside
    // the input rect, an empty selection, or stale/empty cached rows no-ops.

    /// Map a screen point against the last frame's published input-box geometry,
    /// reusing the transcript mapper with the input rect, `input_scroll` as the
    /// first-visible row, and the uniform mode-prefix gutter. Returns a
    /// `(visual_row, char_col)` point into [`Self::input_rows`], or `None` when the
    /// point is outside the input box (or there is nothing cached yet).
    fn map_input_point(&self, col: u16, row: u16) -> Option<crate::selection::Point> {
        let rows = self.input_rows.borrow();
        if rows.is_empty() {
            return None;
        }
        // The gutter is uniform across rows (mode prefix on row 0, an equal-width
        // indent on continuation rows), so one value fills the per-row slice.
        let gutters = vec![self.input_gutter.get(); rows.len()];
        crate::selection::screen_to_content(
            col,
            row,
            self.input_area.get(),
            self.input_scroll.get(),
            &rows,
            &gutters,
        )
    }

    /// Resolve an input-selection `(visual_row, char_col)` point to an absolute
    /// char index into [`Self::input`]. `offset_at_wrapped(row, 0)` gives the char
    /// index of the row's first glyph; adding `col` (a char count within that row,
    /// where no hard newline is consumed mid-row) lands on the exact char. Clamped
    /// to the buffer length — fail-open.
    fn input_char_index(&self, row: usize, col: usize) -> usize {
        let w = self.input_text_cols.get();
        let row16 = u16::try_from(row).unwrap_or(u16::MAX);
        let row_start = crate::ui::offset_at_wrapped(&self.input, row16, 0, w);
        row_start.saturating_add(col).min(self.input_len())
    }

    /// Extract the selected input text for `sel`. Both endpoints resolve to
    /// absolute char indices in [`Self::input`] and the substring between them is
    /// taken — so a soft-wrapped line copies as one unbroken line (the wrap points
    /// aren't chars in the buffer) while a real `Ctrl+J` newline is kept. Char
    /// indexing throughout is CJK-safe. Fail-open: a collapsed/empty range → `""`.
    fn input_selection_text(&self, sel: &crate::selection::Selection) -> String {
        let ((sr, sc), (er, ec)) = sel.normalized();
        let start = self.input_char_index(sr, sc);
        let end = self.input_char_index(er, ec);
        if end <= start {
            return String::new();
        }
        self.input.chars().skip(start).take(end - start).collect()
    }

    /// Mouse-down (left button) at screen `(col, row)`: if the point is inside the
    /// published input box, begin a fresh input selection anchored there and return
    /// `true`; otherwise leave the input layer untouched and return `false` (the
    /// caller then routes to the transcript layer). Beginning an input selection
    /// retires any TRANSCRIPT selection so the two highlights never coexist.
    pub fn input_selection_begin(&mut self, col: u16, row: u16) -> bool {
        // A secret host answer is intentionally painted as bullets. Selecting
        // those bullets must never copy the hidden plaintext through the native
        // clipboard path, so the composer selection layer is disabled for the
        // lifetime of the secret prompt.
        if self
            .pending_host_input
            .as_ref()
            .is_some_and(host_input::PendingHostInputView::is_secret)
        {
            return false;
        }
        let Some(p) = self.map_input_point(col, row) else {
            return false;
        };
        // Clear the other layer (don't fight the transcript selection).
        self.selection = None;
        self.selection_dragging = false;
        self.last_drag_mouse = None;
        self.input_selection = Some(crate::selection::Selection::at(p));
        self.input_selection_dragging = true;
        true
    }

    /// Mouse-drag (left button held) at screen `(col, row)`: extend the live
    /// input selection's cursor toward the point, CLAMPING it into the input rect
    /// first so a drag off an edge pins to the nearest visible cell. No-op without
    /// an active input drag.
    pub fn input_selection_extend(&mut self, col: u16, row: u16) {
        if !self.input_selection_dragging {
            return;
        }
        let (left, top, width, height) = self.input_area.get();
        if width == 0 || height == 0 {
            return;
        }
        let cc = col.clamp(left, left.saturating_add(width).saturating_sub(1));
        let cr = row.clamp(top, top.saturating_add(height).saturating_sub(1));
        if let Some(p) = self.map_input_point(cc, cr) {
            if let Some(sel) = self.input_selection.as_mut() {
                sel.cursor = p;
            }
        }
    }

    /// Mouse-up (left button) — finish an input-box selection and copy it. Mirrors
    /// [`Self::selection_finish_copy`]: extracts the dragged text, shows the
    /// "copied N chars" toast and returns `Some(text)` for the caller's clipboard
    /// path (OSC 52 / native). The span stays highlighted so the user sees what was
    /// copied; a later down elsewhere clears it. Returns `None` when nothing was
    /// selected — fail-open.
    #[must_use]
    pub fn input_selection_finish_copy(&mut self) -> Option<String> {
        if self
            .pending_host_input
            .as_ref()
            .is_some_and(host_input::PendingHostInputView::is_secret)
        {
            self.input_selection = None;
            self.input_selection_dragging = false;
            return None;
        }
        // The button released: the drag is over (the span stays highlighted).
        self.input_selection_dragging = false;
        let sel = self.input_selection?;
        if sel.is_empty() {
            return None;
        }
        let text = self.input_selection_text(&sel);
        if text.is_empty() {
            return None;
        }
        let count = text.chars().count();
        self.show_copy_toast(count);
        Some(text)
    }

    // ---- Ctrl+click → open URL / file (the in-app Cmd+click) --------------

    /// Resolve what a Ctrl+click at screen `(col, row)` would OPEN, without
    /// opening it: maps the cell through the same cached-row geometry the
    /// selection layer uses, rejoins the soft-wrapped logical line (so a URL
    /// folded across visual rows is seen whole), and asks [`crate::link`] for
    /// the token under the cursor. A URL is returned validated + trimmed; a
    /// path token is returned only when it resolves to an EXISTING file/dir
    /// (canonicalized, `~`/relative-to-workspace expanded). `None` = nothing
    /// openable under the cursor. Pure with respect to app state — split from
    /// [`Self::link_click_open`] so tests never spawn a real opener.
    #[must_use]
    pub fn link_target_at(&self, col: u16, row: u16) -> Option<String> {
        let point = self.map_mouse_point(col, row)?;
        let candidate = {
            let rows = self.transcript_rows.borrow();
            let wraps = self.transcript_row_wraps.borrow();
            let (line, off) = crate::link::logical_line_at(&rows, &wraps, point.0, point.1)?;
            crate::link::find_link(&line, off)?
        };
        match candidate {
            crate::link::LinkCandidate::Url(url) => Some(url),
            crate::link::LinkCandidate::Path(tok) => {
                crate::link::resolve_path(&tok, &self.project_root).map(|p| p.display().to_string())
            }
        }
    }

    /// Ctrl+click at screen `(col, row)`: open the URL / existing file under
    /// the cursor with the platform opener, spawned detached (all stdio null,
    /// reaped off-thread — see [`crate::link::spawn_opener`]). Arms
    /// [`Self::link_click_pending`] unconditionally so the rest of the mouse
    /// gesture (drag / up) never touches the selection layer. Affordance: one
    /// status note on success or on a failed spawn; a click that hits nothing
    /// openable is a SILENT no-op (no note spam). Fail-open by contract —
    /// nothing here can block the event loop.
    pub fn link_click_open(&mut self, col: u16, row: u16) {
        self.link_click_pending = true;
        let Some(target) = self.link_target_at(col, row) else {
            return;
        };
        let key = if crate::link::spawn_opener(&target).is_ok() {
            "tui.link.opened"
        } else {
            "tui.link.open_failed"
        };
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, key, &[&target]),
        );
    }

    /// Render the chat history to a plain-text transcript for the **scrollback
    /// handoff** on a clean exit: after UmaDev leaves the alternate screen (which
    /// has no native scrollback), this text is printed to the MAIN screen so the
    /// conversation survives the exit instead of vanishing with the alt buffer.
    ///
    /// Each turn is its speaker tag + body (the flat [`MessageBody::as_text`]
    /// rendering, so a tool call / diff collapses to its one-line form), separated
    /// by blank lines. Empty / whitespace-only turns are skipped. Pure +
    /// fail-open: an empty history yields `""` (the caller prints nothing).
    #[must_use]
    pub fn transcript_plaintext(&self) -> String {
        let mut out = String::new();
        for msg in &self.history {
            let body = msg.body();
            let body = body.trim_end();
            if body.trim().is_empty() {
                continue;
            }
            // A short, language-neutral speaker tag. The base's own output (Host)
            // carries no tag so it reads as the assistant's prose; the others are
            // marked so the user can tell who said what in the scrollback.
            let tag = match msg.role {
                ChatRole::You => "› ",
                ChatRole::UmaDev => "UmaDev: ",
                ChatRole::Gate => "GATE: ",
                ChatRole::System => "· ",
                ChatRole::Error => "! ",
                ChatRole::Host => "",
            };
            if !out.is_empty() {
                out.push('\n');
            }
            // Tag only the first line; a multi-line body keeps its own line breaks.
            let mut lines = body.lines();
            if let Some(first) = lines.next() {
                out.push_str(tag);
                out.push_str(first);
                out.push('\n');
                for line in lines {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out
    }

    /// Build the compact exit footer shown on a clean quit when the full
    /// scrollback handoff is disabled (the default). At most two short lines:
    ///
    /// 1. A "session saved — run `umadev` to resume" hint.
    /// 2. When this workspace holds resumable artifacts (a persisted workflow
    ///    state, or an `output/` directory with content), a second line that
    ///    points the user at them.
    ///
    /// Returns `None` when there is nothing worth resuming (an empty
    /// conversation), so the caller leaves a fully clean terminal — mirroring the
    /// scrollback handoff's empty-history no-op. Pure + fail-open: the artifact
    /// probe swallows IO errors and never panics, and all strings come from the
    /// i18n catalog.
    #[must_use]
    pub fn exit_footer_text(&self) -> Option<String> {
        // No conversation → nothing to resume → a fully clean exit.
        if self.transcript_plaintext().trim().is_empty() {
            return None;
        }
        let mut footer = umadev_i18n::t(self.lang, "exit.footer.saved").to_string();
        if let Some(path) = self.resumable_artifact_path() {
            footer.push('\n');
            footer.push_str(&umadev_i18n::tf(
                self.lang,
                "exit.footer.artifacts",
                &[path],
            ));
        }
        Some(footer)
    }

    /// The workspace-relative location the exit footer points the user at, or
    /// `None` when this session left no resumable artifacts. Prefers user-facing
    /// deliverables under `output/`; falls back to the `.umadev/` workflow state a
    /// prior `/run` persisted. Read-only + fail-open: an unreadable or absent path
    /// just yields `None`, so the footer never fabricates a location that does not
    /// exist.
    fn resumable_artifact_path(&self) -> Option<&'static str> {
        let output = self.project_root.join("output");
        if std::fs::read_dir(&output).is_ok_and(|mut entries| entries.next().is_some()) {
            return Some("output/");
        }
        if umadev_agent::read_workflow_state(&self.project_root).is_some() {
            return Some(".umadev/");
        }
        None
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

    /// Char index of the GRAPHEME-cluster boundary immediately to the LEFT of
    /// `char_pos` — i.e. the start of the user-perceived glyph that ends at
    /// `char_pos`. A ZWJ emoji (👨‍👩‍👧), a flag / skin-tone sequence, or a
    /// base char + combining marks is several codepoints but one cluster, so
    /// stepping the caret / backspace over it must move by the whole cluster,
    /// not one codepoint (which would split the glyph and corrupt the render).
    /// Fail-open: returns `0` when already at the start, and a cluster that
    /// straddles a mid-cluster caret resolves to its own start (de-corrupting
    /// the caret) rather than panicking.
    fn prev_grapheme(&self, char_pos: usize) -> usize {
        if char_pos == 0 {
            return 0;
        }
        // Boundaries appear in increasing order as `acc`; the last one strictly
        // below `char_pos` is the target.
        let mut boundary = 0;
        let mut acc = 0;
        for g in self.input.graphemes(true) {
            if acc >= char_pos {
                break;
            }
            boundary = acc;
            acc += g.chars().count();
        }
        boundary
    }

    /// Char index of the GRAPHEME-cluster boundary immediately to the RIGHT of
    /// `char_pos` — the end of the cluster that starts at `char_pos`. Mirror of
    /// [`Self::prev_grapheme`] for the forward caret / forward-delete. Fail-open:
    /// clamps to the buffer length.
    fn next_grapheme(&self, char_pos: usize) -> usize {
        let len = self.input_len();
        if char_pos >= len {
            return len;
        }
        let mut acc = 0;
        for g in self.input.graphemes(true) {
            acc += g.chars().count();
            if acc > char_pos {
                return acc;
            }
        }
        len
    }

    /// Insert one character at the cursor and advance.
    pub fn insert_at_cursor(&mut self, c: char) {
        if self.input_len() >= INPUT_CAP {
            return;
        }
        // Chip-aware: typing/overtyping STRICTLY interior to a `[图片 N]` /
        // `[粘贴 N 行]` chip splits the token so `expand_attachments` can no longer
        // match it — the image/paste would be silently dropped on submit (the
        // delete paths are chip-aware; insert was the gap). Decide on the pre-insert
        // caret, then reconcile after the splice so the now-broken chip drops its
        // backing ref instead of mis-submitting the corrupted literal.
        let split_chip = (!self.attachments.is_empty() || !self.text_stash.is_empty())
            && self.cursor_inside_chip(self.input_cursor);
        self.snapshot_for_undo();
        let pos = self.byte_index(self.input_cursor);
        self.input.insert(pos, c);
        self.input_cursor += 1;
        if split_chip {
            self.reconcile_attachments();
        }
        self.reset_typeahead_after_edit();
    }

    /// Any edit that changes `input` invalidates the live slash / `@` candidate
    /// lists. Re-open a dismissed mention popover because the token just changed;
    /// pure cursor movement intentionally does not call this.
    fn reset_typeahead_after_edit(&mut self) {
        self.palette_selected = 0;
        self.mention_selected = 0;
        self.mention_dismissed = false;
    }

    /// Insert a whole string at the cursor (bracketed paste / CJK IME commit).
    /// Newlines are kept (multi-line prompts); other control characters are
    /// dropped so a pasted terminal escape sequence can't corrupt the buffer or
    /// the render. Honors the internal input limit and advances the char-cursor by the
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
            // Keep newlines AND tabs; drop every other control char. Dropping
            // `\t` silently stripped the indentation out of pasted tab-indented
            // code (a real "my paste lost all its tabs" bug).
            if c != '\n' && c != '\t' && c.is_control() {
                continue;
            }
            buf.push(c);
            added += 1;
        }
        if !buf.is_empty() {
            // Chip-aware (see `insert_at_cursor`): a paste landing STRICTLY interior
            // to a `[图片 N]` / `[粘贴 N 行]` chip splits its token, so reconcile after
            // the splice to drop the now-broken chip's backing ref rather than
            // silently mis-submitting the corrupted literal.
            let split_chip = (!self.attachments.is_empty() || !self.text_stash.is_empty())
                && self.cursor_inside_chip(self.input_cursor);
            self.snapshot_for_undo();
            let pos = self.byte_index(self.input_cursor);
            self.input.insert_str(pos, &buf);
            self.input_cursor += added;
            if split_chip {
                self.reconcile_attachments();
            }
        }
        self.input_history_idx = None;
        self.reset_typeahead_after_edit();
    }

    /// Delete the GRAPHEME CLUSTER before the cursor (Backspace). Steps over a
    /// whole ZWJ emoji / flag / base+combining glyph as one unit instead of
    /// peeling off a single codepoint and leaving a mojibake half-glyph.
    pub fn backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        self.snapshot_for_undo();
        // Chip-aware: a `[图片 N]` / `[粘贴 N 行]` chip flush against the caret is
        // removed as ONE unit (and its backing image / paste ref dropped via
        // `reconcile_attachments`) instead of being peeled one char at a time —
        // which left a corrupt partial token (`[图片 1`) that looked like Backspace
        // "did nothing" and orphaned the attachment so it mis-expanded on submit.
        if let Some((start_char, _)) = self.chip_span_ending_at(self.input_cursor) {
            self.remove_char_range(start_char, self.input_cursor);
            self.input_cursor = start_char;
            self.reconcile_attachments();
        } else {
            let start_char = self.prev_grapheme(self.input_cursor);
            self.remove_char_range(start_char, self.input_cursor);
            self.input_cursor = start_char;
        }
        self.reset_typeahead_after_edit();
    }

    /// Remove the half-open char range `[start_char, end_char)` from `input`.
    /// Char→byte conversion is done here so callers stay in char units. Fail-open:
    /// a reversed or out-of-range pair is clamped to a no-op.
    fn remove_char_range(&mut self, start_char: usize, end_char: usize) {
        if start_char >= end_char {
            return;
        }
        let start = self.byte_index(start_char);
        let end = self.byte_index(end_char);
        self.input.replace_range(start..end, "");
    }

    /// Delete the GRAPHEME CLUSTER at the cursor (forward Delete) — the mirror of
    /// [`Self::backspace`], removing the whole glyph to the right as one unit.
    pub fn forward_delete(&mut self) {
        if self.input_cursor >= self.input_len() {
            return;
        }
        self.snapshot_for_undo();
        // Chip-aware mirror of `backspace`: a chip starting at the caret is removed
        // whole (and its backing ref dropped) rather than one corrupting char.
        if let Some((_, end_char)) = self.chip_span_starting_at(self.input_cursor) {
            self.remove_char_range(self.input_cursor, end_char);
            self.reconcile_attachments();
        } else {
            let end_char = self.next_grapheme(self.input_cursor);
            self.remove_char_range(self.input_cursor, end_char);
        }
        self.reset_typeahead_after_edit();
    }

    /// Delete from the cursor back to the start of the line (Ctrl+U). The removed
    /// text is PUSHED to the kill-ring (recoverable with Ctrl+Y), not destroyed.
    pub fn delete_to_line_start(&mut self) {
        let start = self.input[..self.byte_index(self.input_cursor)]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        let end = self.byte_index(self.input_cursor);
        if start == end {
            return;
        }
        let start_char = self.input[..start].chars().count();
        let killed = self.input[start..end].to_string();
        self.snapshot_for_undo();
        self.input.replace_range(start..end, "");
        self.input_cursor = start_char;
        // A line-kill can take whole chips with it — drop any now-orphaned ref so
        // a removed chip never silently mis-expands on submit.
        self.reconcile_attachments();
        self.reset_typeahead_after_edit();
        self.push_kill(&killed, KillDir::Backward);
    }

    /// Delete from the cursor to the end of the line (Ctrl+K). The removed text
    /// is PUSHED to the kill-ring (recoverable with Ctrl+Y), not destroyed.
    pub fn delete_to_line_end(&mut self) {
        let from = self.byte_index(self.input_cursor);
        let end = self.input[from..]
            .find('\n')
            .map_or(self.input.len(), |i| from + i);
        if from == end {
            return;
        }
        let killed = self.input[from..end].to_string();
        self.snapshot_for_undo();
        self.input.replace_range(from..end, "");
        // Mirror of `delete_to_line_start`: a kill-to-EOL may swallow whole chips,
        // so drop any orphaned backing ref.
        self.reconcile_attachments();
        self.reset_typeahead_after_edit();
        self.push_kill(&killed, KillDir::Forward);
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

    /// Delete the word before the cursor (Ctrl+W / Alt+Backspace). The removed
    /// text is PUSHED to the kill-ring (recoverable with Ctrl+Y), not destroyed.
    pub fn delete_word_back(&mut self) {
        // A chip flush against the caret is one word-unit: kill the WHOLE chip
        // (and drop its backing ref) so Ctrl+W never bisects `[图片 1]` on its
        // inner space and orphans the attachment.
        let c = if let Some((start_char, _)) = self.chip_span_ending_at(self.input_cursor) {
            start_char
        } else {
            self.prev_word_boundary()
        };
        if c == self.input_cursor {
            return;
        }
        let start = self.byte_index(c);
        let end = self.byte_index(self.input_cursor);
        let killed = self.input[start..end].to_string();
        self.snapshot_for_undo();
        self.input.replace_range(start..end, "");
        self.input_cursor = c;
        self.reconcile_attachments();
        self.reset_typeahead_after_edit();
        self.push_kill(&killed, KillDir::Backward);
    }

    /// Push freshly-killed text onto the kill-ring, coalescing a consecutive
    /// same-direction kill into the front entry (forward APPENDS, backward
    /// PREPENDS) so repeated Ctrl+K / Ctrl+U build one yank-able chunk. A
    /// direction change pushes a new entry; the ring is capped at
    /// [`KILL_RING_CAP`]. Killing also closes any open yank-pop window.
    fn push_kill(&mut self, killed: &str, dir: KillDir) {
        if killed.is_empty() {
            return;
        }
        self.yank_span = None;
        if self.last_kill == Some(dir) {
            if let Some(front) = self.kill_ring.front_mut() {
                match dir {
                    KillDir::Forward => front.push_str(killed),
                    KillDir::Backward => front.insert_str(0, killed),
                }
            } else {
                self.kill_ring.push_front(killed.to_string());
            }
        } else {
            self.kill_ring.push_front(killed.to_string());
            while self.kill_ring.len() > KILL_RING_CAP {
                self.kill_ring.pop_back();
            }
        }
        self.last_kill = Some(dir);
    }

    /// Reset the kill-coalescing + yank-pop windows. Called for every key that is
    /// neither a kill (Ctrl+U/K/W) nor a yank (Ctrl+Y / Alt+Y), so a kill after a
    /// cursor move starts a fresh ring entry and Alt+Y is valid only directly
    /// after a yank. Leaves the ring CONTENTS intact.
    fn reset_kill_yank(&mut self) {
        self.last_kill = None;
        self.yank_span = None;
    }

    /// Ctrl+Y — yank: insert the front kill-ring entry at the caret and remember
    /// the inserted span so an immediately-following Alt+Y can yank-pop it.
    /// No-op (fail-open) on an empty ring.
    pub fn yank(&mut self) {
        let Some(front) = self.kill_ring.front().cloned() else {
            return;
        };
        let start = self.input_cursor;
        // `insert_str_at_cursor` snapshots for undo + honours `INPUT_CAP`; the
        // actual chars inserted is the cursor delta (a full box inserts nothing).
        self.insert_str_at_cursor(&front);
        let added = self.input_cursor.saturating_sub(start);
        self.yank_span = (added > 0).then_some((start, added));
        self.yank_ring_idx = 0;
        self.last_kill = None;
    }

    /// Alt+Y — yank-pop: cycle to the next-older kill-ring entry and REPLACE the
    /// span the previous yank / yank-pop inserted. Valid ONLY immediately after a
    /// yank (the span is recorded); otherwise a no-op. No-op with fewer than two
    /// ring entries (nothing to cycle to).
    pub fn yank_pop(&mut self) {
        let Some((start, len)) = self.yank_span else {
            return;
        };
        if self.kill_ring.len() < 2 {
            return;
        }
        self.yank_ring_idx = (self.yank_ring_idx + 1) % self.kill_ring.len();
        let Some(replacement) = self.kill_ring.get(self.yank_ring_idx).cloned() else {
            return;
        };
        self.snapshot_for_undo();
        let bstart = self.byte_index(start);
        let bend = self.byte_index(start + len);
        self.input.replace_range(bstart..bend, &replacement);
        let new_len = replacement.chars().count();
        self.input_cursor = start + new_len;
        self.yank_span = Some((start, new_len));
        self.palette_selected = 0;
        self.last_kill = None;
    }

    /// Snapshot the current input + caret onto the undo stack BEFORE a mutating
    /// edit, coalescing a rapid burst into one step: if the previous snapshot was
    /// pushed within [`UNDO_COALESCE`], this edit folds into it (no new step).
    /// Every edit — coalesced or not — truncates the redo branch, so a fresh edit
    /// after an undo forks a clean future. Fail-open: pure bookkeeping, no panics.
    fn snapshot_for_undo(&mut self) {
        // Any fresh edit invalidates the redo branch.
        self.redo_stack.clear();
        let now = std::time::Instant::now();
        let coalesce = self
            .last_snapshot_at
            .is_some_and(|t| now.duration_since(t) < UNDO_COALESCE);
        self.last_snapshot_at = Some(now);
        if coalesce {
            return;
        }
        self.undo_stack.push(EditSnapshot {
            text: self.input.clone(),
            cursor: self.input_cursor,
        });
        if self.undo_stack.len() > UNDO_CAP {
            self.undo_stack.remove(0);
        }
    }

    /// Ctrl+Z — undo: restore the previous input snapshot, saving the current
    /// state to the redo stack first. No-op (fail-open) when there is nothing to
    /// undo.
    pub fn undo(&mut self) {
        let Some(prev) = self.undo_stack.pop() else {
            return;
        };
        self.redo_stack.push(EditSnapshot {
            text: self.input.clone(),
            cursor: self.input_cursor,
        });
        if self.redo_stack.len() > UNDO_CAP {
            self.redo_stack.remove(0);
        }
        self.restore_snapshot(prev);
    }

    /// Alt+Z — redo: replay the most recently undone snapshot, saving the current
    /// state back to the undo stack. No-op when the redo branch is empty.
    pub fn redo(&mut self) {
        let Some(next) = self.redo_stack.pop() else {
            return;
        };
        self.undo_stack.push(EditSnapshot {
            text: self.input.clone(),
            cursor: self.input_cursor,
        });
        if self.undo_stack.len() > UNDO_CAP {
            self.undo_stack.remove(0);
        }
        self.restore_snapshot(next);
    }

    /// Apply a snapshot to the live input + caret and settle the surrounding edit
    /// state: the next edit opens a clean undo step (`last_snapshot_at` cleared),
    /// history-recall is exited, and the popover highlights reset so the slash /
    /// @-mention popovers re-evaluate against the restored text. The caret is
    /// clamped to the restored length (fail-open).
    fn restore_snapshot(&mut self, snap: EditSnapshot) {
        self.input = snap.text;
        self.input_cursor = snap.cursor.min(self.input_len());
        self.last_snapshot_at = None;
        self.input_history_idx = None;
        self.reset_typeahead_after_edit();
    }

    /// Move the caret by `delta` GRAPHEME CLUSTERS, clamped to `[0, len]`. `delta`
    /// is a step count: each step snaps to the next/previous cluster boundary so a
    /// single ←/→ over a ZWJ emoji or a base+combining glyph moves past the whole
    /// glyph as one unit (was: one codepoint, which split the cluster). Fail-open:
    /// saturates at the buffer ends.
    pub fn move_cursor(&mut self, delta: isize) {
        let steps = delta.unsigned_abs();
        if delta < 0 {
            for _ in 0..steps {
                if self.input_cursor == 0 {
                    break;
                }
                self.input_cursor = self.prev_grapheme(self.input_cursor);
            }
        } else {
            let len = self.input_len();
            for _ in 0..steps {
                if self.input_cursor >= len {
                    break;
                }
                self.input_cursor = self.next_grapheme(self.input_cursor);
            }
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

    /// P3 — mark the terminal **contaminated** (see
    /// [`Self::terminal_contaminated`]): an out-of-band write or a discrete
    /// layout transition invalidated what is on the real screen, so the event
    /// loop must force ONE full clear + repaint on the next frame. Idempotent;
    /// `&self` (interior-mutable `Cell`) so the pure `&App` renderer, the scroll
    /// helpers, and the event loop's select arms can all raise it.
    pub fn contaminate_terminal(&self) {
        self.terminal_contaminated.set(true);
    }

    /// P3 — drain the contamination flag. The event loop calls this once per
    /// iteration and folds `true` into its `force_full_repaint` gate, which
    /// `terminal.clear()`s (a real `Clear(All)` + a ratatui back-buffer reset)
    /// so the next draw repaints EVERY cell and no stale row survives. Drains in
    /// one shot (a second call returns `false`): exactly one healing repaint per
    /// contamination, then the cheap incremental diff resumes (on a non-sync
    /// terminal — under confirmed sync output every frame is a full atomic
    /// repaint regardless, P0). Returns `false` in the steady state.
    #[must_use]
    pub fn take_terminal_contaminated(&self) -> bool {
        self.terminal_contaminated.replace(false)
    }

    /// Request a FULL clear + redraw on the next frame — an alias of
    /// [`Self::contaminate_terminal`] kept for the height-changing operations
    /// that can otherwise leave stale overlapping rows on the Windows console (a
    /// multi-line history recall, `/clear`).
    pub fn request_full_repaint(&self) {
        self.contaminate_terminal();
    }

    /// Request a FULL clear + redraw on the next frame because the TRANSCRIPT
    /// reflowed / re-based / scrolled (see
    /// the crate's internal transcript-reflow detector) — an alias of
    /// [`Self::contaminate_terminal`] kept for the renderer / scroll callers.
    pub fn request_transcript_repaint(&self) {
        self.contaminate_terminal();
    }

    /// Drain a pending transcript-repaint request — an alias of
    /// [`Self::take_terminal_contaminated`] (one shared contamination flag).
    #[must_use]
    pub fn take_transcript_repaint(&self) -> bool {
        self.take_terminal_contaminated()
    }

    /// The rendered input-box height (clamped visible rows + underline + meta) at
    /// the text width the renderer last published. Mirrors
    /// [`crate::ui::input_block_rows`] so a height-changing edit can decide whether
    /// the prompt actually grew/shrank (the clamp means recalling a 3-line vs a
    /// 10-line entry both cap at the same box height → no needless repaint).
    pub(crate) fn input_block_height(&self) -> u16 {
        crate::ui::input_block_rows(&self.rendered_input(), self.input_text_cols.get())
    }

    /// Clear the input buffer + reset cursor + history-recall index.
    pub fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.input_history_idx = None;
        self.input_history_draft = None;
        self.attachments.clear();
        self.file_attachments.clear();
        self.text_stash.clear();
        self.mention_selected = 0;
        self.mention_dismissed = false;
    }

    /// The chip token shown in the input box for image attachment `n` (1-based),
    /// e.g. `[图片 1]`. The path remains only in the typed backing vector.
    fn image_chip(&self, n: usize) -> String {
        format!("[{} {n}]", umadev_i18n::t(self.lang, "attach.image"))
    }

    /// Visible token for a generic file selected through the `@` picker.
    fn file_chip(&self, n: usize) -> String {
        format!("[{} {n}]", umadev_i18n::t(self.lang, "attach.file"))
    }

    /// Line count used in a large-paste chip label. `lines()` ignores a trailing
    /// newline, so a paste ending in `\n` isn't undercounted by one; at least `1`
    /// (a chip is never `[粘贴 0 行]`).
    fn paste_line_count(text: &str) -> usize {
        text.lines().count().max(1)
    }

    /// Normalize text at the paste boundary.
    ///
    /// Windows Terminal / ConPTY can deliver bracketed paste newlines as bare
    /// `\r`; without this, the insert filter drops them as control chars and a
    /// multi-line paste collapses into one line. ANSI control strings are removed
    /// as whole sequences rather than only dropping the ESC byte, so pasted
    /// colored terminal output does not leak `[31m` fragments into the prompt.
    fn normalize_paste_text(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b'\r' => {
                    out.push('\n');
                    i += 1;
                    if bytes.get(i) == Some(&b'\n') {
                        i += 1;
                    }
                }
                0x1b => {
                    i = Self::skip_paste_escape(bytes, i);
                }
                _ => {
                    if !text.is_char_boundary(i) {
                        i += 1;
                        continue;
                    }
                    let Some(ch) = text[i..].chars().next() else {
                        break;
                    };
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        out
    }

    fn skip_paste_escape(bytes: &[u8], esc: usize) -> usize {
        let Some(kind) = bytes.get(esc + 1).copied() else {
            return esc + 1;
        };
        match kind {
            b'[' => Self::skip_paste_csi(bytes, esc + 2),
            b']' | b'P' | b'_' => Self::skip_paste_string(bytes, esc + 2),
            _ => (esc + 2).min(bytes.len()),
        }
    }

    fn skip_paste_csi(bytes: &[u8], mut i: usize) -> usize {
        while i < bytes.len() {
            let b = bytes[i];
            i += 1;
            if (0x40..=0x7e).contains(&b) {
                break;
            }
        }
        i
    }

    fn skip_paste_string(bytes: &[u8], mut i: usize) -> usize {
        while i < bytes.len() {
            if bytes[i] == 0x07 {
                return i + 1;
            }
            if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                return i + 2;
            }
            i += 1;
        }
        i
    }

    /// The chip token shown in the input box for a stashed large paste, e.g.
    /// `[粘贴 42 行]` / `[pasted 42 lines]`. Derived purely from the stashed
    /// text's line count, so the same definition recomputes the exact token on
    /// expand — keeping insert and expand in lockstep (mirrors `image_chip`).
    fn text_chip(&self, text: &str) -> String {
        let n = Self::paste_line_count(text);
        format!(
            "[{}]",
            umadev_i18n::tf(self.lang, "attach.paste", &[&n.to_string()])
        )
    }

    /// Handle a bracketed-paste payload. If it is one (or several newline-separated)
    /// IMAGE file path(s) — how every terminal delivers a dragged-in image — attach
    /// each as an `[图片 N]` chip; otherwise insert the text verbatim (the common
    /// case). A path-shaped image that fails validation is rejected without being
    /// echoed as prompt text; ordinary prose that merely mentions PNG stays text.
    pub fn handle_paste(&mut self, text: &str) {
        // A paste is an edit — close the kill-coalesce + yank-pop windows so a
        // following kill starts fresh and Alt+Y isn't mistaken for valid.
        self.reset_kill_yank();
        let text = Self::normalize_paste_text(text);
        let lines: Vec<&str> = text.trim().lines().collect();
        let all_images = !lines.is_empty()
            && lines
                .iter()
                .all(|l| is_image_path(&unquote_unescape(l.trim())));
        if all_images {
            let mut any = false;
            for l in &lines {
                let p = unquote_unescape(l.trim());
                // Reserve room for the WHOLE chip BEFORE attaching. Near INPUT_CAP
                // `insert_str_at_cursor` would insert nothing (or a partial token),
                // leaving the pushed attachment with no intact `[图片 N]` chip in the
                // buffer — so `expand_attachments` finds no chip and SILENTLY DROPS
                // the image on submit (an orphaned attachment). If the chip can't fit
                // whole, skip this image rather than orphan it (the box is at cap —
                // nothing more can be typed either).
                let need = self.image_chip(self.attachments.len() + 1).chars().count();
                if INPUT_CAP.saturating_sub(self.input_len()) < need {
                    self.push_attachment_rejection("attach.reason.input_full");
                    continue;
                }
                if let Some(n) = self.attach_image(&p) {
                    let chip = self.image_chip(n);
                    self.insert_str_at_cursor(&chip);
                    self.insert_str_at_cursor(" ");
                    any = true;
                }
            }
            // Belt-and-suspenders: re-sync `attachments` to the chips actually in the
            // buffer (mirrors every edit path), so no image can leave an orphaned
            // backing ref even if a chip failed to land whole. Fail-open bookkeeping.
            if any {
                self.reconcile_attachments();
            }
            // Every line was intentionally an image attachment. A rejected file
            // remains rejected; never fall through and paste its private path as
            // ordinary prompt text.
            return;
        }
        // A BULKY text paste (many lines or a huge single line) collapses to a
        // `[粘贴 N 行]` chip with the full text parked in `text_stash`, so it
        // doesn't flood the box into unscrollable noise; it expands back inline
        // on submit. Same proven chip+stash+expand pattern as images.
        let lines = Self::paste_line_count(&text);
        let chars = text.chars().count();
        if lines > PASTE_CHIP_MIN_LINES || chars > PASTE_CHIP_MIN_CHARS {
            let chip = self.text_chip(&text);
            // Reserve room for the WHOLE chip BEFORE stashing. Near INPUT_CAP,
            // `insert_str_at_cursor` would otherwise land a partial token (or
            // nothing), leaving `text_stash` with no intact chip to expand on
            // submit. Same invariant as image attachments: backing data exists
            // only when the visible token that references it landed whole.
            let need = chip.chars().count();
            if INPUT_CAP.saturating_sub(self.input_len()) < need {
                return;
            }
            self.text_stash.push(text.clone());
            self.insert_str_at_cursor(&chip);
            if INPUT_CAP.saturating_sub(self.input_len()) > 0 {
                self.insert_str_at_cursor(" ");
            }
            return;
        }
        // A small paste → verbatim (real text, the dominant case).
        self.insert_str_at_cursor(&text);
    }

    /// Validate a candidate image without ever echoing its local path. The host
    /// performs the authoritative MIME/identity recheck immediately before the
    /// protocol write; this early pass gives the user a useful error at paste time.
    fn attach_image(&mut self, path: &str) -> Option<usize> {
        let abs = match self.validate_attachment(path, true) {
            Ok(path) => path,
            Err(key) => {
                self.push_attachment_rejection(key);
                return None;
            }
        };
        self.attachments.push(abs);
        Some(self.attachments.len())
    }

    /// Add one generic file selected by the `@` picker.
    fn attach_file(&mut self, path: &std::path::Path) -> Option<usize> {
        let candidate = path.to_string_lossy();
        let abs = match self.validate_attachment(&candidate, false) {
            Ok(path) => path,
            Err(key) => {
                self.push_attachment_rejection(key);
                return None;
            }
        };
        self.file_attachments.push(abs);
        Some(self.file_attachments.len())
    }

    fn push_attachment_rejection(&mut self, reason_key: &str) {
        let reason = umadev_i18n::t(self.lang, reason_key);
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "attach.rejected", &[reason]),
        );
        self.refresh_status();
    }

    fn validate_attachment(
        &self,
        path: &str,
        require_image: bool,
    ) -> Result<std::path::PathBuf, &'static str> {
        if self.attachments.len() + self.file_attachments.len() >= MAX_TURN_ATTACHMENTS {
            return Err("attach.reason.count");
        }
        let path = std::path::Path::new(path);
        let before = std::fs::symlink_metadata(path).map_err(|_| "attach.reason.unavailable")?;
        if before.file_type().is_symlink() {
            return Err("attach.reason.symlink");
        }
        if !before.file_type().is_file() {
            return Err("attach.reason.regular");
        }
        if before.len() > MAX_ATTACHMENT_BYTES {
            return Err("attach.reason.size");
        }
        let prior_total = self
            .attachments
            .iter()
            .chain(self.file_attachments.iter())
            .filter_map(|item| std::fs::metadata(item).ok().map(|meta| meta.len()))
            .fold(0_u64, u64::saturating_add);
        if prior_total.saturating_add(before.len()) > MAX_TOTAL_ATTACHMENT_BYTES {
            return Err("attach.reason.total_size");
        }
        let canonical = std::fs::canonicalize(path).map_err(|_| "attach.reason.unavailable")?;
        if require_image {
            use std::io::Read as _;

            let mut file =
                std::fs::File::open(&canonical).map_err(|_| "attach.reason.unavailable")?;
            let mut header = [0_u8; 16];
            let read = file
                .read(&mut header)
                .map_err(|_| "attach.reason.unavailable")?;
            let bytes = &header[..read];
            if !supported_image_magic(bytes) || !image_extension_matches(path, bytes) {
                return Err("attach.reason.mime");
            }
        }
        Ok(canonical)
    }

    /// Path-free text view of the composed turn. Image/file chips stay visible
    /// labels; bulky paste chips expand to their original text.
    fn expand_attachments(&self, raw: &str) -> String {
        self.compose_submitted_turn(raw).text
    }

    /// Resolve the editor's visible chip stream into ordered typed blocks. This
    /// is called before `clear_input`, so every backing path/stash is snapshotted
    /// while its chip-to-vector index is still valid.
    fn compose_submitted_turn(&self, raw: &str) -> SubmittedTurn {
        #[derive(Clone)]
        enum Marker {
            Image(usize),
            File(usize),
            Paste(String),
        }

        let mut markers: Vec<(usize, usize, Marker)> = Vec::new();
        for index in 0..self.attachments.len() {
            let token = self.image_chip(index + 1);
            if let Some(start) = raw.find(&token) {
                markers.push((start, start + token.len(), Marker::Image(index)));
            }
        }
        for index in 0..self.file_attachments.len() {
            let token = self.file_chip(index + 1);
            if let Some(start) = raw.find(&token) {
                markers.push((start, start + token.len(), Marker::File(index)));
            }
        }
        let mut claimed_pastes = Vec::new();
        for stash in &self.text_stash {
            let token = self.text_chip(stash);
            let mut search = 0;
            while let Some(relative) = raw[search..].find(&token) {
                let start = search + relative;
                if claimed_pastes.contains(&start) {
                    search = start + token.len().max(1);
                    continue;
                }
                claimed_pastes.push(start);
                markers.push((start, start + token.len(), Marker::Paste(stash.clone())));
                break;
            }
        }
        markers.sort_by_key(|marker| marker.0);

        let content_end = submitted_content_end(raw, markers.last().map(|(_, end, _)| *end));

        let mut blocks = Vec::new();
        let mut display = String::with_capacity(content_end);
        let mut cursor = 0usize;
        for (start, end, marker) in markers {
            if start < cursor || end > content_end {
                continue;
            }
            append_text_block(&mut blocks, &raw[cursor..start]);
            display.push_str(&raw[cursor..start]);
            match marker {
                Marker::Image(index) => {
                    if let Some(path) = self.attachments.get(index) {
                        blocks.push(TurnInputBlock::Image { path: path.clone() });
                        display.push_str(&self.image_chip(index + 1));
                    }
                }
                Marker::File(index) => {
                    if let Some(path) = self.file_attachments.get(index) {
                        blocks.push(TurnInputBlock::File {
                            path: path.clone(),
                            mode: FileInputMode::MaterializeText,
                        });
                        display.push_str(&self.file_chip(index + 1));
                    }
                }
                Marker::Paste(text) => {
                    append_text_block(&mut blocks, &text);
                    display.push_str(&text);
                }
            }
            cursor = end;
        }
        append_text_block(&mut blocks, &raw[cursor..content_end]);
        display.push_str(&raw[cursor..content_end]);
        SubmittedTurn {
            text: display,
            input: TurnInput::new(blocks),
        }
    }

    /// Locate every composed-turn chip currently intact in `input`, returning each
    /// as a `(start_char, end_char)` half-open char range. Both chip families are
    /// detected exactly the way [`Self::expand_attachments`] resolves them on
    /// submit, so a chip the user can SEE is the same span this reports:
    ///
    /// - an `[图片 N]` image chip (the token embeds its unique 1-based number);
    /// - a `[粘贴 N 行]` large-paste chip, claimed in buffer order so two pastes
    ///   that share a line count (and thus a token) each map to their OWN position
    ///   instead of collapsing onto one.
    ///
    /// Pure read (fail-open): a chip whose token is no longer intact in the buffer
    /// — e.g. half-peeled by an older build — is simply omitted.
    fn chip_spans(&self) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        let mut push_token = |input: &str, byte_pos: usize, token: &str| {
            let start = input[..byte_pos].chars().count();
            let end = start + token.chars().count();
            spans.push((start, end));
        };
        // Image chips: unique token per attachment number.
        for i in 0..self.attachments.len() {
            let token = self.image_chip(i + 1);
            if let Some(byte_pos) = self.input.find(&token) {
                push_token(&self.input, byte_pos, &token);
            }
        }
        for i in 0..self.file_attachments.len() {
            let token = self.file_chip(i + 1);
            if let Some(byte_pos) = self.input.find(&token) {
                push_token(&self.input, byte_pos, &token);
            }
        }
        // Large-paste chips: claim the next UNCLAIMED occurrence of each stash
        // entry's token, in stash order, so same-line-count duplicates resolve to
        // distinct buffer positions (mirrors expand's sequential `find`).
        let mut claimed: Vec<usize> = Vec::new();
        for stash in &self.text_stash {
            let token = self.text_chip(stash);
            let mut search = 0;
            while let Some(rel) = self.input[search..].find(&token) {
                let pos = search + rel;
                if claimed.contains(&pos) {
                    search = pos + token.len().max(1);
                    continue;
                }
                claimed.push(pos);
                push_token(&self.input, pos, &token);
                break;
            }
        }
        spans
    }

    /// The chip span whose RIGHT edge sits exactly at char `cursor` — i.e. the
    /// chip immediately before the caret, the one a Backspace should swallow whole
    /// instead of peeling one corrupting char at a time. `None` when the caret
    /// isn't flush against a chip.
    fn chip_span_ending_at(&self, cursor: usize) -> Option<(usize, usize)> {
        self.chip_spans()
            .into_iter()
            .find(|&(_, end)| end == cursor)
    }

    /// The chip span whose LEFT edge sits exactly at char `cursor` — the chip
    /// immediately after the caret, the one a forward Delete should swallow whole.
    fn chip_span_starting_at(&self, cursor: usize) -> Option<(usize, usize)> {
        self.chip_spans()
            .into_iter()
            .find(|&(start, _)| start == cursor)
    }

    /// `true` when char `cursor` sits STRICTLY interior to a chip span — between
    /// its edges, where an insert/overtype would split the `[图片 N]` /
    /// `[粘贴 N 行]` token so [`Self::expand_attachments`] can no longer match it
    /// and the backing image/paste would be silently dropped on submit. The edges
    /// (`cursor == start` or `cursor == end`) are adjacent, not interior, and keep
    /// the token intact, so they return `false`.
    fn cursor_inside_chip(&self, cursor: usize) -> bool {
        self.chip_spans()
            .into_iter()
            .any(|(start, end)| start < cursor && cursor < end)
    }

    /// Re-sync `attachments` / `text_stash` to the chips that survive in `input`
    /// after an edit, so a removed chip never leaves an orphaned backing ref that
    /// would be silently dropped (image) or wrongly inlined (paste) on submit.
    ///
    /// Image chips are rebuilt in **buffer order** and the buffer is **renumbered**
    /// to a contiguous `1..=N`, keeping the `[图片 K]` ↔ `attachments[K-1]` coupling
    /// that [`Self::expand_attachments`] relies on intact even when a MIDDLE chip is
    /// deleted (a naive `Vec::remove` would shift every later index and submit the
    /// wrong path / drop one). Large-paste entries whose token no longer has an
    /// unclaimed occurrence are dropped, by buffer order. Fail-open: pure
    /// bookkeeping, never panics; the caret is clamped into the rebuilt buffer.
    fn reconcile_attachments(&mut self) {
        // ---- image chips: rebuild in buffer order + renumber to 1..=N ----
        if !self.attachments.is_empty() {
            // (byte_pos, old 1-based number) for every intact image chip present.
            let mut hits: Vec<(usize, usize)> = Vec::new();
            for k in 1..=self.attachments.len() {
                if let Some(p) = self.input.find(&self.image_chip(k)) {
                    hits.push((p, k));
                }
            }
            hits.sort_by_key(|&(p, _)| p);
            // Surviving paths in buffer order become the new contiguous Vec.
            let new_attachments: Vec<std::path::PathBuf> = hits
                .iter()
                .map(|&(_, k)| self.attachments[k - 1].clone())
                .collect();
            // Renumber the buffer tokens to 1..=len by rebuilding the string once
            // over the sorted, non-overlapping spans — collision-free regardless
            // of how old and new numbers overlap (an in-place rename could clash).
            let needs_rewrite = new_attachments.len() != self.attachments.len()
                || hits.iter().enumerate().any(|(j, &(_, k))| k != j + 1);
            if needs_rewrite {
                let mut edits: Vec<(usize, usize, String)> = hits
                    .iter()
                    .enumerate()
                    .map(|(j, &(p, k))| {
                        let old = self.image_chip(k);
                        (p, p + old.len(), self.image_chip(j + 1))
                    })
                    .collect();
                edits.sort_by_key(|e| e.0);
                let mut out = String::with_capacity(self.input.len());
                let mut last = 0;
                for (s, e, repl) in edits {
                    out.push_str(&self.input[last..s]);
                    out.push_str(&repl);
                    last = e;
                }
                out.push_str(&self.input[last..]);
                self.input = out;
            }
            self.attachments = new_attachments;
        }
        // ---- generic-file chips: same contiguous-index invariant ----
        if !self.file_attachments.is_empty() {
            let mut hits: Vec<(usize, usize)> = Vec::new();
            for k in 1..=self.file_attachments.len() {
                if let Some(position) = self.input.find(&self.file_chip(k)) {
                    hits.push((position, k));
                }
            }
            hits.sort_by_key(|&(position, _)| position);
            let new_files: Vec<std::path::PathBuf> = hits
                .iter()
                .map(|&(_, k)| self.file_attachments[k - 1].clone())
                .collect();
            let needs_rewrite = new_files.len() != self.file_attachments.len()
                || hits
                    .iter()
                    .enumerate()
                    .any(|(index, &(_, k))| k != index + 1);
            if needs_rewrite {
                let mut edits: Vec<(usize, usize, String)> = hits
                    .iter()
                    .enumerate()
                    .map(|(index, &(position, k))| {
                        let old = self.file_chip(k);
                        (position, position + old.len(), self.file_chip(index + 1))
                    })
                    .collect();
                edits.sort_by_key(|edit| edit.0);
                let mut out = String::with_capacity(self.input.len());
                let mut last = 0;
                for (start, end, replacement) in edits {
                    out.push_str(&self.input[last..start]);
                    out.push_str(&replacement);
                    last = end;
                }
                out.push_str(&self.input[last..]);
                self.input = out;
            }
            self.file_attachments = new_files;
        }
        // ---- large-paste chips: drop entries whose token vanished ----
        if !self.text_stash.is_empty() {
            let mut claimed: Vec<usize> = Vec::new();
            let mut keep: Vec<String> = Vec::new();
            for stash in &self.text_stash {
                let token = self.text_chip(stash);
                let mut search = 0;
                let mut found = false;
                while let Some(rel) = self.input[search..].find(&token) {
                    let pos = search + rel;
                    if claimed.contains(&pos) {
                        search = pos + token.len().max(1);
                        continue;
                    }
                    claimed.push(pos);
                    found = true;
                    break;
                }
                if found {
                    keep.push(stash.clone());
                }
            }
            self.text_stash = keep;
        }
        // The renumber may have shortened a multi-digit token before the caret;
        // clamp so the cursor can never index past the rebuilt buffer.
        self.input_cursor = self.input_cursor.min(self.input_len());
    }

    /// Push a submitted line onto the input-history ring. De-dups
    /// consecutive duplicates (typing the same thing twice doesn't
    /// double-pollute the ↑↓ recall). Also persists to disk so history
    /// survives across TUI sessions.
    pub fn remember_submission(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        // I9 — the user has now interacted, so we're past the "first-run" window:
        // the rotating example tip stops being offered above the idle placeholder.
        self.session_turns = self.session_turns.saturating_add(1);
        if self.input_history.back().map(String::as_str) == Some(text) {
            return;
        }
        self.input_history.push_back(text.to_string());
        while self.input_history.len() > INPUT_HISTORY_CAP {
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
        // Snapshot the box height BEFORE the recall: pulling a multi-line entry
        // into a one-row box grows the prompt and shifts the transcript above it,
        // which leaves stale overlapping rows on the Windows console. If the
        // rendered height changes, force a full clear+redraw (see
        // `request_full_repaint`).
        let before = self.input_block_height();
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
        if self.input_block_height() != before {
            self.request_full_repaint();
        }
    }

    /// Step forward through input history. At the most-recent entry,
    /// stepping forward once more clears the input (returns to fresh draft).
    pub fn input_history_forward(&mut self) {
        let Some(idx) = self.input_history_idx else {
            return;
        };
        // Same height-shift guard as `input_history_back`: stepping forward can
        // SHRINK the box (a multi-line entry → a shorter one / the empty draft),
        // which vacates rows the Windows console may leave as stale overlap.
        let before = self.input_block_height();
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
        if self.input_block_height() != before {
            self.request_full_repaint();
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
            "grok",
            &["grok-build"],
            None,
            CmdGroup::Worker,
            "tui.cmd.grok",
        ),
        Self::cmd(
            "kimi",
            &["kimi-code"],
            None,
            CmdGroup::Worker,
            "tui.cmd.kimi",
        ),
        Self::cmd(
            "offline",
            &[],
            None,
            CmdGroup::Worker,
            "tui.help.worker.offline",
        ),
        Self::cmd(
            "base",
            &[],
            Some("</command> [args]"),
            CmdGroup::Worker,
            "input.delivery.native",
        ),
        Self::cmd(
            "thinking",
            &[],
            Some("[on|off]"),
            CmdGroup::Worker,
            "tui.cmd.thinking",
        ),
        Self::cmd(
            "sandbox",
            &[],
            Some("[read-only|workspace-write|danger-full-access]"),
            CmdGroup::Worker,
            "tui.cmd.sandbox",
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
        Self::cmd(
            "tasks",
            &["task"],
            Some("[stop|resume]"),
            CmdGroup::Pipeline,
            "tui.cmd.tasks",
        ),
        Self::cmd(
            "processes",
            &["ps", "bg"],
            Some("[stop <id>]"),
            CmdGroup::Pipeline,
            "tui.cmd.processes",
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
        Self::cmd(
            "memory",
            &[],
            Some("[inventory|capture|recall|retention|export|forget|clear-cache]"),
            CmdGroup::Inspect,
            "tui.cmd.memory",
        ),
        Self::cmd("team", &[], None, CmdGroup::Inspect, "tui.cmd.team"),
        Self::cmd(
            "constitution",
            &["charter"],
            None,
            CmdGroup::Inspect,
            "tui.cmd.constitution",
        ),
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
            "theme",
            &[],
            Some("light|dark|auto"),
            CmdGroup::System,
            "tui.cmd.theme",
        ),
        Self::cmd("logs", &[], None, CmdGroup::System, "tui.cmd.logs"),
        Self::cmd(
            "questions",
            &[],
            Some("text|picker"),
            CmdGroup::System,
            "tui.cmd.questions",
        ),
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
    /// entry by canonical name OR any alias. `None` for an unknown verb.
    #[must_use]
    pub fn resolve_command(verb: &str) -> Option<&'static SlashCommand> {
        Self::COMMANDS
            .iter()
            .find(|c| c.name == verb || c.aliases.contains(&verb))
    }

    fn advertised_command_name(command: &SessionCommandInfo) -> Option<&str> {
        let name = command.name.trim();
        let name = name.strip_prefix('/').unwrap_or(name);
        (!name.is_empty() && !name.contains(char::is_whitespace)).then_some(name)
    }

    fn advertised_base_command(&self, verb: &str) -> bool {
        self.base_session_commands.iter().any(|command| {
            Self::advertised_command_name(command)
                .is_some_and(|name| name.eq_ignore_ascii_case(verb))
        })
    }

    /// Match the verbs prefixed by what comes after `/` in the current
    /// input. Empty input or non-slash input → empty list.
    ///
    /// Descriptions are localized for the active language; hidden commands are
    /// never suggested. Backend switching is intentionally registry-only, so a
    /// transport driver cannot become an undocumented TUI command.
    #[must_use]
    pub fn palette_matches(&self) -> Vec<PaletteEntry<'_>> {
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
        // I8 — rank each verb by an explicit exact→prefix→fuzzy tier, with the
        // fzf positional score as the WITHIN-tier order (`/dpl` → `deploy`). A
        // loose tier-2 fuzzy hit only kicks in at ≥2 typed chars, so a single
        // `/r` doesn't explode into every verb that merely contains an 'r'; an
        // exact/prefix command always sorts first.
        let fuzzy = typed.chars().count() >= 2;
        let rank = |verb: &str| -> Option<(u8, i32)> {
            let tier = if verb == typed {
                0u8
            } else if verb.starts_with(typed.as_str()) {
                1
            } else {
                2
            };
            if tier == 2 && !fuzzy {
                return None;
            }
            // The fzf score doubles as the subsequence existence test (None → no
            // match); prefix/exact tiers are always a subsequence, so they pass.
            fuzzy_score(&typed, verb).map(|s| (tier, s))
        };
        let mut out: Vec<(u8, i32, PaletteEntry<'_>)> = Self::COMMANDS
            .iter()
            .filter(|c| !c.hidden)
            .filter_map(|c| {
                rank(c.name).map(|(t, s)| {
                    (
                        t,
                        s,
                        PaletteEntry {
                            verb: c.name,
                            desc: umadev_i18n::t(self.lang, c.desc_key),
                            arg_hint: c.arg_hint,
                        },
                    )
                })
            })
            .collect();
        out.extend(self.base_session_commands.iter().filter_map(|command| {
            let name = Self::advertised_command_name(command)?;
            let normalized = name.to_ascii_lowercase();
            // A base command that collides with any UmaDev canonical command or
            // alias remains reachable through `/base /…`, but never shadows the
            // product command in autocomplete or direct dispatch.
            if Self::resolve_command(&normalized).is_some() {
                return None;
            }
            rank(&normalized).map(|(tier, score)| {
                (
                    tier,
                    score,
                    PaletteEntry {
                        verb: name,
                        desc: if command.description.trim().is_empty() {
                            "base command"
                        } else {
                            command.description.as_str()
                        },
                        arg_hint: command.input_hint.as_deref(),
                    },
                )
            })
        }));
        // Tier ascending (exact → prefix → fuzzy), then fzf score DESCENDING; a
        // stable sort keeps the canonical verb order within an equal (tier, score).
        out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
        out.into_iter().map(|(_, _, p)| p).collect()
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
        let verb = matches[selected].verb.to_string();
        drop(matches);
        self.input = format!("/{verb} ");
        self.input_cursor = self.input_len();
        self.palette_selected = 0;
    }

    fn try_arg_completion(&self) -> Option<String> {
        let input = self.input.trim_start();
        let (prefix, partial) = if let Some(rest) = input.strip_prefix("/design ") {
            ("/design ", rest.trim())
        } else {
            let rest = input.strip_prefix("/template ")?;
            ("/template ", rest.trim())
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

    /// I9 — the first-run example tip layered above the idle placeholder: a short,
    /// rotating "试试 …" / "Try …" example (trilingual) that teaches the prompt
    /// surface by demonstration. Shown ONLY at the very start of a session — an
    /// empty, idle box on which the user has not yet sent anything
    /// (`session_turns == 0`) — and naming a real recently-touched repo file when
    /// one is found, else a generic token.
    ///
    /// Returns `None` once the user has spoken, while the box has text, or when a
    /// turn / run is in flight or settled — so the tip vanishes the instant they
    /// start typing or working, never nagging. Rotation uses a SESSION-STABLE
    /// index (the persisted prompt-history depth, constant across the first-run
    /// window) so the tip never flickers within a session yet a returning user
    /// sees a different example each launch — no RNG (none is deterministic for
    /// tests here). Pure read plus a once-cached file lookup; safe every frame.
    #[must_use]
    pub(crate) fn first_run_example_tip(&self) -> Option<String> {
        // The rotating example templates (each takes one `{}` = the file).
        const TEMPLATES: [&str; 3] = [
            "input.example.refactor",
            "input.example.tests",
            "input.example.explain",
        ];
        if self.session_turns > 0 || !self.input.is_empty() || !self.is_idle_for_tip() {
            return None;
        }
        let idx = self.input_history.len() % TEMPLATES.len();
        let file = self
            .resolve_example_file()
            .unwrap_or_else(|| umadev_i18n::t(self.lang, "input.example.file_generic").to_string());
        Some(umadev_i18n::tf(self.lang, TEMPLATES[idx], &[&file]))
    }

    /// The idle-empty input placeholder pool: example requirements (echoing
    /// the welcome-banner examples) interleaved with command hints — the
    /// `Enter 提交` / `/help 查看全部命令` chips that used to sit permanently
    /// on the meta row now rotate here instead. i18n keys, resolved per
    /// locale; rotated by [`Self::idle_placeholder`].
    pub(crate) const IDLE_PLACEHOLDERS: [&'static str; 10] = [
        // Keep every entry distinct per locale — the rotation test asserts the
        // whole pool is reachable.
        "input.idle",
        "input.ph.dashboard",
        "input.ph.help",
        "input.ph.todo",
        "input.ph.plan",
        "input.ph.landing",
        "input.ph.design",
        "input.ph.fix",
        "input.ph.keys",
        "input.ph.blog",
    ];

    /// The rotating idle-empty placeholder shown in the input box when nothing
    /// is typed and nothing is in flight. Deterministic rotation: the index
    /// advances once per SUBMITTED prompt (session turns + the persisted
    /// prompt-history depth), never per frame — so the hint is stable while
    /// idle (no flicker) and rotates to the next pool entry after each send /
    /// each launch. Special states (gate / running / finished / aborted / the
    /// I9 first-run tip) are handled by the caller with priority; this is only
    /// the plain-idle fallback.
    #[must_use]
    pub(crate) fn idle_placeholder(&self) -> String {
        let idx = self.session_turns.wrapping_add(self.input_history.len())
            % Self::IDLE_PLACEHOLDERS.len();
        umadev_i18n::t(self.lang, Self::IDLE_PLACEHOLDERS[idx]).to_string()
    }

    /// True when nothing is in flight or settled — the same "idle" condition
    /// under which the input placeholder rotates through the idle pool (no
    /// open gate, not thinking, no tool running, no started / finished /
    /// aborted run). Gates the
    /// first-run example tip ([`Self::first_run_example_tip`]).
    fn is_idle_for_tip(&self) -> bool {
        self.active_gate.is_none()
            && !self.thinking
            && !self.tool_in_progress
            && !self.finished
            && !self.aborted
            && !self.run_started
    }

    /// I9 — cached lookup of the repo file named by the first-run example tip:
    /// the most recently modified source file under the project root, or `None`.
    /// The bounded FS walk runs at most once per session (interior-mutable cache)
    /// so re-rendering the tip every frame stays free. Fail-open via the walk.
    fn resolve_example_file(&self) -> Option<String> {
        if let Some(cached) = self.example_file.borrow().as_ref() {
            return cached.clone();
        }
        let chosen = most_recently_modified_source_file(&self.project_root);
        *self.example_file.borrow_mut() = Some(chosen.clone());
        chosen
    }

    /// The ranked `@`-mention candidates for the partial currently under the
    /// cursor: repo-relative paths filtered by prefix / subsequence (basename
    /// first, then full path), capped at the internal mention-match limit.
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
        // I8 — keep the basename-prefix → path-prefix → fuzzy tier, but order
        // WITHIN a tier by the fzf positional score (computed over the
        // ORIGINAL-case basename / path so camelCase boundaries survive). So
        // `@src/main.rs` ranks `src/main.rs` above an incidental subsequence hit.
        let mut ranked: Vec<(u8, std::cmp::Reverse<i32>, &String)> = Vec::new();
        for f in files {
            let base = f.rsplit('/').next().unwrap_or(f);
            let path_l = f.to_lowercase();
            let base_l = base.to_lowercase();
            let (tier, score) = if p.is_empty() {
                (0u8, 0)
            } else if base_l.starts_with(&p) {
                (0, fuzzy_score(&partial, base).unwrap_or(0))
            } else if path_l.starts_with(&p) {
                (1, fuzzy_score(&partial, f).unwrap_or(0))
            } else if fuzzy {
                // Score the basename first (the part a user usually targets),
                // falling back to the full path; the fzf score is the subsequence
                // test too, so a `None` on both is a real miss.
                match fuzzy_score(&partial, base).or_else(|| fuzzy_score(&partial, f)) {
                    Some(s) => (2, s),
                    None => continue,
                }
            } else {
                continue;
            };
            ranked.push((tier, std::cmp::Reverse(score), f));
        }
        // Tier ascending, then fzf score descending; a stable sort keeps the
        // alphabetical file order within an equal (tier, score).
        ranked.sort_by_key(|(tier, score, _)| (*tier, *score));
        ranked
            .into_iter()
            .take(MENTION_MATCH_CAP)
            .map(|(_, _, f)| f.clone())
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

    /// Select the highlighted `@` candidate as a typed file attachment. The
    /// editor receives a stable `[File N]` chip rather than an `@path`, so paths
    /// containing spaces/CJK remain unambiguous and no local path enters model
    /// text or transcript history.
    pub fn accept_mention(&mut self) {
        let matches = self.mention_matches();
        if matches.is_empty() {
            return;
        }
        let Some((at_char, _)) = self.mention_token() else {
            return;
        };
        let sel = self.mention_selected.min(matches.len() - 1);
        let selected = self.project_root.join(&matches[sel]);
        let Some(number) = self.attach_file(&selected) else {
            return;
        };
        let chars: Vec<char> = self.input.chars().collect();
        let mut end = self.input_cursor.min(chars.len());
        while end < chars.len() && is_mention_char(chars[end]) {
            end += 1;
        }
        let start_b = self.byte_index(at_char);
        let end_b = self.byte_index(end);
        let replacement = format!("{} ", self.file_chip(number));
        let removed = end.saturating_sub(at_char);
        if self
            .input_len()
            .saturating_sub(removed)
            .saturating_add(replacement.chars().count())
            > INPUT_CAP
        {
            self.file_attachments.pop();
            self.push_attachment_rejection("attach.reason.input_full");
            return;
        }
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
    /// the posted plan. Each `PlanPosted` summary is `id · title (seat)`; we keep
    /// the id + title and seed each step with the status the event carries —
    /// all-`pending` for a fresh plan, the persisted truth on a cross-session
    /// RESUME re-post (already-`done` steps stay checked, a `blocked` step stays
    /// `[!]`, and the done/total header counts reflect reality instead of
    /// resetting to 0/N). A missing/short `statuses` falls back to `pending`
    /// per step (fail-open). The panel (rendered above the prompt) then ticks
    /// off live via `PlanStepStatus`, replacing the frozen 0/9 dot bar on the
    /// director path. A one-line "posted N steps" memo also lands in the
    /// transcript so scrollback records it.
    fn apply_plan_posted(
        &mut self,
        steps: &[String],
        statuses: &[String],
        _done: usize,
        total: usize,
    ) {
        // **Scenario B — a genuine new working turn on the director/chat path.** A
        // posted plan is the reliable "a build is live" signal here (this path emits
        // no `PipelineStarted` / `PhaseStarted`), so any TERMINAL flag left over from
        // a PRIOR block is stale. `run_state` already lets `thinking` win, but
        // clearing the raw flags — and the per-run degrade latch — keeps every
        // surface that reads them honest (no `[aborted]` / `[degraded]` over a turn
        // that is demonstrably working).
        self.clear_stale_terminal_on_live_turn();
        self.run_degraded_seen = false;
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
                    status: statuses
                        .get(i)
                        .map_or_else(|| "pending".to_string(), Clone::clone),
                    seat: parse_seat(summary),
                }
            })
            .collect();
        // A fresh plan un-collapses the panel so the first plan is always seen,
        // and seals any open review round (a re-plan starts a clean review cycle).
        // A re-plan also starts a fresh handoff timeline.
        self.plan_collapsed = false;
        // A genuine plan post (fresh build OR a `/continue` resume re-post) is a
        // LIVE plan — it fully replaces `plan_steps` above, so any prior FROZEN
        // interrupted panel is gone and its static header must not linger.
        self.plan_frozen = false;
        self.critic_round_open = false;
        self.handoffs.clear();
        // The director path posts a plan WITHOUT a `PipelineStarted` (it set
        // `agentic_in_flight` directly in the event loop), so a posted plan is the
        // reliable "a build is live" signal here — ensure a task exists and seed
        // its progress. Idempotent: a task already registered for this run is
        // reused, never duplicated.
        self.register_run_task(&self.requirement.clone());
        self.sync_active_task_progress();
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
        // A ticking plan step is a live director turn (e.g. a `/continue` resume
        // may drive steps without re-posting the plan) — clear any stale terminal
        // flag so a prior block's `[aborted]` / `[degraded]` / `[ok]` never reads
        // over a turn that is demonstrably working (Scenario B).
        self.clear_stale_terminal_on_live_turn();
        // A step transition means the director is between review bursts (a build
        // step is running, or a review step just (de)activated) — seal the open
        // review round so the NEXT verdict clears the prior round's seats instead
        // of mixing two rounds. A review burst itself emits no `PlanStepStatus`,
        // so this never splits a round mid-flight.
        self.critic_round_open = false;
        // A step that newly flips to `done` is a real handoff — capture the seat +
        // title BEFORE mutating the row so the timeline records who finished what.
        let mut handoff: Option<Handoff> = None;
        if let Some(row) = self.plan_steps.iter_mut().find(|s| s.id == id) {
            let newly_done = status == "done" && row.status != "done";
            row.status = status.to_string();
            if !title.trim().is_empty() {
                row.title = title.to_string();
            }
            if newly_done {
                handoff = Some(Handoff {
                    seat: row.seat.clone(),
                    title: row.title.clone(),
                });
            }
        } else {
            // An unknown id is appended (never dropped). Try to recover its seat
            // from a trailing `(seat)` on the title (fail-open: usually empty).
            let row_title = if title.trim().is_empty() {
                id.to_string()
            } else {
                title.to_string()
            };
            let seat = parse_seat(title);
            if status == "done" {
                handoff = Some(Handoff {
                    seat: seat.clone(),
                    title: row_title.clone(),
                });
            }
            self.plan_steps.push(PlanStepRow {
                id: id.to_string(),
                title: row_title,
                status: status.to_string(),
                seat,
            });
        }
        if let Some(h) = handoff {
            self.handoffs.push(h);
            // Bound the timeline — the oldest handoffs roll off the front.
            if self.handoffs.len() > HANDOFFS_CAP {
                let overflow = self.handoffs.len() - HANDOFFS_CAP;
                self.handoffs.drain(0..overflow);
            }
        }
        // Reflect the tick into the live registry task's X/Y progress.
        self.sync_active_task_progress();
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
        remediation: Vec<String>,
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
        self.push_critic_note(&seat, accepts, &blocking, &remediation);
        let row = CriticRow {
            seat,
            accepts,
            blocking,
            remediation,
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
    fn push_critic_note(
        &mut self,
        seat: &str,
        accepts: bool,
        blocking: &[String],
        remediation: &[String],
    ) {
        let mut body = if accepts {
            umadev_i18n::tf(self.lang, "plan.review.note.accept", &[seat])
        } else {
            umadev_i18n::tf(
                self.lang,
                "plan.review.note.block",
                &[seat, &blocking.len().max(1).to_string()],
            )
        };
        for (i, b) in blocking.iter().enumerate() {
            let item = b.trim();
            if item.is_empty() {
                continue;
            }
            body.push_str(&format!("\n  - {item}"));
            // The seat's per-blocker "how to fix" (index-aligned) rides directly
            // under the problem so the transcript shows a concrete next-step, not
            // just what is wrong. Fail-open: no matching suggestion → nothing extra.
            if let Some(fix) = remediation
                .get(i)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                body.push_str(&format!(
                    "\n    {}",
                    umadev_i18n::tf(self.lang, "plan.review.fix", &[fix])
                ));
            }
        }
        self.push(ChatRole::System, body);
    }

    /// Build the **live team roster** (Wave C): one [`RosterSeat`] per seat that
    /// owns a real plan step, in first-appearance order, with its aggregated live
    /// status and latest verdict. **Anti-theater is enforced here**: a seat is
    /// included **only** if it has at least one plan step (a real, machine-tracked
    /// unit of work) — a decorative full roster is never produced, only the seats
    /// actually convened this run. Returns empty when no plan is live (the panel
    /// then renders nothing extra). Fail-open: an unresolvable seat id is skipped.
    #[must_use]
    pub fn convened_roster(&self) -> Vec<RosterSeat> {
        let mut roster: Vec<RosterSeat> = Vec::new();
        for step in &self.plan_steps {
            // Anti-theater: only a seat with a real step joins the roster.
            if step.seat.is_empty() {
                continue;
            }
            // Aggregate this step's status into the seat's running tally.
            if let Some(existing) = roster.iter_mut().find(|r| r.role == step.seat) {
                existing.status = merge_seat_status(existing.status, &step.seat, &step.status);
            } else {
                roster.push(RosterSeat {
                    role: step.seat.clone(),
                    status: merge_seat_status(SeatStatus::Done, &step.seat, &step.status),
                    verdict: None,
                });
            }
        }
        // Fold in each seat's latest verdict (matched by canonical role). A
        // verdict from a seat with no plan step is NOT injected — anti-theater
        // keeps the roster to convened seats only (such verdicts still show in the
        // team-review panel).
        for c in &self.critic_verdicts {
            let canonical = umadev_agent::Seat::from_alias(&c.seat)
                .map_or(c.seat.clone(), |s| s.role_id().to_string());
            if let Some(seat) = roster.iter_mut().find(|r| r.role == canonical) {
                seat.verdict = Some((c.accepts, c.blocking.len()));
            }
        }
        roster
    }

    // ---- engine events ----------------------------------------------------

    fn configured_base_state(&self) -> (Option<String>, Option<u64>) {
        let Some(backend) = self
            .backend
            .as_deref()
            .filter(|backend| !backend.is_empty() && *backend != "offline")
        else {
            return (None, None);
        };
        (
            crate::detect_base_model(backend, &self.project_root),
            crate::detect_base_context_window(backend, &self.project_root),
        )
    }

    /// Drop state owned by a particular live base session. Static base-config
    /// observations remain available until the replacement session reports.
    pub(crate) fn reset_base_session_state(&mut self) {
        self.base_session_models.clear();
        self.base_session_model = None;
        self.base_session_mode = None;
        self.base_session_thinking = None;
        self.base_session_thinking_can_enable = false;
        self.base_session_thinking_can_disable = false;
        self.base_session_commands.clear();
        self.base_session_tools.clear();
        self.base_session_plan.clear();
        let (model, context_window) = self.configured_base_state();
        self.base_model = model;
        self.base_model_live = false;
        self.base_context_window = context_window;
    }

    fn set_live_base_session_model(&mut self, model_id: Option<&str>) {
        let Some(model_id) = model_id.map(str::trim).filter(|id| !id.is_empty()) else {
            let (model, context_window) = self.configured_base_state();
            self.base_session_model = None;
            self.base_model = model;
            self.base_model_live = false;
            self.base_context_window = context_window;
            return;
        };
        let catalog_window = self
            .base_session_models
            .iter()
            .find(|model| model.model_id == model_id)
            .and_then(|model| model.total_context_tokens);
        let configured_window = self.backend.as_deref().and_then(|backend| {
            crate::detect_base_context_window_for_model(backend, &self.project_root, model_id)
        });
        self.base_session_model = Some(model_id.to_string());
        self.base_model = Some(model_id.to_string());
        self.base_model_live = true;
        self.base_context_window = catalog_window.or(configured_window);
    }

    fn apply_base_session_state(&mut self, backend_id: &str, update: SessionStateUpdate) {
        if self.backend.as_deref() != Some(backend_id) {
            return;
        }
        match update {
            SessionStateUpdate::ModelCatalogReplaced {
                current_model_id,
                available_models,
            } => {
                self.base_session_models = available_models;
                self.set_live_base_session_model(Some(&current_model_id));
            }
            SessionStateUpdate::ModelChanged { model_id, .. } => {
                self.set_live_base_session_model(Some(&model_id));
            }
            SessionStateUpdate::ModelAutoSwitched { new_model_id, .. } => {
                self.set_live_base_session_model(Some(&new_model_id));
            }
            SessionStateUpdate::ModeChanged { mode } => {
                self.base_session_mode = Some(mode);
            }
            SessionStateUpdate::ThinkingChanged {
                enabled,
                can_enable,
                can_disable,
            } => {
                self.base_session_thinking = enabled;
                self.base_session_thinking_can_enable = can_enable;
                self.base_session_thinking_can_disable = can_disable;
            }
            SessionStateUpdate::CommandCatalogReplaced { commands, tools } => {
                self.base_session_commands = commands;
                self.base_session_tools = tools;
            }
            SessionStateUpdate::PlanReplaced { entries } => {
                self.base_session_plan = entries;
            }
        }
    }

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
                // Surface the run as a manageable background task (idempotent — a
                // gate-anchored `Continue` block re-emits this and REUSES the task).
                self.register_run_task(&requirement);
                // A fresh block clears any prior aborted / degraded / budget-paused
                // terminal state (and the per-run degrade latch) — this run is live again.
                self.aborted = false;
                self.degraded = false;
                self.budget_paused = false;
                self.run_degraded_seen = false;
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
            EngineEvent::PlanPosted {
                steps,
                statuses,
                done,
                total,
            } => {
                self.apply_plan_posted(&steps, &statuses, done, total);
            }
            EngineEvent::PlanStepStatus { id, title, status } => {
                self.apply_plan_step_status(&id, &title, &status);
            }
            EngineEvent::CriticVerdict {
                seat,
                accepts,
                blocking,
                remediation,
                advisory,
            } => self.apply_critic_verdict(seat, accepts, blocking, remediation, advisory),
            EngineEvent::PhaseStarted { phase } => {
                self.set_phase(phase, PhaseStatus::Running);
                self.phase_started_at = Some(std::time::Instant::now());
                // A phase is STARTING — the run is demonstrably alive and
                // progressing, so any prior terminal flag is stale. Clearing it
                // here keeps the status/placeholder honest if a single block
                // aborted / degraded earlier and the director then recovered into a
                // new phase (otherwise the run kept advancing under a frozen
                // "本轮已中止" / "[degraded]"). The per-run `run_degraded_seen` latch is
                // NOT reset here: degraded phases must accumulate ACROSS a block so
                // the end-of-block summary still classifies the exit correctly.
                self.aborted = false;
                self.finished = false;
                self.degraded = false;
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
            EngineEvent::GateOpened { gate, choice } => {
                // A Director session is still being wound down when its engine
                // emits GateOpened. Do not expose an actionable gate yet: a typed
                // `c`, picker approval, or `/continue` would otherwise start a
                // second resume while the first writer still owns the session.
                // The terminal RunPausedAtGate decision activates this exact gate
                // (including its structured choice) after `session.end()` returns.
                if self.director_run_in_flight || (self.agentic_in_flight && self.thinking) {
                    self.pending_director_gate = Some((gate, choice));
                    return;
                }
                self.active_gate = Some(gate);
                // Drop any stale picker up front so the auto-approve / queued-steer
                // early-return paths can never leave one rendering; the paused path
                // re-arms it below.
                self.gate_choice = None;
                // A DIRECTOR run's gate: the slug was never set by a
                // `PipelineStarted` (the director path doesn't emit one), so the
                // gate card would list `output/<slug>-*.md` placeholders. Adopt
                // the persisted workflow slug fail-open (best-effort read).
                if self.slug.is_empty() {
                    if let Some(s) = umadev_agent::read_workflow_state(&self.project_root) {
                        if !s.slug.trim().is_empty() {
                            self.slug = s.slug;
                        }
                    }
                }
                // Resolve the structured choice to render as a picker: the event's
                // choice when it carries renderable options, else none (fail-open
                // → the existing free-form gate). Stashed below ONLY on the paused
                // path (auto-approve / queued-steer return early without a picker).
                let resolved_choice = choice.filter(GateChoice::is_renderable);
                // Feature A — snapshot the run's elapsed BEFORE the live counters
                // are stopped below, so the gate-pause bell (armed only on the
                // manual-pause path) can gate on how long the run has been going.
                let run_started_before_gate = self.run_started_at;
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
                // which block produced this gate. A DIRECTOR run keeps its queue
                // on the steering lane instead (the event loop moves it into the
                // run's step-boundary intake), so the gate PAUSES for the user
                // and the steer applies on the resumed plan — never a legacy
                // block re-spawn.
                if !self.queued_steer.is_empty()
                    && !self.director_run_in_flight
                    && !self.director_gate_paused
                {
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
                        umadev_i18n::tf(
                            self.lang,
                            "gate.auto_approved",
                            &[umadev_i18n::t(self.lang, gate.human_label_key())],
                        ),
                    );
                    self.record_trust_pass(gate.id_str());
                    self.pending_auto_continue = Some(gate);
                    return;
                }

                // Feature A — the run is now PAUSED at a gate awaiting the user's
                // decision (guarded/plan tier; the auto-approve + queued-steer
                // paths already returned above). Bell the possibly-away user so
                // they come back to act. Gated on the run's pre-gate elapsed.
                self.arm_completion_bell(run_started_before_gate);
                self.push(
                    ChatRole::Gate,
                    gate_card(gate, &self.slug, &self.project_root, self.lang),
                );
                // Structured-choice picker (a nicer front-end to the confirm/revise
                // flow). Stored alongside the gate card; the live panel renders it
                // with a moving highlight, ↑↓/number keys drive it, and free-text
                // stays available (typing a custom response still works).
                //
                // Text-question mode (`question_form = "text"`): suppress the numbered
                // picker and instead frame the decision as prose the user answers in
                // natural language. The free-text reply path (`classify_reply`) already
                // turns their words into the decision, so nothing else changes.
                if self.config.prefers_text_questions() {
                    self.gate_choice = None;
                    if let Some(choice) = resolved_choice.as_ref() {
                        let prose = gate_choice_prose(choice, self.lang);
                        self.push(ChatRole::UmaDev, prose);
                    }
                } else {
                    self.gate_choice = resolved_choice;
                }
                self.gate_choice_sel = 0;
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
                    // Feature A — a full run reached delivery; bell the away user
                    // (gated on the run's elapsed, before the timer is cleared).
                    self.arm_completion_bell(self.run_started_at);
                    self.finished = true;
                    // The run delivered cleanly → settle its task as Done.
                    self.mark_active_task(TaskStatus::Done);
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
                    self.gate_choice = None;
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
                } else if paused_at.is_none() {
                    // **Scenario A — a block ended without a gate and SHORT of
                    // Delivery.** Every such exit is TERMINAL (each gate-anchored
                    // block stops and waits for an explicit continue), so a
                    // `final_phase != Delivery` completion is the run's end — but the
                    // old handler had no `else`, so it silently ignored it: the timer
                    // kept ticking and the header stayed `[run] 运行中` forever. Stop
                    // the live counters + activity for ANY such exit, then classify:
                    //   - a DEGRADED / failed exit (base offline → placeholder
                    //     templates, no real code produced, or a blocked quality gate
                    //     — each streamed a stable degrade/fail marker we latched in
                    //     `run_degraded_seen`) sets the `degraded` terminal flag so the
                    //     bar reads `[degraded] … /redo` (NOT a false `[ok] delivered`);
                    //   - a CLEAN short-circuit that legitimately ends before Delivery
                    //     (a docs-only task settling at its docs gate, a light
                    //     small-change / refactor pipeline, a clean single-phase
                    //     `/redo`) settles as `finished` so it drops out of the active
                    //     "运行中" state. Fail-open: an unmarked exit reads as clean.
                    self.arm_completion_bell(self.run_started_at);
                    self.run_started_at = None;
                    self.phase_started_at = None;
                    // The run is over — stop every worker-stall / heartbeat animation.
                    self.thinking = false;
                    self.tool_in_progress = false;
                    self.long_op_in_progress = false;
                    self.last_output_at = None;
                    self.transient_status = None;
                    // A late-queued steer never hit a gate gap on a terminal exit —
                    // surface it rather than silently drop it (mirrors the abort +
                    // delivery paths) so the "queued N" chip clears.
                    if !self.queued_steer.is_empty() {
                        let text = self.queued_steer.drain(..).collect::<Vec<_>>().join("\n");
                        self.push(
                            ChatRole::System,
                            umadev_i18n::tf(self.lang, "run.queued_unsent", &[&text]),
                        );
                    }
                    if self.run_degraded_seen {
                        self.degraded = true;
                        self.mark_active_task(TaskStatus::Failed);
                    } else {
                        self.finished = true;
                        self.mark_active_task(TaskStatus::Done);
                    }
                    // Settle the live plan / team-review panel so a terminal run
                    // stops rendering a stale live list under the transcript.
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
            EngineEvent::TurnUsage { usage } => {
                self.session_usage.apply(usage);
                self.maybe_nudge_compaction();
            }
            EngineEvent::BaseModel { id } => {
                // The base reported the EXACT model it resolved for this session (its
                // session metadata). Adopt it as the live display model. It is NOT
                // used to infer the context window from a hardcoded table: model ids
                // drift, and the base may route to a third-party/local model whose
                // real window UmaDev cannot prove. If OpenCode provider metadata can
                // prove the live model's exact window, keep it; otherwise clear any
                // stale denominator from a previous static config match. Fail-open:
                // an empty id is ignored.
                let id = id.trim();
                if !id.is_empty() {
                    self.set_live_base_session_model(Some(id));
                }
            }
            EngineEvent::BaseSessionState { backend_id, update } => {
                self.apply_base_session_state(&backend_id, update);
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
                // Latch a DEGRADED / failed signal as it streams past, so a terminal
                // `BlockCompleted` short of delivery can tell a genuine degrade apart
                // from a clean short-circuit (docs-only / light / clean `/redo`). The
                // runner tags these with STABLE, language-independent markers (proven
                // stable by its own tests), so this never leans on translated prose:
                //   - `[WARN][降级]` — a phase fell back to an offline placeholder
                //     template (the exact Scenario-A banner the user reported);
                //   - `UD-EVID-003` — the blocked-quality-gate hard-stop evidence code;
                //   - "未产出任何真实代码文件" — the "no real code produced" hard failure.
                if note_signals_degraded(&note) {
                    self.run_degraded_seen = true;
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
                    umadev_runtime::StreamEvent::ToolUseCorrelated {
                        call_id,
                        name,
                        detail,
                        edit,
                    } => {
                        self.collapse_thinking_block();
                        self.stream_text_active = false;
                        self.tool_in_progress = true;
                        self.long_op_in_progress =
                            matches!(name.as_str(), "Bash") && is_long_running_command(&detail);
                        match edit {
                            Some(e) if matches!(name.as_str(), "Write" | "Edit" | "MultiEdit") => {
                                self.push_diff_correlated(&call_id, &e);
                            }
                            _ => self.push_tool_use_correlated(&call_id, &name, &detail),
                        }
                    }
                    umadev_runtime::StreamEvent::ToolProgressCorrelated { call_id, title } => {
                        self.collapse_thinking_block();
                        self.stream_text_active = false;
                        self.tool_in_progress = true;
                        self.attach_tool_progress_correlated(&call_id, &title);
                    }
                    umadev_runtime::StreamEvent::ToolOutputDelta { delta } => {
                        // A process-log delta is progress, never completion: keep
                        // the running row/spinner alive and append the visible log.
                        self.collapse_thinking_block();
                        self.stream_text_active = false;
                        self.tool_in_progress = true;
                        self.attach_tool_output_delta(&delta);
                    }
                    umadev_runtime::StreamEvent::ToolOutputDeltaCorrelated { call_id, delta } => {
                        self.collapse_thinking_block();
                        self.stream_text_active = false;
                        self.tool_in_progress = true;
                        self.attach_tool_output_delta_correlated(&call_id, &delta);
                    }
                    umadev_runtime::StreamEvent::ToolOutputSnapshot { output } => {
                        self.collapse_thinking_block();
                        self.stream_text_active = false;
                        self.tool_in_progress = true;
                        self.attach_tool_output_snapshot(&output);
                    }
                    umadev_runtime::StreamEvent::ToolOutputSnapshotCorrelated {
                        call_id,
                        output,
                    } => {
                        self.collapse_thinking_block();
                        self.stream_text_active = false;
                        self.tool_in_progress = true;
                        self.attach_tool_output_snapshot_correlated(&call_id, &output);
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
                        self.refresh_running_tool_flags();
                    }
                    umadev_runtime::StreamEvent::ToolResultCorrelated {
                        call_id,
                        ok,
                        summary,
                    } => {
                        self.collapse_thinking_block();
                        self.stream_text_active = false;
                        self.tool_in_progress = false;
                        self.long_op_in_progress = false;
                        self.attach_tool_result_correlated(&call_id, ok, &summary);
                        self.refresh_running_tool_flags();
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
                        self.append_thinking_delta(&delta);
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
        // Wave 2 P0 — ONE shared key mapping for both input paths. Any literal
        // control-char key form a backend may surface (Windows/ConPTY Backspace
        // as `Char('\u{8}')` / `Char('\u{7f}')`, a raw Ctrl-C as
        // `Char('\u{3}')`, …) is folded here through the SAME
        // `input::keymap::char_to_key` table the owned byte-decoder uses — so
        // the per-arm duplicate catches this file used to carry are gone and
        // the two paths cannot drift. `InputSource::next` already applies the
        // same fold; this delegating call keeps direct callers (tests, future
        // surfaces) on the identical contract (idempotent, so double
        // normalization is a no-op).
        let (key, mods) = crate::input::keymap::normalize_key(key, mods);
        // F1 toggles help in any mode.
        if let KeyCode::F(1) = key {
            self.show_help = !self.show_help;
            self.help_scroll = 0;
            return Action::None;
        }
        match self.mode {
            AppMode::Picker => self.picker_key(key),
            AppMode::Chat => {
                // A mouse-made input-box selection (see `input_selection`) caches
                // `(visual_row, char_col)` coordinates against the CURRENT wrapped
                // text. A keystroke that EDITS the buffer reflows those rows, so the
                // cached coordinates — and the highlight painted from them — go
                // stale. Snapshot only when a selection is actually live (rare, so
                // no per-keystroke clone cost otherwise) and retire it when the text
                // changed; a pure caret move / scroll leaves the still-valid
                // highlight in place.
                let before = self.input_selection.is_some().then(|| self.input.clone());
                let action = self.chat_key(key, mods);
                if let Some(before) = before {
                    if before != self.input {
                        self.input_selection = None;
                        self.input_selection_dragging = false;
                    }
                }
                action
            }
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
                            // SOFT two-step, NOT a hard block: the login probe is a false
                            // negative for a base the user has pointed at a LOCAL / third-party
                            // model (opencode → local, claude → GLM, …) that needs no
                            // `<base> auth login`, and the product contract is "drive whatever
                            // the base is already configured with." So the FIRST select warns
                            // (login hint + "select again to continue anyway"); selecting the
                            // SAME base again proceeds. A different base re-warns (id mismatch).
                            let id = chosen.backend_id.clone();
                            if id.is_some() && self.picker_login_confirm == id {
                                self.picker_login_confirm = None;
                                // fall through to commit
                            } else {
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
                                self.picker_login_confirm = id;
                                return Action::None;
                            }
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
        // Feature B — search is its own modal mode: while the bar is open it owns
        // EVERY keystroke (typing filters; Enter/↑↓/Ctrl+N/P navigate; Esc
        // closes), so it can't collide with the slash palette, the @-mention
        // popover, history recall, or the editing keys below.
        if self.search.is_some() {
            return self.search_key(key, mods);
        }
        // I3 — reverse prompt-history search (Ctrl+R) is likewise its own modal
        // mode: while open it owns EVERY keystroke (typing narrows; Enter loads;
        // Ctrl+R/↑/↓ cycle; Esc cancels), mutually exclusive with the transcript
        // search, the slash palette, the @-mention popover, and `↑↓` recall.
        if self.history_search.is_some() {
            return self.history_search_key(key, mods);
        }
        if self.show_help {
            match key {
                KeyCode::Esc => {
                    self.show_help = false;
                    return Action::None;
                }
                KeyCode::Down | KeyCode::Char('j' | 'J') => {
                    self.help_scroll_down(1);
                    return Action::None;
                }
                KeyCode::Up | KeyCode::Char('k' | 'K') => {
                    self.help_scroll_up(1);
                    return Action::None;
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.help_scroll_down(10);
                    return Action::None;
                }
                KeyCode::PageUp => {
                    self.help_scroll_up(10);
                    return Action::None;
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.help_scroll = 0;
                    return Action::None;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.help_scroll_to_bottom();
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

        // Grok Build native prompt queue. Both published toggle chords are
        // accepted because several terminal/IDE combinations drop one of them.
        if ctrl && !alt && matches!(key, KeyCode::Char(';' | '\'')) {
            if self.prompt_queue.toggle() {
                self.request_full_repaint();
            }
            return Action::None;
        }

        // A focused queue pane owns its navigation and mutation keys. Mutations
        // only mark "awaiting snapshot"; the rows themselves are untouched.
        if self.prompt_queue.is_open() {
            let mutation = match key {
                KeyCode::Esc => {
                    self.prompt_queue.toggle();
                    self.request_full_repaint();
                    return Action::None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.prompt_queue.select_next();
                    return Action::None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.prompt_queue.select_previous();
                    return Action::None;
                }
                KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace => {
                    self.prompt_queue.remove_selected()
                }
                KeyCode::Char('J') => self.prompt_queue.reorder_selected(false),
                KeyCode::Char('K') => self.prompt_queue.reorder_selected(true),
                KeyCode::Enter | KeyCode::Char('i') if ctrl => {
                    self.prompt_queue.interject_selected()
                }
                KeyCode::Char('e') | KeyCode::Enter => {
                    if let Some(text) = self.prompt_queue.begin_edit() {
                        self.clear_input();
                        self.input = text;
                        self.input_cursor = self.input_len();
                        self.request_full_repaint();
                    }
                    return Action::None;
                }
                _ => return Action::None,
            };
            return mutation.map_or(Action::None, Action::PromptQueueMutate);
        }

        // Kill-ring coalescing + the yank-pop window live only across a run of
        // consecutive same-FAMILY keys. Any key that is neither a kill
        // (Ctrl+U/K/W) nor a yank (Ctrl+Y / Alt+Y) closes both windows, so a
        // cursor move between two kills starts a fresh ring entry and Alt+Y is
        // valid only immediately after a yank. (Search / overlay / help modes
        // return earlier, so they never touch this state.)
        let is_kill = ctrl && !alt && matches!(key, KeyCode::Char('u' | 'k' | 'w'));
        let is_yank = (ctrl ^ alt) && matches!(key, KeyCode::Char('y'));
        if !is_kill && !is_yank {
            self.reset_kill_yank();
        }

        // ---- structured gate picker ----------------------------------------
        // When a gate surfaced a structured choice AND the input box is EMPTY,
        // the arrow keys / number keys / Enter drive the picker — a nicer
        // front-end to the confirm/revise flow. The moment the user types ANY
        // text the box is non-empty and the picker yields to the free-text
        // fallback (a custom revision still works), so it never blocks the
        // keyboard. Esc is deliberately NOT intercepted (it keeps its
        // interrupt/quit meaning), and Shift+↑/↓ still scroll the transcript.
        if self.input.is_empty() {
            if let Some(n) = self.gate_choice.as_ref().map(|c| c.options.len()) {
                match key {
                    KeyCode::Up if !shift => {
                        self.gate_choice_move(-1);
                        return Action::None;
                    }
                    KeyCode::Down if !shift => {
                        self.gate_choice_move(1);
                        return Action::None;
                    }
                    KeyCode::Char(d @ '1'..='9') => {
                        let idx = (d as usize) - ('1' as usize);
                        if idx < n {
                            return self.gate_choice_pick(idx);
                        }
                        // Out-of-range digit → fall through to normal insertion.
                    }
                    KeyCode::Enter if !shift => {
                        return self.gate_choice_pick(self.gate_choice_sel);
                    }
                    _ => {}
                }
            }
        }

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
                if self.has_interruptible_work() {
                    if self.interrupt_armed() {
                        self.interrupt_armed_at = None;
                        return Action::Cancel;
                    }
                    self.interrupt_armed_at = Some(std::time::Instant::now());
                    return Action::None;
                }
                // I6 — empty box + a chat turn parked behind the in-flight one →
                // pull the most recent queued message back for editing (popping
                // it) BEFORE the rewind/quit gesture, so a queued turn can be
                // fixed (or dropped) before it sends. A no-op when the queue is
                // empty, falling through to the rewind/quit arms below.
                if self.input.is_empty() && self.recall_queued_chat() {
                    return Action::None;
                }
                // Idle, EMPTY input, with a prior user turn → double-Esc REWINDS:
                // re-load the last user message into the box for editing and drop
                // the turns after it, so the user can fix + re-ask from that point
                // (chat-only; file/run state is the engine's `checkpoint`, out of
                // scope here). Quitting moved to `/quit`, so this idle gesture is
                // free. A first Esc ARMS it (a stray keypress can't rewind); a
                // second Esc fires. With no prior user message there's nothing to
                // rewind → fall through to the quit-confirm below.
                if self.input.is_empty() && self.last_user_msg_index().is_some() {
                    if self.pending_rewind {
                        self.pending_rewind = false;
                        self.rewind_to_last_user_message();
                        return Action::None;
                    }
                    self.pending_rewind = true;
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "tui.rewind.hint"),
                    );
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
            KeyCode::Backspace if alt => {
                self.pending_quit_confirm = false;
                self.pending_rewind = false;
                self.delete_word_back();
                Action::None
            }
            // The Windows/ConPTY literal BS/DEL control-char forms are folded
            // to `KeyCode::Backspace` upstream by the ONE shared mapping
            // (`input::keymap::normalize_key`, applied in `apply_key_with_mods`
            // and in `InputSource::next`) — no per-arm duplicate catch here.
            KeyCode::Backspace => {
                self.pending_quit_confirm = false;
                self.pending_rewind = false;
                self.backspace();
                Action::None
            }
            KeyCode::Delete => {
                self.pending_quit_confirm = false;
                self.pending_rewind = false;
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
                // I6 — empty box + a chat turn parked behind the in-flight one →
                // recall the QUEUE first (pull the most recent queued message back
                // for editing), taking precedence over shell-history recall. Only
                // on a genuinely empty box that isn't already mid history-paging.
                if self.input.is_empty()
                    && self.input_history_idx.is_none()
                    && self.recall_queued_chat()
                {
                    return Action::None;
                }
                // Caret is on the first row — recall history (Claude Code parity).
                // Even a NON-EMPTY partial draft recalls: `input_history_back`
                // stashes the draft when recall begins (idx `None`) and ↓ past the
                // newest entry restores it, so ↑ never destroys in-progress text —
                // it parks it. (The old gate required an empty box, so ↑ on a
                // first-row caret with leftover text did nothing — the CC mismatch.)
                self.input_history_back();
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
            // ---- Ctrl+J: the UNIVERSAL newline (works on EVERY terminal) ----
            // Ctrl+J is a literal LF (0x0A) on every terminal — the owned
            // decoder folds that byte to `Char('j')` + CONTROL — so this arm
            // ALWAYS inserts a newline regardless of the keyboard protocol.
            // Shift+Enter only reaches the newline path where the terminal
            // reports it via the kitty CSI-u protocol (enabled in
            // `setup_terminal`); on terminals that don't, a bare Shift+Enter
            // arrives as a plain CR and would SUBMIT mid-thought — so Ctrl+J is
            // the terminal-agnostic way to build a multi-line prompt. It is
            // unconditional (never shadowed by the mention popover / slash
            // palette, which own `Enter`, not `Ctrl+J`).
            KeyCode::Char('j') if ctrl => {
                self.pending_quit_confirm = false;
                self.pending_rewind = false;
                self.input_history_idx = None;
                self.insert_at_cursor('\n');
                Action::None
            }

            // Ctrl+V is the explicit image-clipboard action. A PTY cannot emit
            // an image as bracketed paste, so the event loop asks the LOCAL OS
            // clipboard on the blocking pool. Ordinary text paste remains an
            // `Event::Paste` handled by `handle_paste` and never touches this
            // arm — zero new work on the overwhelmingly common path.
            KeyCode::Char('v') if ctrl && !alt => Action::PasteImage,

            // ---- enter: accept the highlighted @-mention (popover open) ----
            // Wins over submit so Enter on the file typeahead inserts the path
            // instead of sending the half-typed `@partial`. Shift+Enter still
            // falls through to insert a literal newline.
            KeyCode::Enter if has_mention && !shift => {
                self.accept_mention();
                Action::None
            }

            // ---- enter: submit, or insert newline with Shift ----
            KeyCode::Enter | KeyCode::Char('i') if ctrl && self.prompt_queue.ready() => {
                let raw = self.input.clone();
                if raw.trim().is_empty() {
                    return self
                        .prompt_queue
                        .interject_top()
                        .map_or(Action::None, Action::PromptQueueMutate);
                }
                let turn = self.compose_submitted_turn(&raw);
                if self.prompt_queue.is_editing() {
                    if turn.has_attachments() {
                        self.push(
                            ChatRole::System,
                            umadev_i18n::t(self.lang, "prompt_queue.edit_attachments"),
                        );
                        return Action::None;
                    }
                    let Some(mutation) = self.prompt_queue.submit_edit(turn.text) else {
                        return Action::None;
                    };
                    self.clear_input();
                    return Action::PromptQueueMutate(mutation);
                }
                self.clear_input();
                self.transcript_scroll_to_bottom();
                self.remember_submission(&turn.text);
                self.submit_turn_with_queue_placement(turn, Some(PromptQueuePlacement::SendNow))
            }

            KeyCode::Enter => {
                if shift {
                    // Shift+Enter inserts a literal newline so the user
                    // can build multi-line prompts inside the chat box.
                    self.insert_at_cursor('\n');
                    return Action::None;
                }
                // Windows paste-truncation guard: the Windows console delivers a bracketed
                // paste as raw key events (no `Event::Paste`), so a newline INSIDE a pasted
                // multi-line requirement arrives as a bare Enter and used to SUBMIT the text
                // before it — truncating the paste at line 1. The event loop flags a key that
                // landed in a sub-`PASTE_BURST_GAP` burst (faster than any human types), so a
                // pasted newline inserts instead of submitting. Only the real loop sets the
                // flag (tests never do), and it's gated to Windows (other platforms frame paste
                // as a proper `Event::Paste`), so genuine submit is untouched everywhere else.
                #[cfg(windows)]
                if self.key_arrived_in_burst {
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
                    let completion = {
                        let matches = self.palette_matches();
                        let typed = self.input[1..].to_ascii_lowercase();
                        let is_exact = matches.iter().any(|p| p.verb == typed)
                            || umadev_host::driver_for(&typed).is_some();
                        if matches.is_empty() || is_exact {
                            None
                        } else {
                            let sel = self.palette_selected.min(matches.len() - 1);
                            Some(matches[sel].verb.to_string())
                        }
                    };
                    if let Some(verb) = completion {
                        self.input = format!("/{verb}");
                        self.input_cursor = self.input_len();
                    }
                }
                // Snapshot typed blocks BEFORE clearing their chip backing stores.
                // No attachment path is ever rewritten into prompt text.
                let raw = self.input.clone();
                let turn = self.compose_submitted_turn(&raw);
                if turn.has_attachments()
                    && (turn.text.trim_start().starts_with('/')
                        || turn.text.trim_start().starts_with('!'))
                {
                    self.push_attachment_rejection("attach.reason.command");
                    return Action::None;
                }
                if self.prompt_queue.is_editing() {
                    if turn.has_attachments() {
                        self.push(
                            ChatRole::System,
                            umadev_i18n::t(self.lang, "prompt_queue.edit_attachments"),
                        );
                        return Action::None;
                    }
                    let Some(mutation) = self.prompt_queue.submit_edit(turn.text) else {
                        return Action::None;
                    };
                    self.clear_input();
                    return Action::PromptQueueMutate(mutation);
                }
                self.clear_input();
                if turn.input.blocks.is_empty() || turn.text.trim().is_empty() {
                    if let Some(mutation) = self.prompt_queue.interject_top() {
                        return Action::PromptQueueMutate(mutation);
                    }
                    return Action::None;
                }
                // Submitting re-pins the transcript to the bottom so the user
                // always sees their own new turn (and the reply) land, even if
                // they were scrolled up reviewing history.
                self.transcript_scroll_to_bottom();
                self.remember_submission(&turn.text);
                if let Some(action) = self.try_slash_command(&turn.text) {
                    return action;
                }
                // `!cmd` runs a one-off local shell in the project root (Claude
                // Code's `!` convenience-shell convention) — NOT routed to the
                // borrowed brain. Checked after the slash dispatch so it can't
                // shadow a command; a bare `!` is a consumed no-op.
                if let Some(action) = self.try_bang_command(&turn.text) {
                    return action;
                }
                self.submit_turn(turn)
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
            // Ctrl+Y — yank: re-insert whatever the last Ctrl+U/K/W removed (the
            // front kill-ring entry). Pairs with the kills so a mis-fired
            // line/word delete is recoverable instead of destroyed.
            KeyCode::Char('y') if ctrl => {
                self.yank();
                Action::None
            }
            // Alt+Y — yank-pop: cycle to an older kill-ring entry, replacing the
            // just-yanked span. Valid only immediately after a yank/yank-pop.
            KeyCode::Char('y') if alt => {
                self.yank_pop();
                Action::None
            }
            // Ctrl+Z — undo the last input edit (text + caret). Raw mode disables
            // the terminal's own SIGTSTP, so Ctrl+Z arrives here as a plain key,
            // free to own — job-control resume is handled separately via SIGCONT,
            // not by this in-app keystroke. The priority recovery key, complement
            // to the kill-ring.
            KeyCode::Char('z') if ctrl => {
                self.undo();
                Action::None
            }
            // Alt+Z — redo (replay an undone edit). A free combo; undo is Ctrl+Z.
            KeyCode::Char('z') if alt => {
                self.redo();
                Action::None
            }
            KeyCode::Char('c') if ctrl => {
                // Ctrl+C parity with Claude Code / opencode: while work is in
                // flight (a pipeline run OR a routed chat turn), Ctrl-C
                // INTERRUPTS it immediately — regardless of whether the input
                // box has text. The old behaviour (only-interrupt-on-empty)
                // forced a second keystroke to actually stop a run when the
                // user had half-typed the next message.
                if self.has_interruptible_work() || self.thinking {
                    // The canonical path aborts any owned task, seals partial
                    // output, preserves deferred FIFO turns, and invalidates an
                    // in-flight gate-query generation.
                    self.clear_input();
                    return Action::Cancel;
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
            // Ctrl+F — open the in-transcript search bar (Feature B). Free key
            // (no bare-Ctrl+F binding existed; Ctrl+Alt+F is the half-page-down
            // combo, a different modifier set). Search then owns input until Esc.
            KeyCode::Char('f') if ctrl => {
                self.open_search();
                Action::None
            }
            // Ctrl+R is dual-purpose, disambiguated by transcript state so neither
            // gesture clobbers the other:
            //  • a foldable row in view → toggle its fold (the "fold just the
            //    latest" lever, secondary to the global Ctrl+O); else
            //  • nothing foldable → open the reverse prompt-history search (I3 —
            //    the readline reverse-i-search convention).
            // Suppressed entirely while the @-mention / slash palette own keys, so
            // a Ctrl+R over `/run …` or `@src` doesn't steal their input.
            KeyCode::Char('r') if ctrl => {
                if self.has_foldable() {
                    self.toggle_last_collapsible();
                } else if !has_mention && !has_palette {
                    self.open_history_search();
                }
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
                self.pending_rewind = false;
                self.input_history_idx = None;
                self.insert_at_cursor(c);
                Action::None
            }

            _ => Action::None,
        }
    }

    /// Index of the most recent user (`You`) turn in the transcript, or `None`
    /// when the user has not spoken yet. Drives the idle double-Esc rewind.
    #[must_use]
    fn last_user_msg_index(&self) -> Option<usize> {
        self.history.iter().rposition(|m| m.role == ChatRole::You)
    }

    /// Rewind the CHAT transcript to the last user turn: re-load that message's
    /// text into the input box for editing and drop it plus every turn after it,
    /// so a resend re-asks from that point. Chat-only — it does NOT roll back
    /// files or run state (that is the engine's `checkpoint`, out of scope).
    /// Fail-open: a no-op when there is no prior user turn.
    fn rewind_to_last_user_message(&mut self) {
        let Some(idx) = self.last_user_msg_index() else {
            return;
        };
        // `idx` is valid (just found by `rposition`); `body()` is the plain text
        // of a `You` row. Drop the user turn + everything after it, then re-load
        // its text for editing.
        let text = self.history[idx].body().into_owned();
        self.history.truncate(idx);
        // Keep the base-facing memory + durable transcript in lockstep with the
        // visible rewind: drop the matching last user turn (and any reply after
        // it) from BOTH `conversation` and `full_transcript`. Truncating only
        // `history` left the dropped turn in the memory handed to the base (so a
        // resend re-asked WITH it) and on disk (so a relaunch `/resume` restored
        // it) — contradicting the "re-ask from that point" contract. Each vector
        // is truncated at its OWN last `user` entry (compaction can desync their
        // lengths), and the disk mirror is rewritten so the relaunch matches.
        if let Some(c_idx) = self.conversation.iter().rposition(|m| m.role == "user") {
            self.conversation.truncate(c_idx);
        }
        if let Some(t_idx) = self.full_transcript.iter().rposition(|m| m.role == "user") {
            self.full_transcript.truncate(t_idx);
        }
        // Mirror the rewind to disk. When the rewound turn was the FIRST one the
        // transcript is now EMPTY, and `persist_chat` early-returns on an empty
        // transcript (to avoid empty-file litter) — which would leave the OLD,
        // un-rewound chat on disk, so a relaunch `/resume` would restore the
        // conversation we just dropped. Delete the persisted chat in that case;
        // otherwise rewrite it to the truncated transcript.
        if self.full_transcript.is_empty() {
            self.discard_persisted_chat();
        } else {
            self.persist_chat();
        }
        self.input = text;
        self.input_cursor = self.input_len();
        // Leave history recall + the quit/rewind arms in a clean state, and
        // re-pin the transcript to the bottom so the freshly truncated tail shows.
        self.input_history_idx = None;
        self.pending_quit_confirm = false;
        self.pending_rewind = false;
        self.transcript_scroll_to_bottom();
    }

    /// Most recent user row that was real work/chat, not a local meta query.
    fn last_non_meta_user_index(&self) -> Option<usize> {
        self.history
            .iter()
            .enumerate()
            .rev()
            .find_map(|(idx, message)| {
                (message.role == ChatRole::You
                    && classify_live_meta(message.body().as_ref()).is_none())
                .then_some(idx)
            })
    }

    /// Structured diff paths emitted after the latest real user request.
    fn latest_turn_diff_paths(&self) -> Vec<String> {
        let start = self.last_non_meta_user_index().map_or(0, |idx| idx + 1);
        let mut paths = Vec::new();
        for message in self.history.iter().skip(start) {
            if let MessageBody::Diff(diff) = &message.kind {
                if !paths.contains(&diff.path) {
                    paths.push(diff.path.clone());
                }
            }
        }
        paths
    }

    /// The post-turn fact line, if the latest turn has already settled.
    fn latest_turn_fact(&self) -> Option<&str> {
        let start = self.last_non_meta_user_index().map_or(0, |idx| idx + 1);
        self.history.iter().skip(start).rev().find_map(|message| {
            let MessageBody::Text(body) = &message.kind else {
                return None;
            };
            (body.contains("[note] 本轮实际文件变更:") || body.contains("[note] 本轮无文件变更"))
                .then_some(body.as_str())
        })
    }

    fn live_progress_reply(&self) -> String {
        let mut lines = vec![umadev_i18n::t(self.lang, "live_meta.progress.title").to_string()];

        if let Some(task) = self.active_task() {
            lines.push(umadev_i18n::tf(
                self.lang,
                "live_meta.progress.task",
                &[
                    &task.id,
                    &task.requirement,
                    &task.done.to_string(),
                    &task.total.to_string(),
                ],
            ));
        } else if self.thinking {
            let current = self
                .last_non_meta_user_index()
                .and_then(|idx| self.history.get(idx))
                .map_or_else(|| self.requirement.clone(), |m| task_summary(&m.body()));
            lines.push(umadev_i18n::tf(
                self.lang,
                "live_meta.progress.chat",
                &[&current],
            ));
        } else {
            lines.push(umadev_i18n::t(self.lang, "live_meta.progress.idle").to_string());
        }

        if !self.plan_steps.is_empty() {
            let done = self
                .plan_steps
                .iter()
                .filter(|step| step.status == "done")
                .count();
            lines.push(umadev_i18n::tf(
                self.lang,
                "live_meta.progress.plan",
                &[&done.to_string(), &self.plan_steps.len().to_string()],
            ));
            if let Some(step) = self
                .plan_steps
                .iter()
                .find(|step| step.status == "active")
                .or_else(|| self.plan_steps.iter().find(|step| step.status == "pending"))
            {
                lines.push(umadev_i18n::tf(
                    self.lang,
                    "live_meta.progress.step",
                    &[&step.id, &step.title],
                ));
            }
        }

        if let Some(row) = self
            .phases
            .iter()
            .find(|row| row.status == PhaseStatus::Running)
        {
            lines.push(umadev_i18n::tf(
                self.lang,
                "live_meta.progress.phase",
                &[row.phase.id()],
            ));
        }
        if let Some(gate) = self.active_gate {
            lines.push(umadev_i18n::tf(
                self.lang,
                "live_meta.progress.gate",
                &[gate.id_str()],
            ));
        }
        lines.join("\n")
    }

    fn live_changes_reply(&self) -> String {
        const CAP: usize = 12;
        let paths = self.latest_turn_diff_paths();
        if !paths.is_empty() {
            let shown = paths
                .iter()
                .take(CAP)
                .cloned()
                .collect::<Vec<_>>()
                .join(" · ");
            return umadev_i18n::tf(
                self.lang,
                "live_meta.changes.diff",
                &[&paths.len().to_string(), &shown],
            );
        }

        if let Some(fact) = self.latest_turn_fact() {
            if fact.contains("[note] 本轮无文件变更") {
                return umadev_i18n::t(self.lang, "live_meta.changes.none").to_string();
            }
            if let Some((_, files)) = fact.split_once("[note] 本轮实际文件变更:") {
                return umadev_i18n::tf(self.lang, "live_meta.changes.fact", &[files.trim()]);
            }
        }

        let status_paths = crate::git_status_porcelain(&self.project_root)
            .map(|snapshot| {
                snapshot
                    .lines()
                    .filter_map(crate::porcelain_path)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if status_paths.is_empty() {
            return umadev_i18n::t(self.lang, "live_meta.changes.none_yet").to_string();
        }
        let shown = status_paths
            .iter()
            .take(CAP)
            .cloned()
            .collect::<Vec<_>>()
            .join(" · ");
        umadev_i18n::tf(
            self.lang,
            "live_meta.changes.git_status",
            &[&status_paths.len().to_string(), &shown],
        )
    }

    fn answer_live_meta(&mut self, intent: LiveMetaIntent, text: String) {
        let reply = match intent {
            LiveMetaIntent::Progress => self.live_progress_reply(),
            LiveMetaIntent::Changes => self.live_changes_reply(),
        };
        self.push(ChatRole::You, text.clone());
        self.push(ChatRole::UmaDev, reply.clone());
        self.record_turn("user", text);
        self.record_turn("assistant", reply);
        self.persist_chat();
        self.refresh_status();
    }

    /// Start the sole read-only question attached to an open confirmation gate.
    /// The writer is already parked; generation tagging prevents late answers
    /// from crossing a cancel, clear, or later run boundary.
    fn begin_gate_query(&mut self, text: String) -> Action {
        self.record_user_turn(&text);
        self.gate_query_epoch = self.gate_query_epoch.wrapping_add(1);
        let epoch = self.gate_query_epoch;
        self.active_gate_query_epoch = Some(epoch);
        self.gate_query_in_flight = true;
        self.thinking = true;
        self.thinking_started = Some(std::time::Instant::now());
        self.agentic_in_flight = true;
        self.refresh_status();
        Action::GateQuery {
            epoch,
            question: text,
        }
    }

    /// Treat non-slash text as either a fresh requirement (if no run is
    /// active) or a revision (if a gate is open). Single-letter `c` at a
    /// gate is the documented shortcut for "approve / continue" — match
    /// the gate card so users don't have to type `/continue` every time.
    #[cfg(test)]
    fn submit_text(&mut self, text: String) -> Action {
        self.submit_turn(SubmittedTurn::text(text))
    }

    fn submit_turn(&mut self, turn: SubmittedTurn) -> Action {
        self.submit_turn_with_queue_placement(turn, None)
    }

    fn submit_turn_with_queue_placement(
        &mut self,
        turn: SubmittedTurn,
        queue_placement: Option<PromptQueuePlacement>,
    ) -> Action {
        let text = turn.text.clone();
        // A cancel is DRAINING ("stopping…"). Restore new text to the input box
        // instead of racing it into the preserved queue while the old task still
        // owns its session; the user can submit once cancellation settles.
        if self.cancelling {
            self.restore_submitted_turn(turn);
            return Action::None;
        }
        // These observations never become steering or a base turn.
        if let Some(intent) = classify_live_meta(&text) {
            self.answer_live_meta(intent, text);
            return Action::None;
        }
        // A2#5 — a PAUSED consequential-action approval is live: an exact typed
        // decision (「批准」/"approve"/"y" allows, 「拒绝」/"deny"/"n" denies)
        // resolves the pause instead of queueing as a normal message with no
        // effect (the reported trap). Anything else falls through to the normal
        // lanes below — a real steering message typed mid-pause still lands, and
        // the sticky bar keeps showing how to answer. The paused drain emits its
        // own allowed/denied Note, so only the user's turn is echoed here.
        if self.pending_approval.is_some() {
            if let Some(allow) = classify_approval_reply(&text) {
                self.pending_approval = None;
                self.push(ChatRole::You, text);
                self.refresh_status();
                return Action::ApprovalReply(allow);
            }
        }
        // Natural-language cancel is a protocol action only while work is live.
        // Keep it outside semantic routing so it cannot sit in queued_chat while
        // the writer the user asked to stop keeps running.
        if (self.thinking
            || self.agentic_in_flight
            || self.is_pipeline_active()
            || self.active_gate.is_some()
            || self.director_gate_paused)
            && umadev_agent::is_running_cancel_intent(&text)
        {
            self.active_gate = None;
            self.gate_choice = None;
            self.push(ChatRole::You, text);
            self.refresh_status();
            return Action::Cancel;
        }
        // A gate question uses a separate read-only one-shot while the parked
        // Director plan remains intact. Keep a second submitted line editable
        // instead of dispatching another base call concurrently; once the answer
        // lands the user can press Enter to send the preserved text. This check
        // deliberately follows natural-language cancellation so `stop` / `取消`
        // can still abort an in-flight gate query immediately.
        if self.gate_query_in_flight {
            self.restore_submitted_turn(turn);
            return Action::None;
        }
        // An explicit Enter after a failed turn is an intentional retry. Exact
        // duplicates already queued before failure are removed by settlement;
        // a later user action must remain authoritative (notably after a 502).
        self.push(ChatRole::You, text.clone());
        // A brain-driven turn is still in flight (`thinking`). Firing a second one
        // now would drive the SAME base `session_id` in two subprocesses at once →
        // interleaved / out-of-order replies and a scrambled memory. Park this
        // turn instead; the event loop fires it the moment the current turn lands.
        // A failed terminal outcome first drops exact duplicate retries of the
        // failed turn, so a double-Enter cannot auto-replay the same broken route.
        // (A gate is never open while `thinking`, so this check sits ahead of gate
        // handling.)
        if self.thinking {
            // A DIRECTOR build has two safe input lanes. Only an explicit change
            // to the CURRENT task reaches the step-boundary steering intake.
            // Questions, later tasks, and ambiguous text wait as ordinary chat;
            // after the run settles they go through the same model-first route as
            // any fresh turn. This classifier is only a concurrency/safety split,
            // never a business-topic intent router.
            if self.director_run_in_flight {
                if turn.has_attachments() {
                    self.queue_chat_turn(turn);
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "input.steer.director_deferred"),
                    );
                    self.refresh_status();
                    return Action::None;
                }
                match umadev_agent::classify_running_input(&text) {
                    umadev_agent::RunningInputDisposition::Steer => {
                        self.queued_steer.push_back(text);
                        self.push(
                            ChatRole::System,
                            umadev_i18n::t(self.lang, "run.steer_queued"),
                        );
                    }
                    umadev_agent::RunningInputDisposition::Query
                    | umadev_agent::RunningInputDisposition::Deferred => {
                        self.queue_chat_turn(turn.clone());
                        self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.deferred"));
                    }
                }
                self.refresh_status();
                return Action::None;
            }
            // Park it WITHOUT recording into conversation memory yet. Recording at
            // submit time left a dangling "user said X" with no assistant reply
            // whenever the user interrupted. The turn is recorded only when it fires
            // (see `take_next_queued_chat`), so an interrupted queue leaves memory
            // clean. Tell the user it is queued — NOT the pipeline `run.queued`
            // text (there is no gate here, this is a plain conversational turn).
            if self.prompt_queue.ready() {
                return Action::PromptQueueEnqueue {
                    turn,
                    placement: queue_placement.unwrap_or(PromptQueuePlacement::Tail),
                };
            }
            if self.live_input_ready {
                return Action::LiveInput(turn);
            }
            self.queue_chat_turn(turn);
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "chat.queued"));
            self.refresh_status();
            return Action::None;
        }
        if let Some(gate) = self.active_gate {
            if turn.has_attachments() {
                self.queue_chat_turn(turn);
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "input.steer.gate_deferred"),
                );
                self.refresh_status();
                return Action::None;
            }
            // A question at a gate asks for a model answer; it is not consent and
            // must never be reinterpreted as `Action::Revise`. The Director writer
            // is already parked at this point, so answer on a fresh read-only
            // surface while keeping the gate open.
            if matches!(
                umadev_agent::classify_running_input(&text),
                umadev_agent::RunningInputDisposition::Query
            ) {
                return self.begin_gate_query(text);
            }
            // ClarifyGate: non-"c" text is an answer (append to
            // answers file); "c" submits all answers + continues.
            if gate == Gate::ClarifyGate {
                if matches!(text.trim(), "c" | "C") {
                    self.active_gate = None;
                    self.gate_choice = None;
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::t(self.lang, "gate.clarify_saved").to_string(),
                    );
                    return Action::Continue(gate);
                }
                if !umadev_agent::is_explicit_clarification_answer(&text) {
                    self.queue_chat_turn(turn.clone());
                    self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.deferred"));
                    self.refresh_status();
                    return Action::None;
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
            // A2#2: run the free text through the SAME `classify_reply` the CLI
            // gate surfaces use, so "确认" / "通过" / "approve" / "ok" / "lgtm"
            // APPROVES the gate instead of being mistaken for a revision that
            // re-runs the whole producing block (the reported trap). The literal
            // `c` shortcut stays first (classify_reply would read it as a
            // revision); "取消" / "cancel" cancels; everything else revises.
            let approved = matches!(text.trim(), "c" | "C")
                || matches!(
                    umadev_agent::classify_reply(&text),
                    umadev_agent::GateOutcome::Approved
                );
            if approved {
                self.active_gate = None;
                self.gate_choice = None;
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
            if matches!(
                umadev_agent::classify_reply(&text),
                umadev_agent::GateOutcome::Cancelled
            ) {
                // An explicit cancel at the gate — same path as the picker's
                // Cancel option (the run is torn down, never a revision spawn).
                self.active_gate = None;
                self.gate_choice = None;
                return Action::Cancel;
            }
            // At a docs/preview gate, only a clear correction of the current
            // artifact is a revision. A later task or ambiguous conversational
            // turn is deferred for the model instead of accidentally re-running
            // this producing block.
            let disposition = umadev_agent::classify_running_input(&text);
            if matches!(disposition, umadev_agent::RunningInputDisposition::Deferred)
                && umadev_agent::is_explicit_later_work(&text)
            {
                self.queue_chat_turn(turn.clone());
                self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.deferred"));
                self.refresh_status();
                return Action::None;
            }
            if !matches!(disposition, umadev_agent::RunningInputDisposition::Steer) {
                // At docs/preview confirmation, ambiguity is safer as a read-only
                // question than as an indefinitely parked message. Only an
                // explicit current correction may revise, and only explicit
                // later-work wording enters the FIFO queue.
                return self.begin_gate_query(text);
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
            // Record the exact dispatched text so a terminal failure can dedup off it.
            self.last_dispatched_chat = Some(text.clone());
            self.pending_route_input = Some(turn);
            self.refresh_status();
            Action::Route(text)
        } else if !self.run_started {
            // Natural-language intent belongs to the selected supported base. UmaDev is
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
            // Record the exact dispatched text so a terminal failure can dedup off it.
            self.last_dispatched_chat = Some(text.clone());
            self.pending_route_input = Some(turn);
            self.refresh_status();
            Action::Route(text)
        } else {
            // Legacy pipeline mid-phase: use the same safe split as the director
            // path. Only a clear current-task correction may be injected at the
            // next gap; everything else becomes a later model-routed chat turn.
            match umadev_agent::classify_running_input(&text) {
                umadev_agent::RunningInputDisposition::Steer => {
                    self.queued_steer.push_back(text);
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "run.steer_queued"),
                    );
                }
                umadev_agent::RunningInputDisposition::Query
                | umadev_agent::RunningInputDisposition::Deferred => {
                    self.queue_chat_turn(turn.clone());
                    self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.deferred"));
                }
            }
            self.refresh_status();
            Action::None
        }
    }

    /// Move the structured-gate picker highlight by `delta` (wrapping). A no-op
    /// when no picker is active or it has no options (fail-open).
    fn gate_choice_move(&mut self, delta: isize) {
        let Some(choice) = self.gate_choice.as_ref() else {
            return;
        };
        let n = choice.options.len();
        if n == 0 {
            return;
        }
        // Wrap with signed arithmetic done in `isize`, then back to a 0..n index.
        let cur = isize::try_from(self.gate_choice_sel.min(n - 1)).unwrap_or(0);
        let n_i = isize::try_from(n).unwrap_or(1);
        let next = (cur + delta).rem_euclid(n_i);
        self.gate_choice_sel = usize::try_from(next).unwrap_or(0);
    }

    /// Confirm the picker option at `idx`, mapping the chosen [`GateDecision`]
    /// onto the EXISTING gate flow (no new decision machinery):
    /// - `Approve` → clear the gate + record trust, drive [`Action::Continue`]
    ///   (exactly what typing `c` does);
    /// - `Revise` / `AddMore` → keep the gate open and drop into the free-text
    ///   fallback (the next typed line drives the existing [`Action::Revise`]),
    ///   prompting the user for specifics;
    /// - `Cancel` → [`Action::Cancel`].
    ///
    /// **Fail-open:** an out-of-range index, no active picker, or no active gate
    /// → [`Action::None`] (the gate is left untouched).
    fn gate_choice_pick(&mut self, idx: usize) -> Action {
        if self.gate_query_in_flight {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "gate.query.busy"),
            );
            self.refresh_status();
            return Action::None;
        }
        let Some(option) = self
            .gate_choice
            .as_ref()
            .and_then(|c| c.options.get(idx))
            .cloned()
        else {
            return Action::None;
        };
        let Some(gate) = self.active_gate else {
            return Action::None;
        };
        // Echo the chosen option so the transcript records the decision (the
        // picker panel itself is transient). Localize the label key via `t()`,
        // which returns a literal verbatim and a known key localized.
        let label = umadev_i18n::t(self.lang, &option.label).to_string();
        self.push(ChatRole::You, label);
        // The picker is consumed regardless of the branch; the gate stays open
        // only for a revise/add-more free-text follow-up (re-checked below).
        self.gate_choice = None;
        self.gate_choice_sel = 0;
        match option.decision {
            GateDecision::Approve => {
                self.active_gate = None;
                let what = match gate {
                    Gate::DocsConfirm => umadev_i18n::t(self.lang, "gate.confirmed_docs"),
                    Gate::PreviewConfirm => umadev_i18n::t(self.lang, "gate.confirmed_preview"),
                    Gate::ClarifyGate => umadev_i18n::t(self.lang, "gate.confirmed_generic"),
                };
                self.push(ChatRole::UmaDev, format!("[ok] {what}"));
                // A picker approval builds trust for this gate, like a manual `c`.
                self.record_trust_pass(gate.id_str());
                Action::Continue(gate)
            }
            GateDecision::Revise | GateDecision::AddMore => {
                // A revise needs specifics → keep the gate open and hand off to
                // the free-text fallback; the user's next line drives the existing
                // revise path. A picked revise resets this gate's trust streak.
                self.record_trust_revision(gate.id_str());
                let prompt_key = if matches!(option.decision, GateDecision::AddMore) {
                    "gate.choice.add_more.prompt"
                } else {
                    "gate.choice.revise.prompt"
                };
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::t(self.lang, prompt_key).to_string(),
                );
                Action::None
            }
            GateDecision::Cancel => {
                self.active_gate = None;
                Action::Cancel
            }
        }
    }

    /// Append one chat turn to BOTH the working view ([`App::conversation`]) and
    /// the durable full transcript ([`App::full_transcript`]), then apply the
    /// synchronous safety net. The working view is what the base sees (and is
    /// later compacted); the full transcript is the append-only durable record
    /// that compaction never touches. Both grow together until compaction folds
    /// the working view's older turns into a summary.
    fn record_turn(&mut self, role: &str, content: String) {
        let msg = umadev_runtime::Message {
            role: role.to_string(),
            content,
        };
        self.conversation.push(msg.clone());
        self.full_transcript.push(msg);
        self.enforce_conversation_safety_net();
    }

    /// Record a user turn into [`App::conversation`] (the memory handed to the
    /// base on the next routed turn) and the durable [`App::full_transcript`].
    pub(crate) fn record_user_turn(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.record_turn("user", text.to_string());
        // Wave 5 / G11: mirror the FULL transcript to disk so a restart reopens it.
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
        self.record_turn("assistant", reply);
        if claims {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "chat.claims_unverified").to_string(),
            );
        }
    }

    /// The route ended without a usable reply (base init failed, an empty
    /// reply, or a hard error). This is a TERMINAL route outcome, so — like
    /// `record_agentic_done` — it stops the
    /// "thinking…" status; otherwise the animation would spin forever on a
    /// route that already failed. The human-readable reason is surfaced as a
    /// System note. Also clears `agentic_in_flight`: a failed agentic execution
    /// call flows through here, so this is its terminal cleanup too.
    /// Whether a failed-turn note carries evidence the BASE SESSION itself is dead / gone -
    /// a broken pipe on send (`os error 232` on Windows, `os error 32` on Unix), the base
    /// process having exited, or a `--resume` that hit "No conversation found" - as opposed to
    /// a content/tool error on a still-LIVE session. On session-death the stored base session
    /// id is a CORPSE: re-`--resume`-ing it every subsequent turn reproduces the failure
    /// forever (the reported "only the first turn works, then every turn fails"), so it must be
    /// invalidated to force a FRESH session (+ UmaDev own transcript replay) next turn.
    fn note_indicates_session_lost(note: &str) -> bool {
        const MARKERS: &[&str] = &[
            "no conversation found",
            "session ended",
            "session send",
            "broken pipe",
            "os error 232",
            "os error 32",
            "pipe is being closed",
            "管道",
            "epipe",
        ];
        let hay = note.to_lowercase();
        MARKERS.iter().any(|m| hay.contains(m))
    }

    pub(crate) fn record_route_failed(&mut self, note: String, origin: FailedRouteOrigin) {
        // Feature A — a turn that ran a while then errored out is still a terminal
        // outcome worth a beep (gated on elapsed, so a fast base-init failure is
        // silent). Arm before clearing the timer.
        self.arm_completion_bell(self.thinking_started.or(self.run_started_at));
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        // P5c: close any open reasoning block on a failed/aborted route.
        self.collapse_thinking_block();
        // P5a: a failed/aborted route ends any in-flight stream — drop its cache.
        self.stream_text_active = false;
        self.reset_stream_md_cache();
        // A failed director run does NOT hand a session back to chat (there is no
        // settled build session to continue) — just clear the in-flight marker
        // (and any stale gate-pause marker: the failed run resolves its pause).
        self.director_run_in_flight = false;
        self.director_gate_paused = false;
        self.pending_director_gate = None;
        // Settle the live task as Failed (a no-op for a plain chat-route failure,
        // which never registered a task).
        self.mark_active_task(TaskStatus::Failed);
        if origin == FailedRouteOrigin::Director {
            // A Director failure has no safe chat dedup key. Explicit `/run`
            // never dispatched `last_dispatched_chat`; a model-promoted run
            // crossed ownership boundaries after setting it.
            self.last_dispatched_chat = None;
        }
        // If the failure is the BASE SESSION dying (broken pipe / process exit / a `--resume`
        // that found no conversation), the stored session id is a corpse - re-resuming it on
        // every subsequent turn reproduces the failure forever. Invalidate it so the NEXT turn
        // opens a FRESH base session and UmaDev replays its own bounded transcript for context
        // (the fresh-open path already exists; it was just never reached because the id
        // survived the failure).
        if Self::note_indicates_session_lost(&note) {
            self.chat_session_id = None;
            self.chat_resume_identity = None;
        }
        // Close the dispatched user turn in durable memory before any queued
        // follow-up is drained. Without this boundary a fresh/resumed base sees
        // `user A -> user B` and may retry A's partial side effects as though it
        // never terminated.
        self.record_turn(
            "assistant",
            format!(
                "[control: the preceding turn failed and must not be resumed implicitly]\n{note}"
            ),
        );
        self.persist_chat();
        self.refresh_status();
        self.push(ChatRole::System, note);
        if origin == FailedRouteOrigin::Chat {
            self.drop_failed_route_duplicate_queued_chat();
        }
    }

    /// Settle an explicitly cancelled pre-session authentication flow without
    /// treating it as a model/base turn failure. The already-rendered user turn
    /// remains paired with a durable control boundary, while its exact typed
    /// blocks are restored into the editor and merged with any newer draft.
    pub(crate) fn record_auth_cancelled(&mut self, turn: SubmittedTurn, note: String) {
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.stream_text_active = false;
        self.stream_tool_batch = None;
        self.collapse_thinking_block();
        self.reset_stream_md_cache();
        self.director_run_in_flight = false;
        self.director_gate_paused = false;
        self.pending_director_gate = None;
        self.last_dispatched_chat = None;
        self.mark_active_task(TaskStatus::Stopped);
        self.record_turn(
            "assistant",
            format!("[control: authentication cancelled; no user turn was sent]\n{note}"),
        );
        self.push(ChatRole::System, note);
        self.restore_rejected_turn(turn);
        self.persist_chat();
        self.refresh_status();
    }

    /// Settle a defensive Director entry that hit the Plan/read-only ceiling.
    /// This is intentionally distinct from both `record_agentic_done` and
    /// `record_route_failed`: no build completed, no failure occurred, and no
    /// completion bell/card or resumable build session should be created.
    pub(crate) fn record_run_not_executed(&mut self) {
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.stream_text_active = false;
        self.stream_tool_batch = None;
        self.collapse_thinking_block();
        self.reset_stream_md_cache();
        self.director_run_in_flight = false;
        self.director_gate_paused = false;
        self.pending_director_gate = None;
        // If a programmatic caller registered a task before reaching the inner
        // ceiling, preserve the honest lifecycle: it was stopped before work,
        // never completed.
        self.mark_active_task(TaskStatus::Stopped);
        self.refresh_status();
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
        base_resume_identity: Option<BaseResumeIdentity>,
    ) {
        // Feature A — a long agentic turn just settled; alert the (possibly away)
        // user. Arm BEFORE clearing `thinking_started`, gated on its elapsed.
        self.arm_completion_bell(self.thinking_started);
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
        if let (Some(id), Some(identity)) = (base_session_id, base_resume_identity) {
            if !id.trim().is_empty() {
                self.chat_session_id = Some(id);
                self.chat_resume_identity = Some(identity);
                self.host_chat_session_active = true;
            }
        }
        // P5c: a turn that ends still inside a reasoning block collapses it now.
        self.collapse_thinking_block();
        // P5a: the streamed turn is settled — drop the stable-prefix cache so the
        // final, complete body renders through one clean whole-body pass (the
        // guaranteed-consistent path) and the NEXT stream starts fresh.
        self.reset_stream_md_cache();
        // Wave 5 deliverable 2 — unify chat ↔ director memory. The exact native
        // session id arrived in `base_session_id` above and is now pinned on App;
        // this one-shot flag tells the next turn not to mint a competing id. Only a
        // real director build sets it (plain chat / explain / quick-edit carry
        // `director_build = false`). Bases without resume support fall back to the
        // bounded UmaDev transcript. The in-flight marker is always cleared.
        self.director_run_in_flight = false;
        // A settled run is no longer parked at any gate (safety: a stale pause
        // marker must never survive into the next run's routing).
        self.director_gate_paused = false;
        self.pending_director_gate = None;
        if director_build {
            self.run_session_handed_to_chat = true;
            // The director build settled cleanly → mark its task Done.
            self.mark_active_task(TaskStatus::Done);
        }
        self.refresh_status();
        let reply = reply.trim().to_string();
        if reply.is_empty() {
            // The base produced only tool calls / a side-effect with no closing
            // prose. Still a clean finish — leave the streamed activity as the
            // record, and drop a short marker so the turn reads as completed.
            let marker = umadev_i18n::t(self.lang, "agentic.done").to_string();
            self.push(ChatRole::System, marker.clone());
            // Durability: a tool-only final turn must still be recorded + persisted.
            // Previously this branch early-returned BEFORE `record_turn` /
            // `persist_chat`, so the exchange (the user turn + this tool-only
            // completion) was never re-saved after the user turn's own persist — a
            // close before any later prose turn silently lost it from the on-disk
            // chat. Record the completion marker as the assistant turn (keeps the
            // user↔assistant pairing in durable memory) and persist the transcript
            // + display snapshot so a relaunch restores the turn.
            self.record_turn("assistant", marker);
            self.persist_chat();
            return;
        }
        self.record_turn("assistant", reply);
        // Wave 5 / G11: persist after the assistant turn lands so the saved chat
        // holds complete user→assistant exchanges.
        self.persist_chat();
    }

    /// A DIRECTOR build parked at a spec-MUST confirmation gate (A1-GAP1) — the
    /// terminal `RunPausedAtGate` decision's recorder. Clears the in-flight
    /// "thinking…" state (the run task has settled; a fresh session resumes it on
    /// approval) and arms [`Self::director_gate_paused`] so the gate approval /
    /// revision paths resume the DIRECTOR plan. A `GateOpened` event arriving
    /// before this terminal decision is intentionally staged, then activated here
    /// only after the writer session is known to have ended.
    pub(crate) fn record_run_paused_at_gate(&mut self, gate: Gate) {
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.stream_text_active = false;
        self.stream_tool_batch = None;
        self.collapse_thinking_block();
        self.reset_stream_md_cache();
        // The run task is gone; what remains is the parked plan on disk. The
        // pause marker (not the in-flight marker) carries the state forward.
        self.director_run_in_flight = false;
        self.director_gate_paused = true;
        // Activate only after the writer session has ended. Replaying the staged
        // event here preserves the choice/card behaviour without exposing a live
        // approval surface during session teardown. A missing staged event fails
        // open to an optionless gate rather than losing the required checkpoint.
        let choice = self
            .pending_director_gate
            .take()
            .filter(|(staged, _)| *staged == gate)
            .and_then(|(_, choice)| choice);
        self.apply_engine(EngineEvent::GateOpened { gate, choice });
        self.refresh_status();
    }

    /// A DIRECTOR build parked because its wall-clock budget was exhausted while
    /// resumable steps remained (Stage 1/2) — the terminal `RunPausedAtBudget`
    /// decision's recorder. Mirrors [`Self::record_run_paused_at_gate`] but for a
    /// PAUSE with no gate: it clears the in-flight "thinking…" state and the live
    /// counters (so the timer stops and the status reads `[paused]`, NEVER
    /// `[aborted]`), arms [`Self::budget_paused`] so `run_state` reads
    /// [`RunState::PausedAtBudget`], keeps the plan panel visible in a FROZEN
    /// (interrupted) form so the user can see what was saved, and pushes the
    /// `/continue` resume hint carrying `done/total`.
    ///
    /// This deliberately does NOT route through [`Self::mark_block_aborted`]: a budget
    /// pause is a resumable settle, not an honest hard abort — the plan is intact on
    /// disk and `/continue` re-drives only the remaining steps.
    pub(crate) fn record_run_paused_at_budget(&mut self, done: usize, total: usize) {
        // An away user should hear that the run parked (same as the abort/deliver
        // paths). Arm before the timers are cleared, gated on how long it had run.
        self.arm_completion_bell(self.run_started_at.or(self.thinking_started));
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.long_op_in_progress = false;
        self.stream_text_active = false;
        self.stream_tool_batch = None;
        self.collapse_thinking_block();
        self.reset_stream_md_cache();
        // The writer session is gone; what remains is the parked plan on disk.
        self.director_run_in_flight = false;
        self.budget_paused = true;
        // Stop every live counter so the status bar reflects a real paused state.
        self.run_started_at = None;
        self.phase_started_at = None;
        self.last_output_at = None;
        self.transient_status = None;
        // Keep the plan panel: drop the LIVE panel, then bring the saved plan back in
        // a FROZEN (interrupted) form so the user sees the completed / remaining steps
        // and that `/continue` resumes them. Fail-open (no readable plan → empty).
        self.clear_live_panels();
        self.rehydrate_frozen_plan_now();
        // The one-line resume hint carrying where the run parked (done/total steps).
        self.push(
            ChatRole::System,
            umadev_i18n::tf(
                self.lang,
                "run.budget_pause_resume_hint",
                &[&done.to_string(), &total.to_string()],
            ),
        );
        // A parked run fires no gate/completion, so drain any queued steer (same as
        // the abort path) so its "queued N" chip can't stay falsely lit forever.
        if !self.queued_steer.is_empty() {
            let text = self.queued_steer.drain(..).collect::<Vec<_>>().join("\n");
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "run.queued_dropped", &[&text]),
            );
        }
        self.refresh_status();
    }

    /// Settle a read-only answer asked while a gate remains open. The streamed
    /// body is already visible; this records it in durable model memory and clears
    /// only the query spinner — never the gate or its Director pause marker.
    pub(crate) fn record_gate_query_done(&mut self, epoch: u64, reply: String) -> bool {
        if self.active_gate_query_epoch != Some(epoch) {
            return false;
        }
        self.active_gate_query_epoch = None;
        self.gate_query_in_flight = false;
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.stream_text_active = false;
        self.stream_tool_batch = None;
        self.collapse_thinking_block();
        self.reset_stream_md_cache();
        let reply = reply.trim().to_string();
        if !reply.is_empty() {
            self.push(ChatRole::UmaDev, reply.clone());
            self.record_turn("assistant", reply);
            self.persist_chat();
        }
        self.refresh_status();
        true
    }

    /// Settle a failed read-only gate question without resolving the gate. The
    /// user can retry, approve, revise, or cancel immediately.
    pub(crate) fn record_gate_query_failed(&mut self, epoch: u64, note: String) -> bool {
        if self.active_gate_query_epoch != Some(epoch) {
            return false;
        }
        self.active_gate_query_epoch = None;
        self.gate_query_in_flight = false;
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.stream_text_active = false;
        self.collapse_thinking_block();
        self.reset_stream_md_cache();
        self.push(ChatRole::System, note.clone());
        self.record_turn(
            "assistant",
            format!("[gate question failed; gate remains open] {note}"),
        );
        self.persist_chat();
        self.refresh_status();
        true
    }

    /// Mark a confirmed `/deploy` as the sole cancellable workspace task.
    pub(crate) fn begin_deploy(&mut self) {
        self.thinking = true;
        self.thinking_started = Some(std::time::Instant::now());
        self.agentic_in_flight = true;
        self.tool_in_progress = true;
        self.refresh_status();
    }

    /// Settle a tracked deploy and release its single-task guard.
    pub(crate) fn record_deploy_done(&mut self, succeeded: bool) {
        self.arm_completion_bell(self.thinking_started);
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.record_turn(
            "assistant",
            format!(
                "[control: deploy task settled — {}]",
                if succeeded {
                    "deployed"
                } else {
                    "not deployed"
                }
            ),
        );
        self.persist_chat();
        self.refresh_status();
    }

    /// Surface steering that never reached a step boundary when a director run
    /// settled (A2#4 — a queued directive must never be dropped silently, and the
    /// queued chip must never stick). Folds the shared intake's `leftover` with
    /// anything still parked in [`Self::queued_steer`], and pushes ONE honest
    /// `run.queued_unsent` note. No-op when both are empty (the common case).
    pub(crate) fn surface_unsent_steer(&mut self, mut leftover: Vec<String>) {
        leftover.extend(self.queued_steer.drain(..));
        leftover.retain(|s| !s.trim().is_empty());
        if leftover.is_empty() {
            return;
        }
        let text = leftover.join("\n");
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "run.queued_unsent", &[&text]),
        );
        self.record_turn(
            "assistant",
            format!("[control: user steering was not applied before settlement]\n{text}"),
        );
        self.persist_chat();
    }

    fn queue_chat_turn(&mut self, turn: SubmittedTurn) {
        self.align_queued_dispatch_kinds();
        self.queued_chat.push_back(turn.text.clone());
        self.queued_turn_inputs.push_back(turn);
        self.queued_dispatch_kinds
            .push_back(QueuedResidentKind::RoutedChat);
    }

    fn queue_native_command(&mut self, payload: String) {
        self.align_queued_dispatch_kinds();
        self.queued_chat.push_back(payload.clone());
        self.queued_turn_inputs
            .push_back(SubmittedTurn::text(payload));
        self.queued_dispatch_kinds
            .push_back(QueuedResidentKind::NativeCommand);
    }

    fn align_queued_dispatch_kinds(&mut self) {
        self.queued_dispatch_kinds.truncate(self.queued_chat.len());
        while self.queued_dispatch_kinds.len() < self.queued_chat.len() {
            self.queued_dispatch_kinds
                .push_back(QueuedResidentKind::RoutedChat);
        }
    }

    fn take_queued_turn_input_front(&mut self, text: &str) -> Option<SubmittedTurn> {
        if self
            .queued_turn_inputs
            .front()
            .is_some_and(|turn| turn.text == text)
        {
            return self.queued_turn_inputs.pop_front();
        }
        // Fail-open compatibility for hand-built test/application state from
        // before snapshots became positionally complete.
        let position = self
            .queued_turn_inputs
            .iter()
            .position(|turn| turn.text == text)?;
        self.queued_turn_inputs.remove(position)
    }

    fn take_queued_turn_input_back(&mut self, text: &str) -> Option<SubmittedTurn> {
        if self
            .queued_turn_inputs
            .back()
            .is_some_and(|turn| turn.text == text)
        {
            return self.queued_turn_inputs.pop_back();
        }
        let position = self
            .queued_turn_inputs
            .iter()
            .rposition(|turn| turn.text == text)?;
        self.queued_turn_inputs.remove(position)
    }

    /// Queue a live input that the active vendor session cannot guarantee as a
    /// same-turn steer. The user bubble already exists; this only preserves the
    /// typed snapshot and publishes an honest transport-specific status.
    pub(crate) fn defer_live_input(&mut self, turn: SubmittedTurn, note_key: &str) {
        self.queue_chat_turn(turn);
        self.push(ChatRole::System, umadev_i18n::t(self.lang, note_key));
        self.refresh_status();
    }

    /// Record a same-turn steer only after its protocol write returned a receipt.
    pub(crate) fn record_live_input_delivered(&mut self, text: &str) {
        self.record_user_turn(text);
        self.refresh_status();
    }

    pub(crate) fn reject_live_input(&mut self, turn: SubmittedTurn, note: String) {
        self.push(ChatRole::System, note);
        self.restore_rejected_turn(turn);
        self.refresh_status();
    }

    /// Keep the authoritative queue rows and restore an unsaved edit if its
    /// protocol mutation failed before a replacement snapshot arrived.
    pub(crate) fn reject_prompt_queue_mutation(
        &mut self,
        mutation: PromptQueueMutation,
        note: String,
    ) {
        self.prompt_queue.reject_pending();
        self.push(ChatRole::System, note);
        if let PromptQueueMutation::Edit { new_text, .. } = mutation {
            self.restore_rejected_turn(SubmittedTurn::text(new_text));
        }
        self.refresh_status();
    }

    /// Put a protocol-rejected structured turn back into the editor without
    /// losing text the user typed while the rejection was in flight.
    pub(crate) fn restore_rejected_turn(&mut self, mut turn: SubmittedTurn) {
        if !self.input.trim().is_empty() {
            let draft = self.compose_submitted_turn(&self.input);
            append_text_block(&mut turn.input.blocks, "\n");
            turn.input.blocks.extend(draft.input.blocks);
            turn.text.push('\n');
            turn.text.push_str(&draft.text);
        }
        // The corrected turn normally keeps the same path-free chip text. It is
        // an explicit user retry and must remain in the editor.
        self.restore_submitted_turn(turn);
    }

    /// Restore a rejected/not-yet-dispatched structured turn into the editor.
    /// Paths stay only in backing vectors; the visible input contains chips.
    pub(crate) fn restore_submitted_turn(&mut self, turn: SubmittedTurn) {
        self.clear_input();
        let mut input = String::new();
        for block in turn.input.blocks {
            match block {
                TurnInputBlock::Text { text } => input.push_str(&text),
                TurnInputBlock::Image { path } => {
                    self.attachments.push(path);
                    input.push_str(&self.image_chip(self.attachments.len()));
                }
                TurnInputBlock::File { path, .. } => {
                    self.file_attachments.push(path);
                    input.push_str(&self.file_chip(self.file_attachments.len()));
                }
            }
        }
        self.input = input;
        self.input_cursor = self.input_len();
        self.request_full_repaint();
    }

    /// Take the structured snapshot paired with an immediate/queued Route action.
    /// Text-only programmatic routes retain the conservative one-text-block shape.
    pub(crate) fn take_route_input(&mut self, text: &str) -> SubmittedTurn {
        self.pending_route_input
            .take()
            .filter(|turn| turn.text == text)
            .unwrap_or_else(|| SubmittedTurn::text(text.to_string()))
    }

    /// Drop input/routing state that belongs only to the current conversation.
    /// Called at every explicit conversation replacement (`/clear`, `/resume`).
    fn clear_transient_routing_state(&mut self) {
        self.queued_steer.clear();
        self.pending_steer = None;
        self.queued_chat.clear();
        self.queued_dispatch_kinds.clear();
        self.queued_turn_inputs.clear();
        self.pending_route_input = None;
        self.route_backlog_len = 0;
        self.last_dispatched_chat = None;
    }

    /// Pop the oldest chat turn parked by [`submit_text`] while a route was in
    /// flight, if any, and record it into conversation memory AT THIS MOMENT — the
    /// instant it actually fires — so the base sees user turns in true send order
    /// with no dangling "user said X" left behind by an interrupted queue. The
    /// event loop fires it as the NEXT route only after the current route result
    /// has landed, keeping same-session routing strictly serial (never two base
    /// subprocesses resuming one `session_id` at once).
    pub(crate) fn take_next_queued_dispatch(&mut self) -> Option<ResidentDispatch> {
        self.align_queued_dispatch_kinds();
        let text = self.queued_chat.pop_front()?;
        let kind = self
            .queued_dispatch_kinds
            .pop_front()
            .unwrap_or(QueuedResidentKind::RoutedChat);
        let input = self.take_queued_turn_input_front(&text);
        self.record_user_turn(&text);
        match kind {
            QueuedResidentKind::RoutedChat => {
                self.pending_route_input = input;
                // This drained turn is now the dispatched one → dedup a later
                // route failure off it.
                self.last_dispatched_chat = Some(text.clone());
                Some(ResidentDispatch::RoutedChat(text))
            }
            QueuedResidentKind::NativeCommand => {
                self.pending_route_input = None;
                self.last_dispatched_chat = None;
                Some(ResidentDispatch::NativeCommand(text))
            }
        }
    }

    /// Compatibility view used by chat-only tests and older in-crate callers.
    #[cfg(test)]
    pub(crate) fn take_next_queued_chat(&mut self) -> Option<String> {
        match self.take_next_queued_dispatch()? {
            ResidentDispatch::RoutedChat(text) | ResidentDispatch::NativeCommand(text) => {
                Some(text)
            }
        }
    }

    /// Snapshot the FIFO backlog that predates a newly dispatched model route.
    /// Called immediately before its asynchronous classifier is spawned.
    pub(crate) fn begin_route_dispatch(&mut self) {
        self.route_backlog_len = self.queued_chat.len();
    }

    /// Reclassify turns submitted while the model was still deciding whether the
    /// current request needed Director. That classification interval can last a
    /// few seconds; an explicit correction typed there belongs to the run once the
    /// model crosses the Director boundary, while questions/future work must remain
    /// ordinary deferred chat. Preserves FIFO within both lanes.
    pub(crate) fn promote_queued_inputs_for_director(&mut self) {
        self.align_queued_dispatch_kinds();
        let boundary = self.route_backlog_len.min(self.queued_chat.len());
        if boundary == self.queued_chat.len() {
            return;
        }
        let snapshots_aligned = self.queued_turn_inputs.len() == self.queued_chat.len()
            && self
                .queued_turn_inputs
                .iter()
                .zip(self.queued_chat.iter())
                .all(|(turn, text)| &turn.text == text);
        // Older queued turns are independent future work. Only messages appended
        // during this route's classification interval can steer this Director.
        let mut route_tail = self.queued_chat.split_off(boundary);
        let mut route_kinds = self.queued_dispatch_kinds.split_off(boundary);
        let mut route_turns = if snapshots_aligned {
            self.queued_turn_inputs.split_off(boundary)
        } else {
            VecDeque::new()
        };
        let mut deferred = VecDeque::with_capacity(route_tail.len());
        let mut deferred_turns = VecDeque::with_capacity(route_tail.len());
        let mut deferred_kinds = VecDeque::with_capacity(route_tail.len());
        while let Some(text) = route_tail.pop_front() {
            let kind = route_kinds
                .pop_front()
                .unwrap_or(QueuedResidentKind::RoutedChat);
            let turn = if snapshots_aligned {
                route_turns.pop_front()
            } else {
                self.take_queued_turn_input_front(&text)
            };
            let has_attachments = turn.as_ref().is_some_and(SubmittedTurn::has_attachments);
            if kind == QueuedResidentKind::RoutedChat
                && !has_attachments
                && matches!(
                    umadev_agent::classify_running_input(&text),
                    umadev_agent::RunningInputDisposition::Steer
                )
            {
                self.queued_steer.push_back(text);
            } else {
                deferred.push_back(text);
                deferred_kinds.push_back(kind);
                if let Some(turn) = turn {
                    deferred_turns.push_back(turn);
                }
            }
        }
        self.queued_chat.extend(deferred);
        self.queued_dispatch_kinds.extend(deferred_kinds);
        self.queued_turn_inputs.extend(deferred_turns);
        self.route_backlog_len = self.queued_chat.len();
        self.refresh_status();
    }

    /// After a route failure, drop leading queued chat turns that are exact
    /// duplicates of the user turn that just failed. This catches a common TUI
    /// race/user gesture: pressing Enter twice (or re-sending the same text while
    /// the first turn is still "thinking") used to make the failed turn auto-fire
    /// again as soon as the failure note landed, reading as "it skipped thinking
    /// and dumped old output". Different queued turns still drain normally.
    pub(crate) fn drop_failed_route_duplicate_queued_chat(&mut self) -> usize {
        self.align_queued_dispatch_kinds();
        // Key off the EXACT text just dispatched to the base (the turn that failed),
        // not the last "user" turn in conversation memory. The conversation heuristic
        // was fragile: a relayed / reframed turn, or an intervening record, could make
        // the "last user turn" differ from what was actually sent, so a real duplicate
        // slipped through (or a distinct turn got wrongly dropped).
        let Some(failed_turn) = self
            .last_dispatched_chat
            .as_deref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            return 0;
        };
        let mut dropped = 0usize;
        while self.queued_dispatch_kinds.front() == Some(&QueuedResidentKind::RoutedChat)
            && self
                .queued_chat
                .front()
                .is_some_and(|text| text.trim() == failed_turn)
        {
            if let Some(text) = self.queued_chat.pop_front() {
                let _ = self.take_queued_turn_input_front(&text);
            }
            let _ = self.queued_dispatch_kinds.pop_front();
            dropped += 1;
        }
        if dropped > 0 {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "chat.queued_duplicate_skipped").to_string(),
            );
            self.refresh_status();
        }
        dropped
    }

    /// I6 — pull the MOST RECENT chat turn parked behind the in-flight one
    /// ([`queued_chat`]) back into the input box for editing, popping it from the
    /// queue. Lets the user fix (or drop) a queued message before it sends,
    /// instead of it being uneditable until it fires. Returns `true` when a
    /// queued message was recalled. Fail-open: a no-op returning `false` when the
    /// queue is empty (the caller then falls back to shell-history recall / the
    /// rewind gesture). The newest is popped because it is the one the user just
    /// typed and most likely wants to correct. Memory stays clean — a queued turn
    /// is only recorded into conversation memory when it actually FIRES
    /// (`take_next_queued_chat`), never at queue time, so this pop leaves no
    /// dangling record.
    fn recall_queued_chat(&mut self) -> bool {
        self.align_queued_dispatch_kinds();
        let Some(text) = self.queued_chat.pop_back() else {
            return false;
        };
        let _ = self.queued_dispatch_kinds.pop_back();
        if let Some(turn) = self.take_queued_turn_input_back(&text) {
            self.restore_submitted_turn(turn);
            self.refresh_status();
            return true;
        }
        self.input = text;
        self.input_cursor = self.input_len();
        // Land as a clean fresh draft: not mid history-recall, no armed
        // quit/rewind gesture carried over.
        self.input_history_idx = None;
        self.pending_quit_confirm = false;
        self.pending_rewind = false;
        // The "queued N" chip count just dropped — keep the status line honest.
        self.refresh_status();
        true
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

    /// Synchronous anti-unbounded safety net on the working view: drop the oldest
    /// messages beyond [`CONVERSATION_HARD_CAP`]. Real compaction triggers far
    /// below this (at [`COMPACTION_TOKEN_BUDGET`]), so this only ever fires when
    /// compaction is impossible (offline / breaker tripped). The full transcript
    /// on disk is never trimmed, so this bounds the live prompt without losing
    /// durable history.
    fn enforce_conversation_safety_net(&mut self) {
        // Never drop from the FRONT while a fold is in flight: the in-flight job
        // summarises a snapshot of the leading prefix and splices by count, so the
        // prefix indices must stay stable until it lands. The job always reports
        // back (and shrinks the buffer on apply / FIFO-falls-back on failure), so
        // this temporary overflow is short-lived and bounded.
        if self.compaction_in_flight {
            return;
        }
        let len = self.conversation.len();
        if len > CONVERSATION_HARD_CAP {
            self.conversation.drain(0..len - CONVERSATION_HARD_CAP);
        }
    }

    /// Best available measurement of the CURRENT context size (tokens the base
    /// just read) — the NUMERATOR of the context-usage gauge. This deliberately
    /// uses only the base's real last-turn input-token report. A chars/4
    /// transcript estimate is useful for compaction internals, but too indirect
    /// for a UI gauge labelled "context", so the gauge stays hidden until usage
    /// lands. Pure read, fail-open.
    #[must_use]
    pub(crate) fn context_used_tokens(&self) -> Option<u64> {
        self.session_usage
            .exact_context_input()
            .filter(|tokens| *tokens > 0)
    }

    /// The context-window DENOMINATOR for the active base/model, or `None` when the
    /// base does not expose an EXACT window in its own configuration. Pure read.
    #[must_use]
    pub(crate) fn context_window_tokens(&self) -> Option<u64> {
        // The gauge denominator is shown ONLY when the base's own config exposes an
        // exact context window (today: OpenCode provider metadata). UmaDev owns no
        // model and cannot read a claude-code / codex window, and inferring one from
        // a model-name table would drift and mislead when a base routes to a
        // third-party / local model — so for those bases the window stays hidden and
        // only the real model NAME is shown (see `model_meta_text`). Honest over
        // decorative: no fabricated or guessed denominator, ever.
        self.base_context_window
    }

    /// Current context occupancy as a whole percent (`used / window`), or `None`
    /// when either the numerator or the denominator is unavailable — fail-open, so
    /// the gauge/nudge never act on a fabricated number. Pure read.
    #[must_use]
    pub(crate) fn context_usage_pct(&self) -> Option<u16> {
        let used = self.context_used_tokens()?;
        let total = self.context_window_tokens()?;
        Some(context_usage_pct(used, total))
    }

    /// Surface the proactive `/compact` nudge ONCE when context occupancy first
    /// crosses [`CONTEXT_NUDGE_PCT`] — the pre-emptive version of the reactive
    /// `BaseFailure::Context` remedy, fired before the base overflows. Bounded:
    /// the [`Self::context_nudge_shown`] latch fires the one-line System hint a
    /// single time per crossing and re-arms once occupancy drops back below the
    /// threshold (e.g. after a `/compact`). No usage / unknown model → no-op
    /// (fail-open, never a spurious nudge). Called after each real `TurnUsage`.
    pub(crate) fn maybe_nudge_compaction(&mut self) {
        let Some(pct) = self.context_usage_pct() else {
            return;
        };
        if pct < CONTEXT_NUDGE_PCT {
            // Back under the line — re-arm so the next crossing can nudge again.
            self.context_nudge_shown = false;
            return;
        }
        if self.context_nudge_shown {
            return; // already nudged for this crossing — don't spam.
        }
        self.context_nudge_shown = true;
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "compact.nudge", &[&pct.to_string()]),
        );
    }

    /// Whether the working transcript is over the compaction token budget and a
    /// fold of at least [`umadev_agent::compaction::MIN_FOLD`] older messages is
    /// available — the deterministic auto-compaction trigger. Pure read.
    #[must_use]
    pub(crate) fn should_auto_compact(&self) -> bool {
        if self.compaction_in_flight || self.compaction_breaker.tripped() {
            return false;
        }
        umadev_agent::compaction::plan(
            &self.conversation,
            COMPACTION_TOKEN_BUDGET,
            COMPACTION_TAIL_BUDGET,
            COMPACTION_MIN_TAIL,
        )
        .is_some()
    }

    /// Begin an **auto** compaction if the working transcript is over budget:
    /// snapshot the older prefix to summarise, mark a job in flight, and return it
    /// for the event loop to drive on a forked base. `None` when not over budget,
    /// already compacting, or the breaker is tripped (the deterministic FIFO floor
    /// keeps the buffer bounded in that case).
    pub(crate) fn begin_auto_compaction(&mut self) -> Option<CompactionJob> {
        if !self.should_auto_compact() {
            return None;
        }
        let plan = umadev_agent::compaction::plan(
            &self.conversation,
            COMPACTION_TOKEN_BUDGET,
            COMPACTION_TAIL_BUDGET,
            COMPACTION_MIN_TAIL,
        )?;
        Some(self.start_compaction(plan.fold_count))
    }

    /// Begin a **manual** (`/compact`) compaction: fold everything except the
    /// recent verbatim tail regardless of the token budget, as long as there is a
    /// worthwhile prefix to fold. `None` when already compacting or there is too
    /// little to fold (the caller surfaces `compact.too_short`).
    pub(crate) fn begin_manual_compaction(&mut self) -> Option<CompactionJob> {
        if self.compaction_in_flight {
            return None;
        }
        let len = self.conversation.len();
        let tail = COMPACTION_MIN_TAIL.min(len);
        let fold_count = len.saturating_sub(tail);
        if fold_count < umadev_agent::compaction::MIN_FOLD {
            return None;
        }
        Some(self.start_compaction(fold_count))
    }

    /// Snapshot the fold prefix + generation and mark a compaction in flight.
    fn start_compaction(&mut self, fold_count: usize) -> CompactionJob {
        self.compaction_in_flight = true;
        CompactionJob {
            folded: self.conversation[..fold_count].to_vec(),
            fold_count,
            generation: self.conversation_generation,
        }
    }

    /// Apply a SUCCESSFUL structured summary: replace the folded older prefix in
    /// the working view with one summary block (a `user`-role grounding note +
    /// the "full history preserved" marker), keeping the recent tail verbatim. The
    /// full transcript on disk is untouched. Resets the breaker.
    ///
    /// Stale-guarded: a job whose generation no longer matches (a `/clear` /
    /// `/resume` happened since it started) is dropped without mutating anything.
    pub(crate) fn apply_compaction(&mut self, summary: &str, fold_count: usize, generation: u64) {
        if generation != self.conversation_generation {
            return; // stale — the conversation was cleared / resumed meanwhile.
        }
        self.compaction_in_flight = false;
        self.compaction_breaker.record_success();
        if fold_count < umadev_agent::compaction::MIN_FOLD || fold_count > self.conversation.len() {
            return;
        }
        let header = umadev_i18n::tf(
            self.lang,
            "compact.summary_block",
            &[&fold_count.to_string()],
        );
        let marker = umadev_i18n::t(self.lang, "compact.full_history_preserved");
        let block = umadev_runtime::Message {
            role: "user".to_string(),
            content: format!("{header}\n{}\n\n{marker}", summary.trim()),
        };
        // Keep the recent tail; replace the folded prefix with the single block.
        let tail = self.conversation.split_off(fold_count);
        self.conversation = std::iter::once(block).chain(tail).collect();
        // NB: `full_transcript` (the on-disk record) is deliberately NOT touched.
        // `persist_chat` writes the full transcript, so re-persisting is harmless
        // and keeps the saved base-session id / timestamp fresh.
        self.persist_chat();
        // The RESIDENT base session (the live host-CLI process the event loop keeps
        // alive across this chat) still holds the FULL pre-compaction history in its
        // OWN process memory, and THAT is what actually drives each turn — so folding
        // only `conversation` here is cosmetic to the base unless we also close it AND
        // break the base-session id. Mirror `/clear`'s base-session break (minus the
        // conversation wipe + `chat_id` re-mint): drop the base's own session pin so
        // the next turn opens a TRULY FRESH session that front-loads only the COMPACTED
        // transcript (via `first_chat_directive`). Without clearing `chat_session_id`
        // the fresh open would RESUME the base's full uncompacted native history — the
        // compacted transcript and the full history would coexist and the fold would be
        // defeated (history bleed + stale-build misroute persist). Fail-open: a missed
        // close/break at worst leaves a stale session one extra turn.
        self.host_chat_session_active = false;
        self.chat_session_id = None;
        self.chat_resume_identity = None;
        self.run_session_handed_to_chat = false;
        self.chat_session_dirty = true;
        self.reset_base_session_state();
        self.push(
            ChatRole::System,
            umadev_i18n::tf(
                self.lang,
                "compact.compacted_notice",
                &[&fold_count.to_string()],
            ),
        );
    }

    /// Fail-open path when the summary `complete()` failed / was empty / the base
    /// was offline: advance the circuit breaker and fall back to the original FIFO
    /// behaviour — drop the oldest down to [`CONVERSATION_CAP`] so the working
    /// prompt stays bounded. The full transcript on disk is untouched, so nothing
    /// durable is lost; the conversation is never corrupted.
    pub(crate) fn fail_compaction(&mut self, generation: u64) {
        if generation != self.conversation_generation {
            return; // stale — never trim or mutate the new conversation's breaker.
        }
        self.compaction_in_flight = false;
        self.compaction_breaker.record_failure();
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
        // The gate line must reflect the ACTIVE trust mode, not a hard-coded
        // "auto-pass": in guarded mode the two pipeline gates pause for approval
        // (the run receives effective_trust_mode()), so a fixed "gates auto-pass"
        // sentence contradicted the guarded chip.
        let gate_key = match self.effective_trust_mode() {
            umadev_agent::TrustMode::Auto => "run.gate_line.auto",
            umadev_agent::TrustMode::Guarded => "run.gate_line.guarded",
            umadev_agent::TrustMode::Plan => "run.gate_line.plan",
        };
        let gate_line = umadev_i18n::t(self.lang, gate_key);
        let plan = umadev_i18n::tf(
            self.lang,
            "run.preflight_plan",
            &[text, &self.backend_label, ds, tpl, gate_line],
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
        self.gate_choice = None;
        // A fresh run supersedes any parked director gate (its plan is being
        // replaced) — the stale pause marker must not hijack the next approval.
        self.director_gate_paused = false;
        self.pending_director_gate = None;
        self.gate_query_in_flight = false;
        self.active_gate_query_epoch = None;
        // A fresh run starts UNARMED: interrupt_armed_at is only cleared by the confirming
        // second Esc, so without this a stale arm from a PREVIOUS run (still inside its 3s
        // window) made a single Esc on the new run cancel it immediately - defeating the
        // deliberate double-press guard.
        self.interrupt_armed_at = None;
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
        // A cleared panel is never frozen — a transient rehydrate re-arms this
        // right after calling here; every other terminal/reset path leaves it off.
        self.plan_frozen = false;
        self.critic_verdicts.clear();
        self.critics_collapsed = false;
        self.critic_round_open = false;
        self.handoffs.clear();
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

    fn append_thinking_delta(&mut self, delta: &str) {
        let Some(idx) = self.thinking_block_idx else {
            return;
        };
        let still_placeholder = matches!(
            self.history.get(idx),
            Some(message) if message.role == ChatRole::System
                && message.body().trim_start().starts_with(THINKING_PLACEHOLDER_TAG)
        );
        if !still_placeholder {
            self.thinking_block_idx = None;
            return;
        }
        let Some(text) = self.history.get_mut(idx).and_then(ChatMessage::text_mut) else {
            return;
        };
        if text.len() >= THINKING_REASONING_MAX {
            return;
        }
        if !text.contains('\n') {
            text.push('\n');
        }
        text.push_str(delta);
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
    /// stop the spinner, clear in-flight steering, preserve deferred chat turns,
    /// and post a cancelled note. The canonical Esc/Ctrl-C handler (see `lib.rs`).
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
        // A cancel resolves any parked director gate — the run is over.
        self.director_gate_paused = false;
        self.pending_director_gate = None;
        self.gate_query_in_flight = false;
        self.active_gate_query_epoch = None;
        // The native base session contains an unfinished request but no explicit
        // cancellation boundary. Never resume that session as if it settled;
        // UmaDev's own durable transcript carries the honest boundary into the
        // next fresh session instead.
        self.chat_session_id = None;
        self.chat_resume_identity = None;
        self.host_chat_session_active = false;
        self.run_session_handed_to_chat = false;
        // Deferred chat turns are independent future model turns, not revisions
        // owned by the cancelled writer. Preserve them so cancellation cannot
        // silently eat input; they remain visible in the queued chip and can be
        // recalled/edited before they are sent.
        let deferred_count = self.queued_chat.len();
        // M2 — also drop any pipeline-run steer parked in `queued_steer`. A user
        // cancel ends the run, so a parked steer can never reach a gate; leaving
        // it would keep the "queued N" chip falsely lit after the reset.
        self.queued_steer.clear();
        self.pending_quit_confirm = false;
        // The aborted task has now fully wound down — leave the "stopping…" state.
        self.cancelling = false;
        // A user cancel settles the live task as Stopped (resumable via /tasks).
        self.mark_active_task(TaskStatus::Stopped);
        let cancelled = umadev_i18n::t(self.lang, "run.cancelled").to_string();
        self.push(ChatRole::System, cancelled.clone());
        self.record_turn(
            "assistant",
            format!(
                "{cancelled}\n[control] The preceding in-flight request was cancelled by the user before completion. Do not resume it unless the user explicitly asks."
            ),
        );
        self.persist_chat();
        if deferred_count > 0 {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(
                    self.lang,
                    "chat.queued_preserved",
                    &[&deferred_count.to_string()],
                ),
            );
        }
    }

    /// Enter the **stopping** state the instant Esc/Ctrl-C cancels an in-flight
    /// run/turn: keep the spinner alive and post a "stopping…" line so the UI
    /// reads as in-progress while the aborted task winds down OFF the render path
    /// (the actual drain + reset happens in the event loop's drain branch, which
    /// calls [`Self::cancel_run`] once the task has released its session). This is
    /// the public entry the loop calls while the underlying queue insertion stays private.
    pub fn begin_cancelling(&mut self) {
        // Cancellation acceptance is the linearization point for a gate query.
        // Invalidate it now — not after the async task's bounded drain — so a
        // terminal answer already queued on another channel cannot land during
        // the stopping window.
        self.active_gate_query_epoch = None;
        self.gate_query_in_flight = false;
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
        self.chat_resume_identity = None;
        self.maybe_suggest_design();
        self.push_preflight(requirement);
    }

    /// `Some(action)` if `raw` was a `!`-prefixed local shell command; `None`
    /// means "not a bang command, treat as ordinary input".
    ///
    /// `!cmd` runs `cmd` once in the project root and renders its output as a
    /// `Bash` tool row — the SAME surface a base-issued `Bash` uses, so the
    /// command and its folded output read consistently with the rest of the
    /// transcript. It is deliberately NOT routed to the borrowed brain (Claude
    /// Code's `!` convenience-shell convention), so it never touches the base
    /// session. A bare `!` (or `!` + only whitespace) is a consumed no-op — it
    /// neither runs anything nor leaks the literal `!` to the base. Fully
    /// fail-open: a spawn error / nonzero exit / >10s hang all surface as a
    /// finished row with an explanatory line, never a panic or a frozen UI.
    fn try_bang_command(&mut self, raw: &str) -> Option<Action> {
        let cmd = raw.strip_prefix('!')?.trim();
        if cmd.is_empty() {
            // Bare `!` — do nothing, but still CONSUME it so the literal `!`
            // is not handed to the base as a chat turn.
            return Some(Action::None);
        }
        // A bang command can mutate the same workspace as the base. Keep it on
        // the single-writer boundary instead of running a second shell beside an
        // active/paused task.
        if self.has_interruptible_work() || self.thinking || self.cancelling {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "chat.busy_cancel_first"),
            );
            return Some(Action::None);
        }
        let (ok, output) = run_bang_command(&self.project_root, cmd, self.lang);
        self.push_shell_row(cmd, ok, output);
        Some(Action::None)
    }

    /// Append a finished `Bash` tool row for a one-off `!`-shell run: the command
    /// as the row arg, its (already-bounded) output folded into the result
    /// gutter. An OK run auto-collapses (long output folds to a head-N preview the
    /// global Ctrl+O / latest-row Ctrl+R reveals); a failed run stays expanded so
    /// the error is never hidden — mirroring the base-issued tool-row policy.
    fn push_shell_row(&mut self, cmd: &str, ok: bool, output: String) {
        // A one-off shell row is its own row; never fold it into a low-signal
        // read batch.
        self.stream_tool_batch = None;
        let arg: String = cmd.chars().take(80).collect();
        self.history.push_back(ChatMessage {
            role: ChatRole::Host,
            kind: MessageBody::Tool(ToolCall {
                call_id: None,
                name: "Bash".to_string(),
                arg,
                status: if ok { ToolStatus::Ok } else { ToolStatus::Fail },
                result: (!output.trim().is_empty()).then_some(output),
                progress: None,
                merged: false,
                count: 1,
                collapsed: ok,
            }),
            collapsed: false,
        });
        while self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
    }

    fn native_command_action(&mut self, payload: String) -> Action {
        if self
            .backend
            .as_deref()
            .is_none_or(|backend| !crate::FIRST_CLASS_BACKEND_IDS.contains(&backend))
        {
            self.push(
                ChatRole::System,
                "[warn] 当前没有可接收原生命令的底座 / no active base can receive a native command",
            );
            return Action::None;
        }
        if self.cancelling {
            self.restore_submitted_turn(SubmittedTurn::text(payload));
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "status.stopping"),
            );
            return Action::None;
        }
        if self.thinking || self.has_interruptible_work() || self.gate_query_in_flight {
            self.queue_native_command(payload);
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "chat.queued"));
            self.refresh_status();
            return Action::None;
        }
        self.record_user_turn(&payload);
        self.last_dispatched_chat = None;
        self.thinking = true;
        self.thinking_started = Some(std::time::Instant::now());
        self.last_output_at = None;
        self.tool_in_progress = false;
        self.refresh_status();
        Action::NativeCommand(payload)
    }

    fn slash_base(&mut self, raw: &str) -> Action {
        let suffix = raw.get("/base".len()..).unwrap_or("");
        let payload = suffix.trim_start_matches(char::is_whitespace);
        if !payload.starts_with('/') {
            self.push(ChatRole::System, "usage: /base /<command> [args]");
            return Action::None;
        }
        self.native_command_action(payload.to_string())
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
        // `quit`; `/abort` → `cancel`; `/语言` → `lang`). An unknown verb passes
        // through unchanged to the `_` fallback. The
        // `commands_and_dispatch_are_in_lockstep` test parses the
        // arm literals between the COMMAND-DISPATCH sentinels and locks them
        // against [`COMMANDS`](Self::COMMANDS) so no arm can drift from the
        // registry that the palette + help also read.
        let resolved = Self::resolve_command(&verb);
        let canonical = resolved.map_or(verb.as_str(), |c| c.name);
        // Product commands always win. Only a verb absent from the static
        // registry may use the current base's replacement catalog directly.
        if resolved.is_none() && self.advertised_base_command(&verb) {
            return Some(self.native_command_action(raw.to_string()));
        }
        if self.cancelling && !matches!(canonical, "quit" | "base") {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "status.stopping"),
            );
            self.refresh_status();
            return Some(Action::None);
        }
        // A query at a parked Director gate owns the sole async task slot. Do not
        // let slash commands such as /continue, /revise, /redo, /clear, /run, or
        // a backend switch detach that task and start a writer underneath it. The
        // query can settle, or the user can explicitly cancel/quit it; /help is a
        // harmless local overlay and remains available.
        if self.gate_query_in_flight && !matches!(canonical, "cancel" | "quit" | "help") {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "gate.query.busy"),
            );
            self.refresh_status();
            return Some(Action::None);
        }
        // Commands that write project metadata, snapshot/publish the workspace,
        // or launch an external mutation share the same single-writer boundary
        // as the base. Read-only overlays remain available during a run.
        if (self.has_interruptible_work() || self.thinking)
            && matches!(canonical, "init" | "adopt" | "deploy" | "pr" | "checkpoint")
        {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "chat.busy_cancel_first"),
            );
            self.refresh_status();
            return Some(Action::None);
        }
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
                if self.has_interruptible_work() || self.thinking {
                    self.push(ChatRole::System, umadev_i18n::t(self.lang, "clear.busy"));
                    return Some(Action::None);
                }
                self.history.clear();
                self.conversation.clear();
                // Drop the durable transcript too (a cleared chat starts a fresh
                // persisted file) and invalidate any in-flight compaction so a late
                // summary can never splice into the new conversation.
                self.full_transcript.clear();
                self.conversation_generation = self.conversation_generation.wrapping_add(1);
                self.compaction_in_flight = false;
                // A cleared session starts metering from zero — the persistent
                // token/cost gauge resets with the transcript, and the context
                // gauge + its one-shot nudge re-arm for the fresh conversation.
                self.session_usage.reset();
                self.context_nudge_shown = false;
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
                self.plan_frozen = false;
                self.critic_verdicts.clear();
                self.handoffs.clear();
                self.last_intent_class = None;
                // A cleared transcript also ABANDONS any paused run gate: without this the
                // active_gate/gate_choice stay armed (now invisible), so the NEXT plain
                // message is taken as a gate reply / revise and silently RE-DRIVES the
                // workspace run the user just cleared. Reset the run/gate state to idle.
                self.active_gate = None;
                self.gate_choice = None;
                self.gate_choice_sel = 0;
                self.run_started = false;
                self.run_started_at = None;
                // This is a conversation boundary, not only a transcript wipe.
                // Drop every not-yet-dispatched input and the failed-turn dedup
                // keys so neither steering nor a stale one-shot suppression can
                // affect the first message in the new chat.
                self.clear_transient_routing_state();
                // A cleared transcript means the base should start a fresh
                // session on the next turn, not resume the old one.
                self.host_chat_session_active = false;
                self.chat_session_id = None;
                self.chat_resume_identity = None;
                self.run_session_handed_to_chat = false;
                // The RESIDENT chat session held by the event loop predates the
                // cleared conversation — flag it for close so the next turn opens a
                // fresh one instead of carrying the old dialogue's live process.
                self.chat_session_dirty = true;
                self.reset_base_session_state();
                // Wave 5 / G11: `/clear` starts a FRESH persistent chat — mint a new
                // id so the prior saved chat stays on disk (resumable via /resume)
                // and the next turn persists under the new id.
                self.chat_id = new_chat_session_id();
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "slash.history_cleared"),
                );
                // `/clear` empties the transcript without changing the input-box
                // height, so the loop's generic height-change guard can't catch
                // it: force a full clear+redraw so the dropped rows can't survive
                // as stale overlap on the Windows console (conhost / PowerShell).
                self.request_full_repaint();
                Action::None
            }
            "claude" => self.slash_backend(Some("claude-code")),
            "codex" => self.slash_backend(Some("codex")),
            "opencode" => self.slash_backend(Some("opencode")),
            "grok" => self.slash_backend(Some("grok-build")),
            "kimi" => self.slash_backend(Some("kimi-code")),
            "offline" => self.slash_backend(None),
            "base" => self.slash_base(raw),
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
                match umadev_agent::initialize_project(
                    &self.project_root,
                    &umadev_agent::ProjectInitOptions::new(slug),
                ) {
                    Ok(report) => {
                        self.slug = report.effective_slug();
                        self.push(ChatRole::UmaDev, report.render_summary(self.lang));
                        self.refresh_status();
                        return Some(Action::WorkspaceInitialized);
                    }
                    Err(error) => self.push(
                        ChatRole::System,
                        umadev_i18n::tf(self.lang, "init.failed", &[&error.to_string()]),
                    ),
                }
                Action::None
            }
            "continue" => {
                // Plan may collect clarification, but it must never approve the
                // docs/preview execution boundary or resume a mutating Director.
                // Check before `take()` so the gate remains open and recoverable
                // after the user switches to guarded/auto.
                if self.effective_trust_mode() == umadev_agent::TrustMode::Plan
                    && matches!(
                        self.active_gate,
                        Some(Gate::DocsConfirm | Gate::PreviewConfirm)
                    )
                {
                    self.reject_director_execution_in_plan();
                    Action::None
                } else if let Some(gate) = self.active_gate.take() {
                    self.push(
                        ChatRole::UmaDev,
                        umadev_i18n::tf(
                            self.lang,
                            "slash.gate_approved",
                            &[umadev_i18n::t(self.lang, gate.human_label_key())],
                        ),
                    );
                    self.record_trust_pass(gate.id_str());
                    Action::Continue(gate)
                } else if self.has_interruptible_work() || self.thinking {
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "continue.running"),
                    );
                    Action::None
                } else if !self.run_started
                    && !self.finished
                    && umadev_agent::has_resumable_run(&self.project_root)
                {
                    if self.reject_director_execution_in_plan() {
                        return Some(Action::None);
                    }
                    // Fresh session (no in-memory gate, no in-flight run) but the
                    // previous `/run` left a resumable director-loop run on disk —
                    // RE-ATTACH to the saved plan and drive only the remaining steps
                    // rather than telling the user to restart the whole pipeline. The
                    // requirement is read back from `.umadev/workflow-state.json` when
                    // the in-memory one is empty (a reopened TUI has none).
                    let req = self.resume_run_requirement();
                    // Divider BEFORE the resuming note: the earlier steps stay in
                    // scrollback (the block never cleared the transcript) and the
                    // resumed run appends below this, so the whole run reads as one
                    // continuous history instead of looking like the earlier steps
                    // vanished.
                    self.push_resume_separator();
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
            "thinking" => self.slash_thinking(rest),
            "sandbox" => self.slash_sandbox(rest),
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
            "theme" => self.slash_theme(rest),
            "logs" => self.slash_logs(),
            "questions" => self.slash_questions(rest),
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
                let report = umadev_agent::lessons_report(&self.project_root);
                let body = format_pitfalls_report(self.lang, &report);
                self.overlay = Some(Overlay::from_body(
                    umadev_i18n::t(self.lang, "pitfalls.overlay_title"),
                    &body,
                ));
                Action::None
            }
            "lessons" => self.slash_lessons(),
            "memory" => self.slash_memory(rest),
            "team" => self.slash_team(rest),
            "constitution" => self.slash_constitution(),
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
                if self.has_interruptible_work() || self.thinking {
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
            "tasks" => self.slash_tasks(rest),
            "processes" => self.slash_processes(rest),
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
        // Otherwise Levenshtein ≤ 2 against known registry verbs.
        let typed_lower = typed.to_ascii_lowercase();
        let (mut best, mut best_dist) = (None, usize::MAX);
        let all_verbs = Self::COMMANDS.iter().map(|c| c.name);
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
        // Refuse mid-run / mid-turn (mirrors slash_backend): load_chat swaps in ANOTHER
        // chat's conversation + session while an in-flight build/turn still streams — the
        // finishing turn then overwrites the resumed session's holder (orphaning a base
        // subprocess with no end()) and appends its reply into the WRONG chat's transcript.
        if self.has_interruptible_work() || self.thinking {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "chat.busy_cancel_first"),
            );
            return Action::None;
        }
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

    /// `/compact` — fold the older turns into ONE **structured** summary via the
    /// SAME base-driven path as auto-compaction (intent / files / decisions /
    /// errors / pending / current work), keeping the recent tail verbatim, instead
    /// of the old lossy 160-char digest. The summary `complete()` is async (it
    /// forks the base), so this slash handler only validates + signals intent
    /// ([`Action::Compact`]); the event loop drives the fork and applies the result
    /// ([`App::apply_compaction`]), falling back to FIFO if the base is unreachable.
    fn slash_compact(&mut self) -> Action {
        // Refuse mid-run / mid-turn for consistency with /resume + /backend: a compaction
        // that folds the transcript while a turn is streaming races the generation guard.
        if self.has_interruptible_work() || self.thinking {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "chat.busy_cancel_first"),
            );
            return Action::None;
        }
        if self.compaction_in_flight {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "compact.in_progress").to_string(),
            );
            return Action::None;
        }
        let len = self.conversation.len();
        let tail = COMPACTION_MIN_TAIL.min(len);
        let fold_count = len.saturating_sub(tail);
        if fold_count < umadev_agent::compaction::MIN_FOLD {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "compact.too_short").to_string(),
            );
            return Action::None;
        }
        self.push(
            ChatRole::System,
            umadev_i18n::t(self.lang, "compact.in_progress").to_string(),
        );
        Action::Compact
    }

    fn slash_backend(&mut self, backend: Option<&str>) -> Action {
        // P1-I: refuse to switch the base mid-run. A live run (continuous or
        // single-shot) is driving a base session pinned to the CURRENT backend;
        // swapping `self.backend` + persisting it to config now would (a) leave the
        // in-flight run on the old base while the UI/config claim the new one, and
        // (b) make the NEXT resume/continue open a session on a base the run was
        // never built against — a silent backend mismatch. Reject and tell the user
        // to cancel first; the run, its parked session, and config all stay coherent.
        //
        // A streaming CHAT turn is `agentic_in_flight` but NOT `is_pipeline_active()`
        // (the chat Route never registers a run task), so a pipeline-only guard would
        // let a `/codex` mid-chat-turn commit the new backend + preload a new-base
        // session while the old turn keeps running and unconditionally parks its
        // OLD-base session — racing the preload into either a leaked/dropped session
        // (never `end()`ed) or a holder pinned to the OLD base while config/UI claim
        // the new one. Guard on both, mirroring the `/cancel` arm.
        if self.has_interruptible_work() || self.thinking {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "backend.busy_no_switch"),
            );
            return Action::None;
        }
        let id = backend.unwrap_or("offline").to_string();
        // Snapshot the OLD base label BEFORE committing — the context-handoff block
        // below names both sides of the switch.
        let previous = self.backend_label.clone();
        self.commit_backend(backend.map(str::to_string));
        // The resident chat session is pinned to the OLD base — flag it for close so
        // the next chat turn opens a fresh session on the newly-selected base.
        self.chat_session_dirty = true;
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "backend.switched", &[&id]),
        );
        // CONTEXT HANDOFF (honest-continuity contract): a mid-conversation switch
        // carries the dialogue ONLY via UmaDev's own bounded transcript — the new
        // base's first directive front-loads it (`first_chat_directive`); the OLD
        // base's native deep session context cannot migrate. Record ONE compact
        // handoff block into the conversation itself so (a) the NEW base explicitly
        // sees the preceding dialogue was handed over from another base and (b) the
        // user sees honestly what carries and what doesn't. Only a REAL switch
        // (target differs) with prior dialogue records it; persisted so a relaunch
        // keeps the marker. Appended at the transcript TAIL, so the token-bounded
        // front-load always keeps it. Fail-open: an empty conversation is a no-op.
        if previous != id && !self.conversation.is_empty() {
            let handoff = umadev_i18n::tf(self.lang, "backend.handoff", &[&previous, &id]);
            self.record_turn("system", handoff.clone());
            self.persist_chat();
            self.push(ChatRole::System, handoff);
        }
        self.refresh_status();
        Action::BackendChanged
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
    pub(crate) fn resume_run_requirement(&mut self) -> String {
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

    /// Refuse an explicit Director execution command while the session is in
    /// Plan mode. Returns `true` when the command was consumed. This check lives
    /// on the UI thread so `/run`, `/goal`, and cross-session resume settle before
    /// task registration, run-lock/branch setup, workflow persistence, or host
    /// session creation. Ordinary conversation remains available for read-only
    /// research and planning.
    fn reject_director_execution_in_plan(&mut self) -> bool {
        if self.effective_trust_mode() != umadev_agent::TrustMode::Plan {
            return false;
        }
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::t(self.lang, "continuous.plan_mode_skip").to_string(),
        );
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::t(self.lang, "mode.plan.gate").to_string(),
        );
        true
    }

    fn slash_run(&mut self, arg: &str) -> Action {
        // Single-writer guard: only ONE workspace-mutating run at a time. A second
        // `/run` while one is live is politely rejected (never silently starts a
        // second writer / leaks the running task) and points at the `/tasks`
        // surface to see + stop it. Covers BOTH the legacy pipeline and the
        // director/agentic build via `has_active_run`.
        if self.has_active_run() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "run.already_active"),
            );
            return Action::None;
        }
        if arg.is_empty() {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.usage"));
            return Action::None;
        }
        if self.reject_director_execution_in_plan() {
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
        if self.has_active_run() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "run.already_active"),
            );
            return Action::None;
        }
        let objective = arg.trim();
        if objective.is_empty() {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "goal.usage"));
            return Action::None;
        }
        if self.reject_director_execution_in_plan() {
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

    fn open_memory_overlay(&mut self, title_key: &str, body: &str) {
        self.overlay = Some(Overlay::from_body(
            umadev_i18n::t(self.lang, title_key),
            body,
        ));
    }

    fn push_memory_failure(&mut self, error: &dyn std::fmt::Display) {
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "memory.operation_failed", &[&error.to_string()]),
        );
    }

    fn slash_memory(&mut self, rest: &str) -> Action {
        let command = match parse_memory_command(rest) {
            Ok(command) => command,
            Err(error) => {
                let body = format!(
                    "{}\n\n{}",
                    format_memory_parse_error(self.lang, &error),
                    umadev_i18n::t(self.lang, "memory.usage")
                );
                self.open_memory_overlay("memory.usage_title", &body);
                return Action::None;
            }
        };
        if command.mutates() && (self.has_interruptible_work() || self.thinking) {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "memory.busy"));
            return Action::None;
        }
        self.execute_memory_command(command);
        Action::None
    }

    fn execute_memory_command(&mut self, command: MemoryTuiCommand) {
        use umadev_agent::memory_control::{self, MemoryScope, MemoryStore};

        match command {
            MemoryTuiCommand::Inventory { scope } => {
                let mut sections = Vec::new();
                for selected in scope.scopes() {
                    sections.push(format_memory_inventory(
                        self.lang,
                        &memory_control::inventory(&self.project_root, *selected),
                        *selected,
                        false,
                        None,
                    ));
                }
                self.open_memory_overlay("memory.overlay_title", &sections.join("\n\n"));
            }
            MemoryTuiCommand::RetentionView { scope, store } => {
                let mut sections = Vec::new();
                for selected in scope.scopes() {
                    sections.push(format_memory_inventory(
                        self.lang,
                        &memory_control::inventory(&self.project_root, *selected),
                        *selected,
                        true,
                        store,
                    ));
                }
                self.open_memory_overlay("memory.overlay_title", &sections.join("\n\n"));
            }
            MemoryTuiCommand::Capture {
                scope,
                selector,
                enabled,
            } => {
                let selected = selector.map(|selector| selector.capture_stores(scope));
                let result = selected.as_ref().map_or_else(
                    || memory_control::update_capture(&self.project_root, scope, None, enabled),
                    |stores| {
                        memory_control::update_capture_stores(
                            &self.project_root,
                            scope,
                            stores,
                            enabled,
                        )
                    },
                );
                match result {
                    Ok(()) => {
                        let stores = selected.as_ref().map_or_else(
                            || {
                                umadev_i18n::t(self.lang, "memory.stores.all_configurable")
                                    .to_string()
                            },
                            |stores| memory_store_summary(self.lang, stores),
                        );
                        self.push(
                            ChatRole::System,
                            umadev_i18n::tf(
                                self.lang,
                                "memory.capture_ok",
                                &[
                                    memory_state_label(self.lang, Some(enabled)),
                                    scope.id(),
                                    &stores,
                                ],
                            ),
                        );
                    }
                    Err(error) => self.push_memory_failure(&error),
                }
            }
            MemoryTuiCommand::Recall {
                scope,
                selector,
                enabled,
            } => {
                let selected = selector.map(|selector| selector.recall_stores(scope));
                let affects_input_history = scope == MemoryScope::Project
                    && selected
                        .as_ref()
                        .is_none_or(|stores| stores.contains(&MemoryStore::InputHistory));
                let result = selected.as_ref().map_or_else(
                    || memory_control::update_recall(&self.project_root, scope, None, enabled),
                    |stores| {
                        memory_control::update_recall_stores(
                            &self.project_root,
                            scope,
                            stores,
                            enabled,
                        )
                    },
                );
                match result {
                    Ok(()) => {
                        if affects_input_history {
                            self.input_history.clear();
                            self.input_history_idx = None;
                            self.input_history_draft = None;
                            self.load_history();
                        }
                        let stores = selected.as_ref().map_or_else(
                            || {
                                umadev_i18n::t(self.lang, "memory.stores.all_configurable")
                                    .to_string()
                            },
                            |stores| memory_store_summary(self.lang, stores),
                        );
                        self.push(
                            ChatRole::System,
                            umadev_i18n::tf(
                                self.lang,
                                "memory.recall_ok",
                                &[
                                    memory_state_label(self.lang, Some(enabled)),
                                    scope.id(),
                                    &stores,
                                ],
                            ),
                        );
                    }
                    Err(error) => self.push_memory_failure(&error),
                }
            }
            MemoryTuiCommand::RetentionSet { scope, store, days } => {
                match memory_control::update_retention(&self.project_root, scope, store, Some(days))
                {
                    Ok(()) => self.push(
                        ChatRole::System,
                        umadev_i18n::tf(
                            self.lang,
                            "memory.retention_set_ok",
                            &[scope.id(), store.id(), &days.to_string()],
                        ),
                    ),
                    Err(error) => self.push_memory_failure(&error),
                }
            }
            MemoryTuiCommand::RetentionClear { scope, store } => {
                match memory_control::update_retention(&self.project_root, scope, store, None) {
                    Ok(()) => self.push(
                        ChatRole::System,
                        umadev_i18n::tf(
                            self.lang,
                            "memory.retention_clear_ok",
                            &[scope.id(), store.id()],
                        ),
                    ),
                    Err(error) => self.push_memory_failure(&error),
                }
            }
            MemoryTuiCommand::RetentionRun {
                scope,
                store,
                confirmed,
            } => {
                if !confirmed {
                    let body = umadev_i18n::tf(
                        self.lang,
                        "memory.retention_preview",
                        &[scope.id(), store.id()],
                    );
                    self.open_memory_overlay("memory.confirm_title", &body);
                    return;
                }
                match memory_control::enforce_retention(&self.project_root, scope, store) {
                    Ok(report) => {
                        let days = report.retention_days.map_or_else(
                            || umadev_i18n::t(self.lang, "common.none").to_string(),
                            |days| days.to_string(),
                        );
                        self.push(
                            ChatRole::System,
                            umadev_i18n::tf(
                                self.lang,
                                "memory.retention_run_ok",
                                &[
                                    scope.id(),
                                    report.store.id(),
                                    &days,
                                    &report.scanned_files.to_string(),
                                    &report.forgotten_files.to_string(),
                                    &report.bytes.to_string(),
                                    report.tombstone_id.as_deref().unwrap_or("none"),
                                ],
                            ),
                        );
                    }
                    Err(error) => self.push_memory_failure(&error),
                }
            }
            MemoryTuiCommand::Export {
                scope,
                selector,
                destination,
                confirmed,
            } => {
                let stores = selector.stores(scope);
                if stores.is_empty() {
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "memory.no_stores"),
                    );
                    return;
                }
                let store_summary = memory_store_summary(self.lang, &stores);
                if !confirmed {
                    let body = umadev_i18n::tf(
                        self.lang,
                        "memory.export_preview",
                        &[
                            scope.id(),
                            &store_summary,
                            &destination.display().to_string(),
                        ],
                    );
                    self.open_memory_overlay("memory.confirm_title", &body);
                    return;
                }
                match memory_control::export(&self.project_root, scope, &stores, &destination, true)
                {
                    Ok(report) => self.push(
                        ChatRole::System,
                        umadev_i18n::tf(
                            self.lang,
                            "memory.export_ok",
                            &[
                                scope.id(),
                                &report.files.to_string(),
                                &report.bytes.to_string(),
                                &report.destination.display().to_string(),
                            ],
                        ),
                    ),
                    Err(error) => self.push_memory_failure(&error),
                }
            }
            MemoryTuiCommand::Forget {
                scope,
                selector,
                confirmed,
            } => {
                let stores = selector.forget_stores(scope);
                if stores.is_empty() {
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "memory.no_stores"),
                    );
                    return;
                }
                let store_summary = memory_store_summary(self.lang, &stores);
                if !confirmed {
                    let body = umadev_i18n::tf(
                        self.lang,
                        "memory.forget_preview",
                        &[scope.id(), &store_summary],
                    );
                    self.open_memory_overlay("memory.confirm_title", &body);
                    return;
                }
                match memory_control::forget(&self.project_root, scope, &stores, true) {
                    Ok(report) => self.push(
                        ChatRole::System,
                        umadev_i18n::tf(
                            self.lang,
                            "memory.forget_ok",
                            &[
                                scope.id(),
                                &report.files.to_string(),
                                &report.bytes.to_string(),
                                report.tombstone_id.as_deref().unwrap_or("none"),
                            ],
                        ),
                    ),
                    Err(error) => self.push_memory_failure(&error),
                }
            }
            MemoryTuiCommand::ClearCache { store, confirmed } => {
                if !confirmed {
                    let body =
                        umadev_i18n::tf(self.lang, "memory.clear_cache_preview", &[store.id()]);
                    self.open_memory_overlay("memory.confirm_title", &body);
                    return;
                }
                match memory_control::clear_derived_cache(&self.project_root, store) {
                    Ok((files, bytes)) => self.push(
                        ChatRole::System,
                        umadev_i18n::tf(
                            self.lang,
                            "memory.clear_cache_ok",
                            &[store.id(), &files.to_string(), &bytes.to_string()],
                        ),
                    ),
                    Err(error) => self.push_memory_failure(&error),
                }
            }
        }
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
        if self.has_active_run() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "run.already_active"),
            );
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
                let mark = plan_step_glyph(step.status.as_str());
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
            // (2a) Convened roster — ONLY the seats with a real plan step, each
            // with its live status + (if reviewed) its verdict chip. Anti-theater:
            // `convened_roster` never invents a seat without a step.
            let roster = self.convened_roster();
            if roster.is_empty() {
                // No seat-attributed plan steps yet — fall back to the raw verdict
                // list so a review-only run still shows who weighed in (the old
                // wording), never a blank run section.
                if has_review {
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
                        body.push_str(&format!(
                            "  {} · {verdict}\n",
                            seat_display_name(self.lang, &c.seat)
                        ));
                    }
                }
            } else {
                for seat in &roster {
                    let status = umadev_i18n::t(self.lang, seat.status.label_key());
                    let chip = seat
                        .verdict
                        .map(|(accepts, n)| {
                            let v = if accepts {
                                umadev_i18n::t(self.lang, "plan.review.accept").to_string()
                            } else {
                                umadev_i18n::tf(
                                    self.lang,
                                    "plan.review.block",
                                    &[&n.max(1).to_string()],
                                )
                            };
                            format!(" · {v}")
                        })
                        .unwrap_or_default();
                    body.push_str(&format!(
                        "  {} · {status}{chip}\n",
                        seat_display_name(self.lang, &seat.role)
                    ));
                }
            }
            // (2b) Handoff timeline — the real DONE transitions, in order.
            if !self.handoffs.is_empty() {
                body.push_str(umadev_i18n::t(self.lang, "team.handoff.header"));
                body.push('\n');
                for h in &self.handoffs {
                    body.push_str("  ");
                    body.push_str(&umadev_i18n::tf(
                        self.lang,
                        "team.handoff.entry",
                        &[&seat_display_name(self.lang, &h.seat), &h.title],
                    ));
                    body.push('\n');
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

    /// `/constitution` (alias `/charter`) — surface the team's **constitution**:
    /// the visible, user-editable charter of the team's non-negotiable operating
    /// principles (Wave C of the development-team repositioning). The firmware
    /// already injects these non-negotiables into every turn; this makes them a
    /// thing the user can READ and EDIT.
    ///
    /// On first use it generates `.umadev/constitution.md` from the rules the team
    /// actually enforces + the craft principles the firmware injects, then shows
    /// it in a scrollable overlay and notes the path so the user can edit it. An
    /// existing (already user-edited) file is shown verbatim and NEVER clobbered.
    /// Read-only + fail-open: a write failure still shows the in-memory default.
    fn slash_constitution(&mut self) -> Action {
        let doc = umadev_agent::ensure_constitution(&self.project_root);
        self.overlay = Some(Overlay::from_body(
            umadev_i18n::t(self.lang, "constitution.overlay_title"),
            &doc.markdown,
        ));
        let path = doc.path.display().to_string();
        // First-time generation gets a one-line "generated from the rules" note;
        // every open notes where to edit it (and that edits are never overwritten).
        if doc.generated {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "constitution.generated", &[&path]),
            );
        }
        self.push(
            ChatRole::System,
            umadev_i18n::tf(self.lang, "constitution.edit_hint", &[&path]),
        );
        Action::None
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
                format!(
                    "Plan steering: SKIP step `{target}` — do not perform it; proceed with the rest of the plan."
                ),
                umadev_i18n::tf(self.lang, "plan.steer.skip", &[target]),
            ),
            "veto" => (
                format!(
                    "Plan steering: VETO step `{target}` — remove it from the plan entirely and do not perform it."
                ),
                umadev_i18n::tf(self.lang, "plan.steer.veto", &[target]),
            ),
            "up" => (
                format!(
                    "Plan steering: REORDER step `{target}` EARLIER — do it before its current predecessors where dependencies allow."
                ),
                umadev_i18n::tf(self.lang, "plan.steer.move", &[target, "↑"]),
            ),
            "down" => (
                format!(
                    "Plan steering: REORDER step `{target}` LATER — defer it after its current successors where dependencies allow."
                ),
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
        if self.has_interruptible_work() || self.thinking {
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
        body.push_str("model:           (the base's own — UmaDev never sets one)\n");
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
        body.push_str("  /claude /codex /opencode /grok /kimi\n");
        body.push_str("                                  switch base CLI (or /offline)\n");
        body.push_str(
            "  (model)                       set it in the base — UmaDev never overrides it\n",
        );
        body.push_str("  /manual  /auto                review mode: pause vs autonomous\n");
        body.push_str("  /design <name>                switch design system\n");
        body.push_str("  /template <name>              switch seed template\n");
        body.push_str("  /run <slug> <req>             set slug + requirement\n");
        body.push_str("  edit .umadevrc               project-level overrides\n");
        self.overlay = Some(Overlay::from_body(" config — Esc close ", &body));
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
        // Read-only observation: UmaDev never sets a model. Prefer the model the
        // live base reported for this session; fall back to the base's static config
        // only when no runtime report exists.
        let display_model = self
            .base_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        let detected_model = display_model
            .map(|m| {
                let source = if self.base_model_live {
                    "reported by the base"
                } else {
                    "configured by the base"
                };
                (m, source)
            })
            .or_else(|| {
                self.backend
                    .as_deref()
                    .filter(|b| !b.is_empty() && *b != "offline")
                    .and_then(|b| crate::detect_base_model(b, &self.project_root))
                    .map(|m| (m, "configured by the base"))
            });
        match detected_model {
            Some((m, source)) => body.push_str(&format!("model        {m} ({source})\n")),
            None => body.push_str(&format!(
                "model        {} login default (the base's own)\n",
                self.backend_label
            )),
        }
        if let Some(enabled) = self.base_session_thinking {
            let locked = if enabled {
                !self.base_session_thinking_can_disable
            } else {
                !self.base_session_thinking_can_enable
            };
            body.push_str(&format!(
                "thinking     {}{} (reported by the base)\n",
                if enabled { "on" } else { "off" },
                if locked { " · model-locked" } else { "" }
            ));
        } else if let Some(b) = self.backend.as_deref() {
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
        self.reset_base_session_state();
        self.config.backend = Some(self.backend_label.clone());
        // A different base means a different session — don't resume the old
        // base's conversation into the new one.
        self.host_chat_session_active = false;
        self.chat_session_id = None;
        self.chat_resume_identity = None;
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
            match self.base_model.clone() {
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

    fn preview_url_from_notes_for_product(&self) -> Option<String> {
        if self.notes_preview_is_acceptance_harness() {
            None
        } else {
            self.preview_url_from_notes()
        }
    }

    fn run_command_from_notes_for_product(&self) -> Option<String> {
        if self.notes_preview_is_acceptance_harness() {
            None
        } else {
            self.run_command_from_notes()
        }
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
        let command = match (&detected, self.run_command_from_notes_for_product()) {
            // Self-detection wins — we control the command + know the URL.
            (Some(ds), _) => Some(ds.command.clone()),
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
        if let Some(u) = self.preview_url_from_notes_for_product() {
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

        // Per-step task breakdown — the FINAL status of every plan step, with the
        // SAME glyphs + `done/total` count the live `/plan` panel used. This is the
        // card's whole reason for existing after a build ends: the live plan panel
        // is cleared on finish (`finalize_live_panels`), so without this the user
        // loses all visibility into WHICH tasks landed DONE, which are BLOCKED, and
        // which stayed INCOMPLETE — the exact loss the user reported, worst when a
        // build ends withheld (delivery blocked). Read here, in `build_completion_card`
        // (called by `post_build_completion_card` BEFORE it clears the rows), so the
        // statuses are still live. Omitted for a plan-less chat/Fast build so there's
        // never an empty "tasks" block.
        if !self.plan_steps.is_empty() {
            let done = self
                .plan_steps
                .iter()
                .filter(|s| s.status == "done")
                .count();
            let total = self.plan_steps.len();
            let mut section =
                umadev_i18n::tf(lang, "build.complete.tasks", &[&format!("{done}/{total}")]);
            // When the build did NOT finish every step, lead with an unmissable
            // one-liner naming how many blocked vs. still-unfinished — the user's
            // core ask ("show me which are BLOCKED, which INCOMPLETE") surfaced up
            // front, not buried in the list.
            if done < total {
                let blocked = self
                    .plan_steps
                    .iter()
                    .filter(|s| s.status == "blocked")
                    .count();
                let unfinished = total - done - blocked;
                section.push('\n');
                section.push_str(&umadev_i18n::tf(
                    lang,
                    "build.complete.tasks_incomplete",
                    &[&blocked.to_string(), &unfinished.to_string()],
                ));
            }
            for step in &self.plan_steps {
                section.push_str(&format!(
                    "\n  {} {} · {}",
                    plan_step_glyph(step.status.as_str()),
                    step.id,
                    step.title
                ));
            }
            lines.push(section);
        }

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
        let run_cmd = self.run_command_from_notes_for_product().or_else(|| {
            umadev_agent::verify::detect_dev_server(&self.project_root).map(|ds| ds.command.clone())
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
            .preview_url_from_notes_for_product()
            .unwrap_or_else(|| ds.default_url.to_string());
        Some((url, ds.command.clone()))
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
        // Kill the whole process GROUP, not just the wrapper: `npm/pnpm run dev`
        // forks the real node/vite server as a grandchild that a bare start_kill
        // leaves holding the port (the "reported stopped but still running" bug).
        // The preview child was spawned detached (its own session leader), so a
        // group kill reaches the grandchild too.
        let killed = self.preview_server.lock().is_ok_and(|mut g| {
            g.take().is_some_and(|mut c| {
                let _ = umadev_agent::kill_process_group(&c);
                let _ = c.start_kill();
                true
            })
        });
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
        // The user explicitly approved running this command — remember the
        // approval for this project's trust ledger so the same reversible action
        // class isn't re-asked. Fail-open + floor-safe: an irreversible (network)
        // class records nothing, and the deploy preflight floor above uses an
        // always-escalating `git push` probe regardless, so a deploy itself is
        // never skipped by this ledger entry.
        if confirmed {
            self.record_action_approval(&cmd, "");
        }
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(self.lang, "deploy.starting", &[&cmd]),
        );
        Action::RunDeploy { command: cmd }
    }

    /// Toggle animations, or cycle the trust tier with Shift+Tab:
    /// Plan (read-only) → Guarded (approval-gated) → Auto → Plan.
    /// `/manual` and `/auto` select the corresponding execution tier directly.
    pub fn cycle_approval_mode(&mut self) {
        use umadev_agent::TrustMode;
        let next = match self.effective_trust_mode() {
            TrustMode::Plan => TrustMode::Guarded,
            TrustMode::Guarded => TrustMode::Auto,
            TrustMode::Auto => TrustMode::Plan,
        };
        // A live writer can lose authority only at a cancel/rebuild boundary.
        if next == TrustMode::Plan && (self.has_interruptible_work() || self.thinking) {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "chat.busy_cancel_first"),
            );
            return;
        }
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
        if self.effective_trust_mode().is_downgrade_to(mode)
            && (self.has_interruptible_work() || self.thinking)
        {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "chat.busy_cancel_first"),
            );
            return Action::None;
        }
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
                if self.effective_trust_mode().is_downgrade_to(mode)
                    && (self.has_interruptible_work() || self.thinking)
                {
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "chat.busy_cancel_first"),
                    );
                    return Action::None;
                }
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

    /// `/thinking [on|off]` — inspect or change Kimi Code's native,
    /// model-owned thinking toggle. The change is sent as typed ACP session
    /// configuration; it is never forwarded as chat text.
    fn slash_thinking(&mut self, rest: &str) -> Action {
        if self.backend.as_deref() != Some("kimi-code") {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "thinking.kimi_only"),
            );
            return Action::None;
        }
        let arg = rest.trim().to_ascii_lowercase();
        if arg.is_empty() {
            let state = self.base_session_thinking.map_or_else(
                || umadev_i18n::t(self.lang, "thinking.unavailable").to_string(),
                |enabled| {
                    let locked = if enabled {
                        !self.base_session_thinking_can_disable
                    } else {
                        !self.base_session_thinking_can_enable
                    };
                    umadev_i18n::tf(
                        self.lang,
                        if locked {
                            "thinking.current_locked"
                        } else {
                            "thinking.current"
                        },
                        &[if enabled { "on" } else { "off" }],
                    )
                },
            );
            self.push(
                ChatRole::System,
                format!("{state}\n{}", umadev_i18n::t(self.lang, "thinking.usage")),
            );
            return Action::None;
        }
        let enabled = match arg.as_str() {
            "on" | "enable" | "enabled" | "开" | "開" => true,
            "off" | "disable" | "disabled" | "关" | "關" => false,
            _ => {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "thinking.usage"),
                );
                return Action::None;
            }
        };
        if self.has_interruptible_work() || self.thinking {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "thinking.busy"));
            return Action::None;
        }
        if let Some(current) = self.base_session_thinking {
            if current == enabled {
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(
                        self.lang,
                        "thinking.already",
                        &[if enabled { "on" } else { "off" }],
                    ),
                );
                return Action::None;
            }
            let selectable = if enabled {
                self.base_session_thinking_can_enable
            } else {
                self.base_session_thinking_can_disable
            };
            if !selectable {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "thinking.locked"),
                );
                return Action::None;
            }
        }
        self.transient_status = Some(umadev_i18n::tf(
            self.lang,
            "thinking.changing",
            &[if enabled { "on" } else { "off" }],
        ));
        Action::SetThinking(enabled)
    }

    /// `/sandbox [read-only|workspace-write|danger-full-access]` — view or change
    /// the **Codex base** launch sandbox without hand-editing `.umadevrc` (or
    /// hacking `UMADEV_CODEX_SANDBOX` into a shell rc).
    ///
    /// No arg → show the CURRENT tier + the three options with a one-line WHY for
    /// each. In particular it answers the "why does network need it?" question:
    /// `workspace-write` sandboxes the base so the NETWORK and local
    /// dev ports are blocked and `git` won't run — which is why `npm start`, a dev
    /// server, package installs and `git commit` all FAIL under it;
    /// `danger-full-access` removes the sandbox so full-stack work runs. If the
    /// active base isn't codex, a note says the setting only applies to codex.
    ///
    /// A valid arg sets it for THIS session (publishes into the codex driver's
    /// thread-safe shared sandbox override — the SAME mechanism as startup) AND
    /// persists it to `.umadevrc` so it survives a restart. The high-risk tier
    /// reuses the SAME loud red liability warning shown at startup. An
    /// unrecognised arg just prints usage (never crashes). Fail-open throughout:
    /// a failed `.umadevrc` write still applies for the session + warns the user.
    fn slash_sandbox(&mut self, rest: &str) -> Action {
        use umadev_agent::config::CodexSandbox;
        let arg = rest.trim();
        let is_codex = self.backend.as_deref() == Some("codex");

        // ── No arg: explain the current tier + the three options + WHY each. ──
        if arg.is_empty() {
            let current = effective_codex_sandbox(&self.project_root);
            let mut body = umadev_i18n::tf(self.lang, "sandbox.current", &[current.as_codex_arg()]);
            for key in [
                "sandbox.why.read_only",
                "sandbox.why.workspace_write",
                "sandbox.why.danger",
            ] {
                body.push('\n');
                body.push_str(umadev_i18n::t(self.lang, key));
            }
            if !is_codex {
                body.push('\n');
                body.push_str(umadev_i18n::t(self.lang, "sandbox.codex_only"));
            }
            body.push('\n');
            body.push_str(umadev_i18n::t(self.lang, "sandbox.usage"));
            self.push(ChatRole::System, body);
            return Action::None;
        }

        // ── An arg: accept only a recognised tier; garbage → usage (no crash). ──
        // `CodexSandbox::parse_fail_open` resolves the VALUE leniently, but we gate
        // on an explicit recognised-token set first so a typo shows usage instead
        // of silently resolving to `workspace-write`.
        let normalized = arg.to_ascii_lowercase().replace('_', "-");
        let recognized = matches!(
            normalized.as_str(),
            "read-only"
                | "readonly"
                | "workspace-write"
                | "danger-full-access"
                | "danger-full"
                | "full-access"
        );
        if !recognized {
            self.push(ChatRole::System, umadev_i18n::t(self.lang, "sandbox.usage"));
            return Action::None;
        }
        // A live turn owns the resident session. Replacing its sandbox in the
        // middle of a tool call would race the old process and make the visible
        // setting disagree with the work still running. Ask for an explicit
        // cancel first; an idle/parked session is rebuilt below.
        if is_codex && (self.has_interruptible_work() || self.thinking) {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "sandbox.busy_no_change").to_string(),
            );
            return Action::None;
        }
        let mode = CodexSandbox::parse_fail_open(arg);

        // Apply for THIS session: publish to the codex driver's thread-safe shared
        // override, so the next codex turn uses it — the SAME mechanism as startup.
        // NOT a process-env `set_var`: the driver reads this from a background task
        // while a turn streams, so a runtime setenv racing its getenv is UB.
        umadev_host::codex_session::set_codex_sandbox(Some(mode.as_codex_arg()));

        // Persist to `.umadevrc` so it survives a restart. Fail-open: a write
        // error still leaves the session env set; we just warn it didn't save.
        let persisted = umadev_agent::config::persist_codex_sandbox(&self.project_root, mode);

        // The high-risk tier reuses the SAME loud red startup liability warning.
        if mode.is_high_risk() {
            self.push(
                ChatRole::Error,
                umadev_i18n::t(self.lang, "codex.sandbox.danger_warning").to_string(),
            );
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::t(self.lang, "sandbox.danger_set").to_string(),
            );
        } else {
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(self.lang, "sandbox.set", &[mode.as_codex_arg()]),
            );
        }
        // Remind that the change only bites once codex is the active base.
        if !is_codex {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "sandbox.codex_only").to_string(),
            );
        }
        if let Err(e) = persisted {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "sandbox.persist_failed", &[&e.to_string()]),
            );
        }
        if is_codex {
            Action::SandboxChanged
        } else {
            Action::None
        }
    }

    /// Apply a trust tier as the session override, keeping the legacy binary
    /// `auto_approve_override` consistent so the prompt chip + any old code path
    /// reads the same state.
    fn set_trust_mode(&mut self, mode: umadev_agent::TrustMode) {
        let changed = self.effective_trust_mode() != mode;
        self.trust_mode_override = Some(mode);
        self.auto_approve_override = Some(mode.gates_auto_approve());
        // Keep a future config-derived fallback honest if this override is cleared.
        self.invalidate_trust_cache();
        if changed {
            // Native sessions retain launch permissions and a persisted vendor id
            // is authority-bound to that exact profile. Rebuild at the boundary
            // and deliberately hand the conversation over through UmaDev's durable
            // transcript; never load the old id under the new trust tier.
            self.chat_session_dirty = true;
            self.chat_session_id = None;
            self.chat_resume_identity = None;
            self.host_chat_session_active = false;
            self.run_session_handed_to_chat = false;
            self.reset_base_session_state();
            self.persist_chat();
        }
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

    /// Record that the user **approved** a guarded confirmation for `command` /
    /// `target`'s reversible action class, scoped to THIS project, so the same
    /// class isn't re-asked. Persists to the per-project trust ledger
    /// (`.umadev/trust.json`) via the ready
    /// [`umadev_agent::trust::remember_project_approval`] API and mirrors the new
    /// rule into the in-memory ledger so [`TrustLedger::remembers`] is true for
    /// the rest of this session too (without a disk re-read).
    ///
    /// Fully **fail-open** and floor-safe: an irreversible-floor action (`.git`
    /// internals / network / destructive verb) records nothing and returns
    /// `false`, so the safety floor always re-confirms; any IO error is swallowed
    /// so trust learning never blocks a run. Returns `true` when a NEW rule was
    /// recorded, and surfaces a one-time localized note so the user knows the
    /// approval was remembered.
    fn record_action_approval(&mut self, command: &str, target: &str) -> bool {
        let recorded =
            umadev_agent::trust::remember_project_approval(&self.project_root, command, target);
        // Keep the in-memory ledger in lockstep with disk (the disk helper does a
        // fresh load+save; this avoids re-reading just to stay consistent).
        self.trust_ledger.remember_approval(command, target);
        if recorded {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "trust.approval_remembered").to_string(),
            );
        }
        recorded
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

    /// `/theme light|dark|auto` — the blind-recovery control for a terminal
    /// whose background UmaDev guessed wrong (most often Apple Terminal.app on a
    /// non-stock profile, where near-white text lands on a white background and
    /// the whole UI looks blank). `light`/`dark` force the palette for the rest
    /// of the session and pin it so a late background-probe reply cannot flip it
    /// back; `auto` releases the pin and re-runs detection. A user staring at
    /// invisible text can type `/theme light` sight-unseen and recover at once.
    fn slash_theme(&mut self, rest: &str) -> Action {
        match rest.trim().to_ascii_lowercase().as_str() {
            "light" => {
                crate::ui::set_light_theme(true);
                crate::ui::set_theme_locked(true);
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "slash.theme_light"),
                );
                Action::ForceRedraw
            }
            "dark" => {
                crate::ui::set_light_theme(false);
                crate::ui::set_theme_locked(true);
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "slash.theme_dark"),
                );
                Action::ForceRedraw
            }
            // No argument defaults to auto — the least surprising "reset" verb.
            "auto" | "" => {
                // Release the manual pin, apply the best synchronous guess now
                // (env + Apple Terminal.app heuristic), then ask the event loop
                // to re-fire the OSC 11 probe so an OSC-capable terminal refines
                // it. Fail-open: a terminal that never answers keeps the guess.
                crate::ui::set_theme_locked(false);
                crate::ui::set_light_theme(crate::detect_light_bg());
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "slash.theme_auto"),
                );
                Action::ReprobeTheme
            }
            other => {
                self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "slash.theme_unknown", &[other]),
                );
                Action::None
            }
        }
    }

    /// `/logs` — toggle **process-log visibility** for the base's long-running
    /// commands. A multi-minute Maven/Gradle/`spring-boot:run` build is captured by
    /// the base's own sandbox and only handed back as a tight 200-char clip on
    /// completion, so during the build the user sees a silent "thinking." ON makes
    /// the base drivers surface the FULL command output — and, for codex, stream it
    /// as it runs — so the live build log reaches the transcript and the user can
    /// see it progressing. Flips the env the host drivers read (live, picked up on
    /// the next turn/session) and persists the preference so it survives a restart.
    fn slash_logs(&mut self) -> Action {
        let now_on = !self.show_process_logs;
        self.show_process_logs = now_on;
        self.config.show_process_logs = now_on;
        // Publish to the env the (out-of-process) base drivers read on their next
        // turn/session — the TUI renderer reads `self.show_process_logs` directly.
        UserConfig::publish_process_logs(now_on);
        // Persist so the choice survives a restart (fail-open: a write error still
        // leaves the session env set; we just note it didn't save).
        let _ = crate::config::save_to(&self.config, &self.config_path);
        let key = if now_on {
            "slash.logs_on"
        } else {
            "slash.logs_off"
        };
        self.push(ChatRole::System, umadev_i18n::t(self.lang, key));
        Action::None
    }

    /// `/questions [text|picker]` — choose how approval questions (UmaDev's own
    /// gate checkpoints AND the base's `AskUserQuestion`) are presented: `text`
    /// frames the question + options as prose the user answers in natural language;
    /// `picker` (the default) renders the numbered multiple-choice picker. With no
    /// argument it TOGGLES between the two. The free-text reply path already works
    /// either way — only the presentation changes. Persists so it survives a
    /// restart, and publishes the agent-side flag live (picked up on the next turn).
    fn slash_questions(&mut self, rest: &str) -> Action {
        let arg = rest.trim().to_ascii_lowercase();
        let to_text = match arg.as_str() {
            "text" | "prose" | "free" => true,
            "picker" | "menu" | "choice" => false,
            // No / unknown argument → toggle the current preference.
            _ => !self.config.prefers_text_questions(),
        };
        self.config.question_form = Some(if to_text { "text" } else { "picker" }.to_string());
        // Publish the agent-side shared flag live (base AskUserQuestion notes) and
        // persist so the choice survives a restart (fail-open on a write error).
        self.config.apply_question_form();
        let _ = crate::config::save_to(&self.config, &self.config_path);
        let key = if to_text {
            "slash.questions_text"
        } else {
            "slash.questions_picker"
        };
        self.push(ChatRole::System, umadev_i18n::t(self.lang, key));
        Action::None
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
        let body = crate::usage_view::format_usage_report(
            self.lang,
            &umadev_agent::runner::usage_report(),
        );
        self.push(ChatRole::System, body);
        Action::None
    }

    /// `/lessons` — show reusable rules distilled from concrete incidents and
    /// mechanically verified outcomes. The incident ledger itself remains in
    /// `/pitfalls`; this view never duplicates it. Pure read of
    /// `.umadev/learned/` (mirrors the `umadev lessons` CLI verb).
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
        if let Some(url) = self.effective_preview_url() {
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

    // ---- Feature A: completion notification (terminal bell) --------------
    //
    // A run / long agentic turn reaching a terminal state — finished, aborted,
    // or paused at a gate needing the user — arms a bell IFF (a) it's enabled and
    // (b) the work ran long enough (≥ `BELL_MIN_ELAPSED`) that the user may have
    // stepped away, so a quick chat reply never beeps. The actual BEL byte is
    // written by the event loop between frames (single-writer R3 discipline); we
    // only set a flag here. Every path is fail-open.

    /// Arm the completion bell if it's enabled and `since` (the turn/run's start
    /// instant, captured BEFORE the settle clears it) is at least
    /// the internal minimum elapsed interval in the past. A `None` start or a too-short turn arms
    /// nothing — no beep on a quick turn.
    /// Mirror the shared in-flight approval pause into the app model (called by
    /// the event loop each iteration with the holder's current `(action, target)`
    /// snapshot). Returns `true` when the state CHANGED so the caller schedules a
    /// redraw of the sticky approval bar (A2#5). The appearance edge (`None` →
    /// `Some`) also arms the completion bell — a pause popping up deep into a
    /// long turn is exactly the "stepped away, run silently waiting" case the
    /// bell exists for. Fail-open: a same-value call is a no-op.
    pub fn set_pending_approval(&mut self, item: Option<(String, String)>) -> bool {
        if self.pending_approval == item {
            return false;
        }
        if item.is_some() && self.pending_approval.is_none() {
            self.arm_completion_bell(self.thinking_started);
        }
        self.pending_approval = item;
        true
    }

    pub(crate) fn arm_completion_bell(&mut self, since: Option<std::time::Instant>) {
        if !self.bell_enabled {
            return;
        }
        if since.is_some_and(|t| t.elapsed() >= BELL_MIN_ELAPSED) {
            self.bell_pending = true;
            self.bell_count = self.bell_count.saturating_add(1);
        }
    }

    /// Drain the pending-bell flag. The event loop calls this each iteration and,
    /// when `true`, writes the BEL byte through the render's own backend writer
    /// between frames. Returns `false` once drained (idempotent). A drained
    /// `true` also marks the terminal contaminated (P3): the BEL the loop is
    /// about to write is an OUT-OF-BAND byte outside ratatui's diff, so the next
    /// frame does one full clear + repaint — the universal catch-all that keeps
    /// any terminal-side reaction to the bell from surviving as drift.
    pub fn take_bell(&mut self) -> bool {
        let pending = std::mem::take(&mut self.bell_pending);
        if pending {
            self.contaminate_terminal();
        }
        pending
    }

    // ---- Feature B: in-transcript search --------------------------------
    //
    // A modal find over the folded transcript rows cached in `transcript_rows`
    // (one logical, gutter-stripped string per visual row — the same coordinate
    // space the drag-to-copy selection uses). Matches carry the visual-row index,
    // so navigating one scrolls it into view and the renderer paints the span.

    /// Open the search bar (Ctrl+F). Starts empty; an empty query has no matches.
    pub fn open_search(&mut self) {
        self.search = Some(SearchState::default());
        // Drop any in-flight drag selection so the two highlight layers don't
        // fight over the same rows while search owns the transcript.
        self.selection = None;
        self.recompute_search_matches();
    }

    /// Close the search bar (Esc) and clear all match state.
    pub fn close_search(&mut self) {
        self.search = None;
    }

    /// Append a char to the query, rescan, and jump to the first match.
    fn search_input_char(&mut self, c: char) {
        if let Some(s) = self.search.as_mut() {
            s.query.push(c);
            s.current = 0;
        }
        self.recompute_search_matches();
        self.search_focus_current();
    }

    /// Delete the last query char, rescan, and re-anchor on the first match.
    fn search_backspace(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.query.pop();
            s.current = 0;
        }
        self.recompute_search_matches();
        self.search_focus_current();
    }

    /// Advance to the next match (wraps past the end), scrolling it into view.
    /// No-op when there are no matches.
    pub fn search_next(&mut self) {
        if let Some(s) = self.search.as_mut() {
            if s.matches.is_empty() {
                return;
            }
            s.current = (s.current + 1) % s.matches.len();
        }
        self.search_focus_current();
    }

    /// Step to the previous match (wraps past the start), scrolling it into view.
    /// No-op when there are no matches.
    pub fn search_prev(&mut self) {
        if let Some(s) = self.search.as_mut() {
            if s.matches.is_empty() {
                return;
            }
            let n = s.matches.len();
            s.current = (s.current + n - 1) % n;
        }
        self.search_focus_current();
    }

    /// Rescan the cached transcript rows for the current query and refill the
    /// match list (case-insensitive, non-overlapping). Called on every query
    /// edit and once on open; cheap (a substring sweep over the published rows).
    /// `current` is re-clamped so it never points past the end. Fail-open: an
    /// empty/absent query yields zero matches.
    pub fn recompute_search_matches(&mut self) {
        if self.search.is_none() {
            return;
        }
        let query: Vec<char> = self
            .search
            .as_ref()
            .map(|s| s.query.chars().collect())
            .unwrap_or_default();
        let mut matches: Vec<SearchMatch> = Vec::new();
        if !query.is_empty() {
            let rows = self.transcript_rows.borrow();
            for (ri, row) in rows.iter().enumerate() {
                let rc: Vec<char> = row.chars().collect();
                if rc.len() < query.len() {
                    continue;
                }
                let mut start = 0usize;
                while start + query.len() <= rc.len() {
                    if (0..query.len()).all(|k| chars_ci_eq(query[k], rc[start + k])) {
                        matches.push(SearchMatch {
                            row: ri,
                            start,
                            end: start + query.len(),
                        });
                        start += query.len(); // non-overlapping
                    } else {
                        start += 1;
                    }
                }
            }
        }
        if let Some(s) = self.search.as_mut() {
            if s.current >= matches.len() {
                s.current = 0;
            }
            s.matches = matches;
        }
    }

    /// The rows-from-bottom scroll offset that brings visual row `row` into the
    /// transcript viewport, roughly centered. Reads the renderer-published
    /// `transcript_max_scroll` (total hidden-above rows) and
    /// `transcript_viewport_rows`; the renderer re-clamps, so a stale bound is
    /// self-correcting.
    #[must_use]
    pub fn search_scroll_offset_for(&self, row: usize) -> usize {
        let max = self.transcript_max_scroll.get();
        let viewport = self.transcript_viewport_rows.get().max(1);
        let top_target = row.saturating_sub(viewport / 2);
        max.saturating_sub(top_target)
    }

    /// Scroll the transcript so the focused match is in view. No-op when there
    /// is no current match.
    fn search_focus_current(&self) {
        let Some(s) = self.search.as_ref() else {
            return;
        };
        if let Some(m) = s.matches.get(s.current) {
            self.set_transcript_scroll(self.search_scroll_offset_for(m.row));
        }
    }

    /// Key handler while the search bar is open (a mutually-exclusive mode —
    /// routed here from the top of [`Self::chat_key`] before the palette /
    /// mention / recall paths). Esc closes; Enter/↓ and Ctrl+N go to the next
    /// match, ↑ and Ctrl+P to the previous; Backspace edits; any other printable
    /// char filters live. Anything else is swallowed (search owns the keyboard).
    fn search_key(&mut self, key: KeyCode, mods: crossterm::event::KeyModifiers) -> Action {
        let ctrl = mods.contains(crossterm::event::KeyModifiers::CONTROL);
        let alt = mods.contains(crossterm::event::KeyModifiers::ALT);
        match key {
            KeyCode::Esc => {
                self.close_search();
                Action::None
            }
            KeyCode::Enter | KeyCode::Down => {
                self.search_next();
                Action::None
            }
            KeyCode::Up => {
                self.search_prev();
                Action::None
            }
            KeyCode::Char('n') if ctrl => {
                self.search_next();
                Action::None
            }
            KeyCode::Char('p') if ctrl => {
                self.search_prev();
                Action::None
            }
            // Literal BS/DEL char forms are folded to `Backspace` upstream by
            // the shared `input::keymap` mapping — one arm suffices.
            KeyCode::Backspace => {
                self.search_backspace();
                Action::None
            }
            // Printable char (no ctrl/alt) filters the query live. `n`/`N` type
            // into the box rather than navigating, so a query can contain them —
            // navigation lives on Enter/↑/↓/Ctrl+N/Ctrl+P (standard incremental
            // search). Shift is allowed (uppercase chars).
            KeyCode::Char(c) if !ctrl && !alt => {
                self.search_input_char(c);
                Action::None
            }
            _ => Action::None,
        }
    }

    // ── I3 — reverse prompt-history search (Ctrl+R) ─────────────────────────
    // A modal incremental find over the submitted-prompt ring (what the user
    // TYPED), distinct from the Ctrl+F transcript search (what's on screen).
    // Newest-first, deduplicated, with a live preview; Enter loads the focused
    // match into the input box for re-editing / resubmission, Esc cancels.

    /// True when at least one transcript row is long enough to fold — drives the
    /// Ctrl+R "fold the latest vs. open history search" disambiguation. Mirrors
    /// [`Self::toggle_last_collapsible`]'s scan.
    #[must_use]
    fn has_foldable(&self) -> bool {
        self.history.iter().any(message_is_collapsible)
    }

    /// Open the reverse prompt-history search (Ctrl+R). Snapshots the submitted-
    /// prompt ring deduplicated + newest-first and starts with an empty query
    /// (which matches everything, previewing the most-recent entry). No-op
    /// (fail-open) when there is no history to search. Drops any in-flight drag
    /// selection so the two highlight layers don't fight over the transcript.
    pub fn open_history_search(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        // Dedup newest-first: walk the ring back-to-front, keeping the FIRST
        // sighting of each distinct prompt (so a repeated command shows once, at
        // its most-recent position).
        let mut seen = std::collections::HashSet::new();
        let mut entries: Vec<String> = Vec::new();
        for e in self.input_history.iter().rev() {
            if seen.insert(e.as_str()) {
                entries.push(e.clone());
            }
        }
        if entries.is_empty() {
            return;
        }
        let mut st = HistorySearchState {
            entries,
            ..HistorySearchState::default()
        };
        Self::recompute_history_matches(&mut st);
        self.selection = None;
        self.history_search = Some(st);
    }

    /// Close the reverse-history search (Esc / after an accept). Leaves the input
    /// box untouched — the accept path loads the match first, then closes.
    pub fn close_history_search(&mut self) {
        self.history_search = None;
    }

    /// Refill `st.matches` with the indices of `st.entries` whose text contains
    /// the query (case-insensitive substring). The empty query matches
    /// everything; `current` is re-clamped so it never points past the end.
    /// Fail-open (a static helper, so callers avoid a double mutable borrow).
    fn recompute_history_matches(st: &mut HistorySearchState) {
        let q = st.query.to_lowercase();
        st.matches = st
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| q.is_empty() || e.to_lowercase().contains(q.as_str()))
            .map(|(i, _)| i)
            .collect();
        if st.current >= st.matches.len() {
            st.current = 0;
        }
    }

    /// Append a char to the query, rescan, and re-anchor on the newest match.
    fn history_search_input_char(&mut self, c: char) {
        if let Some(st) = self.history_search.as_mut() {
            st.query.push(c);
            st.current = 0;
            Self::recompute_history_matches(st);
        }
    }

    /// Delete the last query char, rescan, and re-anchor on the newest match.
    fn history_search_backspace(&mut self) {
        if let Some(st) = self.history_search.as_mut() {
            st.query.pop();
            st.current = 0;
            Self::recompute_history_matches(st);
        }
    }

    /// Step to an OLDER match (Ctrl+R / ↓ / Ctrl+N), wrapping past the oldest.
    /// No-op when there are no matches.
    pub fn history_search_older(&mut self) {
        if let Some(st) = self.history_search.as_mut() {
            if !st.matches.is_empty() {
                st.current = (st.current + 1) % st.matches.len();
            }
        }
    }

    /// Step to a NEWER match (↑ / Ctrl+P), wrapping past the newest. No-op when
    /// there are no matches.
    pub fn history_search_newer(&mut self) {
        if let Some(st) = self.history_search.as_mut() {
            if !st.matches.is_empty() {
                let n = st.matches.len();
                st.current = (st.current + n - 1) % n;
            }
        }
    }

    /// The currently-previewed history entry, if any (the focused match's text) —
    /// what the live in-box preview shows and what Enter would load.
    #[must_use]
    pub fn history_search_preview(&self) -> Option<&str> {
        let st = self.history_search.as_ref()?;
        let idx = *st.matches.get(st.current)?;
        st.entries.get(idx).map(String::as_str)
    }

    /// Accept the focused match (Enter): load it into the input box for editing /
    /// resubmission and close the overlay. When nothing matches it just closes
    /// (the prompt is left as-is). Resets the `↑↓` recall cursor + popover
    /// highlights so the loaded text re-evaluates cleanly.
    pub fn history_search_accept(&mut self) {
        if let Some(text) = self.history_search_preview().map(str::to_string) {
            self.input = text;
            self.input_cursor = self.input_len();
            self.input_history_idx = None;
            self.palette_selected = 0;
            self.mention_selected = 0;
        }
        self.close_history_search();
    }

    /// Key handler while the reverse prompt-history search is open (I3 — a
    /// mutually-exclusive mode routed here from the top of [`Self::chat_key`]
    /// before the palette / mention / recall paths). Esc cancels (prompt
    /// untouched); Enter loads the focused match; Ctrl+R / ↓ / Ctrl+N step to an
    /// older match, ↑ / Ctrl+P to a newer one; Backspace edits; any other
    /// printable char narrows live. Everything else is swallowed (it owns the
    /// keyboard).
    fn history_search_key(&mut self, key: KeyCode, mods: crossterm::event::KeyModifiers) -> Action {
        let ctrl = mods.contains(crossterm::event::KeyModifiers::CONTROL);
        let alt = mods.contains(crossterm::event::KeyModifiers::ALT);
        match key {
            KeyCode::Esc => {
                self.close_history_search();
                Action::None
            }
            KeyCode::Enter => {
                self.history_search_accept();
                Action::None
            }
            KeyCode::Down => {
                self.history_search_older();
                Action::None
            }
            KeyCode::Up => {
                self.history_search_newer();
                Action::None
            }
            // Repeated Ctrl+R steps to an older match (readline convention);
            // Ctrl+N mirrors ↓. `r`/`n` still TYPE into the query (they're only
            // navigation WITH Ctrl), so a query can contain them.
            KeyCode::Char('r' | 'n') if ctrl => {
                self.history_search_older();
                Action::None
            }
            KeyCode::Char('p') if ctrl => {
                self.history_search_newer();
                Action::None
            }
            // Literal BS/DEL char forms are folded to `Backspace` upstream by
            // the shared `input::keymap` mapping — one arm suffices.
            KeyCode::Backspace => {
                self.history_search_backspace();
                Action::None
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.history_search_input_char(c);
                Action::None
            }
            _ => Action::None,
        }
    }
}

/// Minimum elapsed time a turn/run must have taken before its completion bell
/// fires (Feature A). A quick chat reply (a couple seconds) shouldn't beep —
/// only work the user likely stepped away from.
const BELL_MIN_ELAPSED: std::time::Duration = std::time::Duration::from_secs(5);

/// Parse the `UMADEV_BELL` env value into the completion-bell enable flag.
/// Default ON; `0` / `false` / `off` / `no` (case-insensitive, trimmed) silence
/// it. Pure (takes the value) so it's unit-testable without mutating process env.
#[must_use]
fn bell_enabled_from_env(val: Option<&str>) -> bool {
    match val {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => true,
    }
}

/// Case-insensitive single-char equality that never changes the char count
/// (so match offsets stay aligned with the original row's char indices).
/// ASCII folds via `eq_ignore_ascii_case`; other scripts fall back to full
/// Unicode lowercase folding compared iterator-to-iterator.
fn chars_ci_eq(a: char, b: char) -> bool {
    a == b || a.eq_ignore_ascii_case(&b) || a.to_lowercase().eq(b.to_lowercase())
}

/// Parse a `## <heading>` section out of a markdown body and return the first
/// non-empty, non-italic-placeholder line under it. Returns `None` when the
/// section is absent or only contains placeholder text (`_(…)_`).
/// Minimal `which`: true when `program` is on PATH.
/// Mint a fresh RFC-4122 version-4-*formatted* logical chat-file id.
///
/// Not cryptographically random — it only needs to be unique per chat session
/// on one machine. It is never handed to a base as native resume authority.
/// Entropy mixes wall-clock nanoseconds, a per-process
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
    /// in `/sessions`; launch remains fresh until the user explicitly resumes.
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
    /// Immutable launch/sandbox identity attached to `base_session_id`.
    /// Missing on legacy chat files. Grok legacy or unverified identities are
    /// deliberately not resumed; the durable transcript still restores normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_resume_identity: Option<BaseResumeIdentity>,
    /// The conversation transcript, oldest → newest.
    pub messages: Vec<umadev_runtime::Message>,
    /// Wave 3 — the **visible display transcript**: the rendered rows the user
    /// actually saw (prose, structured tool rows, diff cards, system notes),
    /// bounded by [`HISTORY_CAP`]. Persisted alongside the base-facing
    /// [`Self::messages`] so an explicit `/resume` rebuilds the same screen
    /// instead of an empty conversation.
    /// `#[serde(default)]` for back-compat: an old file without the field loads
    /// as `None` and `load_chat` seeds prose rows from `messages` instead.
    /// Deserialized LENIENTLY, element-wise ([`lenient_display_rows`]): a
    /// corrupt / unknown row is skipped rather than failing the whole session
    /// (fail-open — the durable prose transcript always survives). An older
    /// binary reading a new file simply ignores this extra field.
    #[serde(default, deserialize_with = "lenient_display_rows")]
    pub display: Option<Vec<ChatMessage>>,
}

/// Lenient, element-wise deserializer for [`ChatSession::display`] (Wave 3).
///
/// The display transcript is a *reconstruction convenience*, never the durable
/// record — so its parse must be unable to take the session down. A field that
/// isn't an array yields `None` (→ prose seeding from `messages`); an array is
/// parsed row by row and any row that fails (hand-edited file, a variant from a
/// newer binary) is skipped; an all-corrupt/empty array again yields `None`.
fn lenient_display_rows<'de, D>(de: D) -> Result<Option<Vec<ChatMessage>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let value = serde_json::Value::deserialize(de)?;
    let serde_json::Value::Array(items) = value else {
        return Ok(None);
    };
    let rows: Vec<ChatMessage> = items
        .into_iter()
        .filter_map(|item| serde_json::from_value(item).ok())
        .collect();
    Ok(if rows.is_empty() { None } else { Some(rows) })
}

/// Seed a plain-prose display transcript from the durable `{role, content}`
/// turns — the Wave 3 fallback for an OLD session file persisted before the
/// `display` field existed (or one whose display rows were corrupt). Even a
/// legacy chat then reopens with the conversation visible instead of empty:
/// user turns render as `You` rows, everything else as `Host` prose.
fn seed_display_from_transcript(messages: &[umadev_runtime::Message]) -> Vec<ChatMessage> {
    messages
        .iter()
        .map(|m| ChatMessage {
            role: if m.role == "user" {
                ChatRole::You
            } else {
                ChatRole::Host
            },
            kind: MessageBody::Text(m.content.clone()),
            collapsed: false,
        })
        .collect()
}

/// Settle one restored display row (Wave 3): a tool row persisted mid-flight
/// (`Queued` / `Running` — e.g. the process died between the announce and the
/// result) is marked `Aborted`, mirroring the live interrupt-settle policy, so
/// a restored transcript can never show a spinner for a call that will never
/// return. Every other row passes through unchanged.
fn settle_restored_row(mut row: ChatMessage) -> ChatMessage {
    if let MessageBody::Tool(t) = &mut row.kind {
        if !t.status.is_terminal() {
            t.status = ToolStatus::Aborted;
        }
    }
    row
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
/// Resolve the effective Codex launch sandbox and publish it into the codex
/// driver's **thread-safe shared override** (`umadev_host::codex_session`).
/// Precedence: an override already in effect (an external `UMADEV_CODEX_SANDBOX`
/// launch env is seeded into that shared state on first read — advanced / CI) wins;
/// otherwise the project's `.umadevrc` `[codex] sandbox_mode` is resolved
/// (missing section → `danger-full-access`; an explicitly invalid value restricts
/// to `workspace-write`) and published. Returns the effective mode so
/// the caller can decide whether to warn.
///
/// Uses shared state, NOT a process-env `set_var`: the driver reads it from a
/// background task while a turn streams, so a runtime setenv racing its getenv is
/// UB. The launch env is read once (to seed the shared override), never mutated.
fn resolve_and_publish_codex_sandbox(
    project_root: &std::path::Path,
) -> umadev_agent::config::CodexSandbox {
    use umadev_agent::config::CodexSandbox;
    // An override already in effect (seeded from an external launch env, or set
    // earlier) is authoritative.
    if let Some(v) = umadev_host::codex_session::codex_sandbox_override() {
        if !v.trim().is_empty() {
            return CodexSandbox::parse_fail_open(&v);
        }
    }
    let _ = umadev_agent::config::migrate_legacy_generated_codex_sandbox(project_root);
    // Otherwise publish the `.umadevrc` choice so the codex driver honors it.
    let mode = umadev_agent::config::load_project_config(project_root)
        .codex
        .resolved_sandbox();
    umadev_host::codex_session::set_codex_sandbox(Some(mode.as_codex_arg()));
    mode
}

/// The Codex sandbox tier currently in effect, for DISPLAY (`/sandbox` with no
/// arg). Reads the driver's shared sandbox override first (what the codex driver
/// will actually use this session — seeded from the launch env at startup or set
/// by `/sandbox <mode>`), falling back to the project's `.umadevrc`. Pure read;
/// unlike [`resolve_and_publish_codex_sandbox`] it does NOT mutate any state.
fn effective_codex_sandbox(project_root: &std::path::Path) -> umadev_agent::config::CodexSandbox {
    use umadev_agent::config::CodexSandbox;
    if let Some(v) = umadev_host::codex_session::codex_sandbox_override() {
        if !v.trim().is_empty() {
            return CodexSandbox::parse_fail_open(&v);
        }
    }
    umadev_agent::config::load_project_config(project_root)
        .codex
        .resolved_sandbox()
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

fn claude_subagent_row(name: &str) -> Option<(&str, bool)> {
    let tail = name.strip_prefix(CLAUDE_SUBAGENT_STEM)?;
    if tail.is_empty() {
        return Some(("", false));
    }
    let tail = tail.strip_prefix(" · ")?;
    if tail == CLAUDE_SUBAGENT_WORKING {
        return Some(("", true));
    }
    if let Some(label) = tail.strip_suffix(" · 工作中…") {
        return Some((label, true));
    }
    Some((tail, false))
}

fn is_claude_subagent_result(summary: &str) -> bool {
    summary
        .strip_prefix(CLAUDE_SUBAGENT_STEM)
        .is_some_and(|tail| tail.starts_with(" · "))
}

/// The headline for a merged low-signal batch row, e.g. `读取 3 个文件,搜索` /
/// `inspected 3 items`. One localized phrase carries the live count; the count
/// is greatest-seen so a streamed value never visibly jumps backwards.
fn merged_batch_summary(lang: umadev_i18n::Lang, count: u32) -> String {
    umadev_i18n::tf(lang, "tui.tool.batch", &[&count.to_string()])
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

/// Decide whether an on-disk vendor pointer may be handed to a new base process.
///
/// Grok's ACP `session/load` occurs after process sandbox startup, so it cannot
/// perform the native saved-profile conflict check. Until the launch adapter
/// reports that native preflight as satisfied, even a syntactically complete
/// identity fails closed on every base: old chat files did not persist their
/// launch permission profile, so an Auto-created id cannot safely be loaded in
/// Plan/Guarded. The durable transcript remains the compatibility handoff. A
/// present typed identity must match every immutable field; Grok additionally
/// needs its native preflight and effective-state proof.
fn chat_resume_identity_allows_load(
    saved_backend: &str,
    current_backend: &str,
    saved: Option<&BaseResumeIdentity>,
    requested: Option<&BaseResumeIdentity>,
) -> bool {
    if saved_backend != current_backend {
        return false;
    }
    match (saved, requested) {
        (Some(saved), Some(requested)) => saved.permits_resume_as(requested, false),
        _ => false,
    }
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
        u[0],
        u[1],
        u[2],
        u[3],
        u[4],
        u[5],
        u[6],
        u[7],
        u[8],
        u[9],
        u[10],
        u[11],
        u[12],
        u[13],
        u[14],
        u[15]
    )
}

/// M3 — drain a child pipe into a buffer **capped at `cap` bytes**, on its own
/// thread, returning the captured bytes. A dedicated thread per stream avoids the
/// classic two-pipe deadlock (a single reader blocked on stdout while stderr's
/// pipe fills). Reading stops at the cap (the read end is then dropped, so the
/// child blocks on a full pipe / takes `EPIPE` and is killed at the run deadline)
/// and on EOF or any read error — so a runaway emitter (`yes`, `cat /dev/zero`)
/// can NEVER buffer unbounded into memory the way `Command::output()` does.
fn spawn_capped_pipe_reader<R: std::io::Read + Send + 'static>(
    src: Option<R>,
    cap: usize,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::new();
        let Some(mut r) = src else {
            return buf;
        };
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                // EOF or a read error: stop draining this stream.
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(buf.len());
                    if room == 0 {
                        break; // cap reached — stop reading + drop the pipe end
                    }
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                    if buf.len() >= cap {
                        break;
                    }
                }
            }
        }
        buf
    })
}

/// Run a one-off `!`-prefixed shell command in `root` and return
/// `(success, combined_output)`. stdout + stderr are merged and bounded
/// ([`bound_shell_output`]); a nonzero exit appends its code, a killed process a
/// generic failure note, a spawn error an error line, and a >10s hang a timeout
/// note — so the call ALWAYS returns and never panics or freezes the UI. The
/// command is run via the platform shell (`sh -c` on unix, `cmd /C` on Windows)
/// with its stdout/stderr **piped and drained incrementally into bounded
/// buffers** ([`spawn_capped_pipe_reader`]); a command still running past the
/// 10s budget is **killed and reaped** (no orphan left running, no unbounded
/// memory) before the timeout note returns. NOT routed to the base — a local
/// convenience shell. M3: the previous `Command::output()` on a worker thread
/// buffered stdout/stderr to EOF in memory and never killed the child on
/// timeout, so `!yes` / `!cat /dev/zero` / `!tail -f` could OOM + run on.
fn run_bang_command(root: &std::path::Path, cmd: &str, lang: umadev_i18n::Lang) -> (bool, String) {
    // Per-stream in-memory read cap (M3). Far above `bound_shell_output`'s
    // 300-line / 16k-char display trim, so nothing visible is lost, yet a runaway
    // stream is hard-bounded in memory.
    const READ_CAP: usize = 256 * 1024;
    const BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
    const POLL: std::time::Duration = std::time::Duration::from_millis(20);

    #[cfg(windows)]
    let mut command = std::process::Command::new("cmd");
    #[cfg(windows)]
    command.args(["/C", cmd]);
    #[cfg(not(windows))]
    let mut command = std::process::Command::new("sh");
    #[cfg(not(windows))]
    command.args(["-c", cmd]);

    let mut child = match command
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                false,
                umadev_i18n::tf(lang, "tui.bang.spawn_failed", &[&e.to_string()]),
            );
        }
    };

    // Drain each stream on its own thread into a bounded buffer.
    let h_out = spawn_capped_pipe_reader(child.stdout.take(), READ_CAP);
    let h_err = spawn_capped_pipe_reader(child.stderr.take(), READ_CAP);

    // Wait for exit, bounded; KILL + reap on the deadline so a hung command never
    // runs on as an orphan (the readers then EOF and join).
    let deadline = std::time::Instant::now() + BUDGET;
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(s)) => break (Some(s), false),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap + close pipes so readers EOF
                    break (None, true);
                }
                std::thread::sleep(POLL);
            }
            Err(_) => break (None, false),
        }
    };

    // Readers end on pipe EOF (after exit / kill) or at the cap; join their
    // captured bytes. A join error (panicked reader — not expected) is treated
    // as empty so the call still returns (fail-open).
    let stdout_bytes = h_out.join().unwrap_or_default();
    let stderr_bytes = h_err.join().unwrap_or_default();

    let mut body = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes);
    if !stderr.trim().is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&stderr);
    }
    let bounded = bound_shell_output(&body);

    if timed_out {
        // The command was killed at the deadline — surface any partial output
        // above the timeout note.
        let note = umadev_i18n::t(lang, "tui.bang.timeout").to_string();
        let text = if bounded.trim().is_empty() {
            note
        } else {
            format!("{bounded}\n{note}")
        };
        return (false, text);
    }

    if let Some(status) = status {
        if status.success() {
            let text = if bounded.trim().is_empty() {
                umadev_i18n::t(lang, "tui.bang.no_output").to_string()
            } else {
                bounded
            };
            return (true, text);
        }
        // A nonzero exit reports its code (or a generic note when the process
        // was killed by a signal and has no code), keeping any output above it.
        let note = match status.code() {
            Some(code) => umadev_i18n::tf(lang, "tui.bang.exit", &[&code.to_string()]),
            None => umadev_i18n::t(lang, "tui.bang.failed").to_string(),
        };
        let text = if bounded.trim().is_empty() {
            note
        } else {
            format!("{bounded}\n{note}")
        };
        return (false, text);
    }

    // `try_wait` errored (rare) — report a generic failure, keeping any output.
    let note = umadev_i18n::t(lang, "tui.bang.failed").to_string();
    let text = if bounded.trim().is_empty() {
        note
    } else {
        format!("{bounded}\n{note}")
    };
    (false, text)
}

/// Cap a one-off shell command's output so a chatty command can't flood the
/// transcript: at most 300 lines, then a hard 16k-char ceiling. The folded
/// tool-row renderer still applies its own head-N preview on top — this is the
/// storage bound, not the display fold.
fn bound_shell_output(body: &str) -> String {
    const MAX_LINES: usize = 300;
    const MAX_CHARS: usize = 16_000;
    let mut out: String = body.lines().take(MAX_LINES).collect::<Vec<_>>().join("\n");
    if out.chars().count() > MAX_CHARS {
        out = out.chars().take(MAX_CHARS).collect();
    }
    out
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

/// Render a structured gate [`GateChoice`] as PROSE for text-question mode: the
/// localized question, its options as plain bullets (no numbered "pick a number"
/// framing), and a hint to answer in natural language. Used when the user set
/// `question_form = "text"` so the gate reads as a conversational question instead
/// of a picker. The free-text reply path (`classify_reply`) already maps their
/// words to the decision, so no picker is needed. Labels are localized via `t()`
/// (an i18n key resolves; a literal passes through verbatim).
fn gate_choice_prose(choice: &GateChoice, lang: umadev_i18n::Lang) -> String {
    let mut s = umadev_i18n::t(lang, &choice.question).to_string();
    for opt in &choice.options {
        s.push_str("\n  - ");
        s.push_str(umadev_i18n::t(lang, &opt.label));
    }
    s.push('\n');
    s.push_str(umadev_i18n::t(lang, "question.text_hint"));
    s
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

/// Extract the canonical seat role id from a `PlanPosted` summary's (or step
/// title's) trailing `(seat)` token — `… (frontend)` → `frontend-engineer`,
/// using the same alias set the agent resolves. **Fail-open**: no parenthesised
/// suffix, or a token that doesn't resolve to a known seat, yields `""` — an
/// unattributed step simply never joins the live roster (anti-theater: a phantom
/// seat is never invented from a malformed summary).
pub(crate) fn parse_seat(summary: &str) -> String {
    let trimmed = summary.trim_end();
    if let Some(open) = trimmed.rfind('(') {
        if trimmed.ends_with(')') && open + 1 < trimmed.len() - 1 {
            let inner = &trimmed[open + 1..trimmed.len() - 1];
            if let Some(seat) = umadev_agent::Seat::from_alias(inner) {
                return seat.role_id().to_string();
            }
        }
    }
    String::new()
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

/// Trim a (possibly multi-line) run requirement to a single-line summary for the
/// `/tasks` list + the compact run chip: the first non-empty line, clipped to a
/// readable length on a char boundary (CJK-safe) with an ellipsis when cut.
fn task_summary(requirement: &str) -> String {
    const MAX_CHARS: usize = 60;
    let redacted = umadev_agent::task_lifecycle::redact_task_text(requirement);
    let first = redacted
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim();
    if first.chars().count() > MAX_CHARS {
        let head: String = first.chars().take(MAX_CHARS).collect();
        format!("{head}…")
    } else {
        first.to_string()
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

const LESSONS_LINE_WIDTH: usize = 80;

fn lesson_display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

fn split_lesson_token(token: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for ch in token.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if !current.is_empty() && current_width + char_width > max_width {
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += char_width;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn wrap_lesson_text(text: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for token in text.split_whitespace() {
        let token_width = lesson_display_width(token);
        let separator = usize::from(!current.is_empty());
        if token_width <= max_width
            && lesson_display_width(&current) + separator + token_width <= max_width
        {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(token);
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if token_width <= max_width {
            current.push_str(token);
            continue;
        }
        let chunks = split_lesson_token(token, max_width);
        let chunk_count = chunks.len();
        for (index, chunk) in chunks.into_iter().enumerate() {
            if index + 1 == chunk_count {
                current = chunk;
            } else {
                lines.push(chunk);
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn wrap_lesson_message(message: &str) -> String {
    message
        .lines()
        .flat_map(|line| wrap_lesson_text(line, LESSONS_LINE_WIDTH))
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_lesson_field(out: &mut String, prefix: &str, value: &str) {
    let prefix = format!("{prefix} ");
    let prefix_width = lesson_display_width(&prefix);
    let available = LESSONS_LINE_WIDTH.saturating_sub(prefix_width).max(16);
    let lines = wrap_lesson_text(value, available);
    if lines.is_empty() {
        return;
    }
    let continuation = " ".repeat(prefix_width);
    for (index, line) in lines.iter().enumerate() {
        if index == 0 {
            out.push_str(&prefix);
        } else {
            out.push_str(&continuation);
        }
        out.push_str(line);
        out.push('\n');
    }
}

fn push_pitfall_observations(
    out: &mut String,
    lang: umadev_i18n::Lang,
    observations: &[umadev_agent::PitfallObservation],
) {
    if observations.is_empty() {
        return;
    }
    out.push_str(umadev_i18n::t(lang, "pitfalls.observations_header"));
    out.push('\n');
    let unknown = umadev_i18n::t(lang, "pitfalls.time.unknown");
    for observation in observations {
        let observed_at = if observation.observed_at.trim().is_empty() {
            unknown
        } else {
            observation.observed_at.trim()
        };
        let episode = compact_audit_id(&observation.episode_id, 8, 6);
        let evidence = compact_audit_id(&observation.evidence_hash, 12, 0);
        let value = umadev_i18n::tf(
            lang,
            "pitfalls.observation_value",
            &[
                observed_at,
                if episode.is_empty() {
                    unknown
                } else {
                    &episode
                },
                if evidence.is_empty() {
                    unknown
                } else {
                    &evidence
                },
            ],
        );
        push_lesson_field(out, "    -", &value);
    }
}

fn pitfall_first_observed(
    lang: umadev_i18n::Lang,
    value: &str,
    timeline_complete: bool,
    has_observations: bool,
) -> String {
    if value.trim().is_empty() {
        return umadev_i18n::t(lang, "pitfalls.time.unknown").to_string();
    }
    if !timeline_complete && !has_observations {
        return umadev_i18n::tf(lang, "pitfalls.time.legacy_value", &[value.trim()]);
    }
    value.trim().to_string()
}

fn push_wrapped_pitfall_line(out: &mut String, value: &str) {
    out.push_str(&wrap_lesson_message(value));
    out.push('\n');
}

#[cfg(test)]
#[path = "app/tests.rs"]
mod tests;
