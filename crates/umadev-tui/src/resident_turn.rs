//! Resident chat-turn liveness and runaway safeguards.

use umadev_agent::{director_loop::IdleBudget, RoutePlan};
use umadev_runtime::{BaseSession, SessionEvent};

/// Pull the next [`SessionEvent`] under the liveness-based idle watchdog — the
/// local analogue of the agent crate's
/// [`umadev_agent::director_loop::next_event_idle`], so chat and `/run` pumps
/// follow the same rules.
///
/// The window is selected by whether a tool is running:
///
/// - During a tool call it is a liveness poll. A live base keeps waiting,
///   while a dead base settles as `Ok(None)`.
/// - Outside a tool call it is the hang deadline and settles as `Err(())`.
///
/// The independent absolute `deadline` still bounds the complete turn, so
/// continuous output cannot keep a resident turn alive forever.
#[allow(clippy::result_unit_err)]
pub(super) async fn next_chat_event_idle(
    session: &mut dyn BaseSession,
    budget: IdleBudget,
    in_tool_call: bool,
    deadline: Option<std::time::Instant>,
) -> Result<Option<SessionEvent>, ()> {
    let window = budget.window(in_tool_call);
    // A live tool normally emits a result or progress. If it produces nothing
    // for this entire ceiling it is wedged, rather than merely long-running.
    let silence_ceiling = chat_tool_silence_ceiling();
    let waited_since = std::time::Instant::now();

    loop {
        let now = std::time::Instant::now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            return Err(());
        }
        let wait = deadline.map_or(window, |deadline| {
            deadline.saturating_duration_since(now).min(window)
        });
        if let Ok(event) = tokio::time::timeout(wait, session.next_event()).await {
            return Ok(event);
        }
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err(());
        }
        if in_tool_call {
            if session.try_exit_status().is_some() {
                return Ok(None);
            }
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                return Err(());
            }
            if waited_since.elapsed() >= silence_ceiling {
                return Err(());
            }
            continue;
        }
        return Err(());
    }
}

/// Ceiling on continuous in-tool silence for one chat turn. Base output resets
/// this idle timer; [`ResidentTurnLimiter`] independently enforces absolute
/// wall-clock and resource ceilings.
pub(super) fn chat_tool_silence_ceiling() -> std::time::Duration {
    let secs = std::env::var("UMADEV_CHAT_TOOL_MAX_SILENCE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(1_800);
    std::time::Duration::from_secs(secs)
}

/// Idle budget shared with the director loop.
pub(super) fn chat_idle_budget() -> IdleBudget {
    IdleBudget::from_env()
}

// Ten minutes was too short for a healthy coding turn. This is now the sliding
// no-progress window, not an absolute guillotine: productive stream/tool events
// renew it, while the independent absolute ceiling below still bounds a base that
// emits forever without converging.
pub(super) const DEFAULT_RESIDENT_TURN_MAX_SECS: u64 = 1_800;
pub(super) const HARD_RESIDENT_TURN_MAX_SECS: u64 = 3_600;
pub(super) const DEFAULT_RESIDENT_TURN_MAX_TOKENS: u64 = 500_000;
pub(super) const HARD_RESIDENT_TURN_MAX_TOKENS: u64 = 2_000_000;
pub(super) const DEFAULT_RESIDENT_TURN_MAX_EVENTS: u64 = 20_000;
const HARD_RESIDENT_TURN_MAX_EVENTS: u64 = 100_000;
pub(super) const HARD_RESIDENT_TURN_MAX_TOOL_CALLS: u64 = 640;

fn bounded_resident_limit(key: &str, default: u64, hard_max: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map_or(default, |value| value.min(hard_max))
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum ResidentTurnLimit {
    WallClock { seconds: u64 },
    StreamTokens { used: u64, ceiling: u64 },
    ReportedTokens { used: u64, ceiling: u64 },
    Events { used: u64, ceiling: u64 },
    ToolCalls { used: u64, ceiling: u64 },
}

impl ResidentTurnLimit {
    pub(super) fn note(self) -> String {
        let detail = match self {
            Self::WallClock { seconds } => format!("wall-clock {seconds}s"),
            Self::StreamTokens { used, ceiling } => {
                format!("stream token estimate {used}/{ceiling}")
            }
            Self::ReportedTokens { used, ceiling } => {
                format!("base-reported turn tokens {used}/{ceiling}")
            }
            Self::Events { used, ceiling } => format!("stream events {used}/{ceiling}"),
            Self::ToolCalls { used, ceiling } => format!("tool calls {used}/{ceiling}"),
        };
        format!(
            "[warn] 本轮触发防失控硬上限({detail})，已停止且不会自动重发或进入评审；\
             会话保持可继续，请缩小需求或明确继续 / resident turn hit its runaway ceiling; \
             stopped without automatic replay or post-build review"
        )
    }
}

/// Hard runaway backstop for one resident (non-Director) turn.
///
/// Idle liveness remains independent: a healthy long tool can keep emitting
/// heartbeats, but a resident turn cannot evade every bound by streaming
/// forever. User overrides are accepted only inside global maxima, so an
/// accidental `999999999` never disables the product safety ceiling.
#[derive(Debug)]
pub(super) struct ResidentTurnLimiter {
    pub(super) started: std::time::Instant,
    pub(super) last_progress: std::time::Instant,
    pub(super) wall_seconds: u64,
    pub(super) absolute_seconds: u64,
    pub(super) token_ceiling: u64,
    stream_token_ceiling: u64,
    pub(super) event_ceiling: u64,
    pub(super) tool_call_ceiling: u64,
    stream_tokens: u64,
    events: u64,
    tool_calls: u64,
}

impl ResidentTurnLimiter {
    pub(super) fn new(route: &RoutePlan) -> Self {
        let wall_seconds = bounded_resident_limit(
            "UMADEV_CHAT_TURN_MAX_SECS",
            DEFAULT_RESIDENT_TURN_MAX_SECS,
            HARD_RESIDENT_TURN_MAX_SECS,
        );
        let token_ceiling = bounded_resident_limit(
            "UMADEV_CHAT_TURN_MAX_TOKENS",
            DEFAULT_RESIDENT_TURN_MAX_TOKENS,
            HARD_RESIDENT_TURN_MAX_TOKENS,
        );
        let event_ceiling = bounded_resident_limit(
            "UMADEV_CHAT_TURN_MAX_EVENTS",
            DEFAULT_RESIDENT_TURN_MAX_EVENTS,
            HARD_RESIDENT_TURN_MAX_EVENTS,
        );
        let default_tool_calls = u64::from(route.est_budget.max_tool_calls)
            .saturating_mul(2)
            .clamp(16, HARD_RESIDENT_TURN_MAX_TOOL_CALLS);
        let tool_call_ceiling = bounded_resident_limit(
            "UMADEV_CHAT_TURN_MAX_TOOL_CALLS",
            default_tool_calls,
            HARD_RESIDENT_TURN_MAX_TOOL_CALLS,
        );
        let stream_token_ceiling = u64::from(route.est_budget.max_tokens)
            .saturating_mul(4)
            .max(64_000)
            .min(token_ceiling);
        Self {
            started: std::time::Instant::now(),
            last_progress: std::time::Instant::now(),
            wall_seconds,
            absolute_seconds: wall_seconds
                .saturating_mul(4)
                .min(HARD_RESIDENT_TURN_MAX_SECS),
            token_ceiling,
            stream_token_ceiling,
            event_ceiling,
            tool_call_ceiling,
            stream_tokens: 0,
            events: 0,
            tool_calls: 0,
        }
    }

    pub(super) fn deadline(&self) -> std::time::Instant {
        let absolute = self.started + std::time::Duration::from_secs(self.absolute_seconds);
        self.last_progress
            .checked_add(std::time::Duration::from_secs(self.wall_seconds))
            .map_or(absolute, |sliding| sliding.min(absolute))
    }

    pub(super) fn wall_limit(&self) -> Option<ResidentTurnLimit> {
        let now = std::time::Instant::now();
        let absolute_elapsed = now.saturating_duration_since(self.started)
            >= std::time::Duration::from_secs(self.absolute_seconds);
        let idle_elapsed = now.saturating_duration_since(self.last_progress)
            >= std::time::Duration::from_secs(self.wall_seconds);
        (absolute_elapsed || idle_elapsed).then_some(ResidentTurnLimit::WallClock {
            seconds: if absolute_elapsed {
                self.absolute_seconds
            } else {
                self.wall_seconds
            },
        })
    }

    pub(super) fn observe(&mut self, event: &SessionEvent) -> Option<ResidentTurnLimit> {
        self.events = self.events.saturating_add(1);
        if self.events > self.event_ceiling {
            return Some(ResidentTurnLimit::Events {
                used: self.events,
                ceiling: self.event_ceiling,
            });
        }
        match event {
            SessionEvent::TextDelta(delta) | SessionEvent::ThinkingDelta(delta) => {
                self.stream_tokens = self
                    .stream_tokens
                    .saturating_add(umadev_agent::director_loop::approx_tokens(delta));
                if self.stream_tokens > self.stream_token_ceiling {
                    return Some(ResidentTurnLimit::StreamTokens {
                        used: self.stream_tokens,
                        ceiling: self.stream_token_ceiling,
                    });
                }
            }
            SessionEvent::ToolCall { .. } | SessionEvent::ToolCallCorrelated { .. } => {
                self.tool_calls = self.tool_calls.saturating_add(1);
                if self.tool_calls > self.tool_call_ceiling {
                    return Some(ResidentTurnLimit::ToolCalls {
                        used: self.tool_calls,
                        ceiling: self.tool_call_ceiling,
                    });
                }
            }
            SessionEvent::TurnDone {
                usage: Some(usage), ..
            } if usage.total_tokens > self.token_ceiling => {
                return Some(ResidentTurnLimit::ReportedTokens {
                    used: usage.total_tokens,
                    ceiling: self.token_ceiling,
                });
            }
            _ => {}
        }
        if matches!(
            event,
            SessionEvent::TextDelta(_)
                | SessionEvent::ThinkingDelta(_)
                | SessionEvent::ToolCall { .. }
                | SessionEvent::ToolCallCorrelated { .. }
                | SessionEvent::ToolProgressCorrelated { .. }
                | SessionEvent::ToolOutputDelta(_)
                | SessionEvent::ToolOutputDeltaCorrelated { .. }
                | SessionEvent::ToolOutputSnapshot(_)
                | SessionEvent::ToolOutputSnapshotCorrelated { .. }
                | SessionEvent::ToolResult { .. }
                | SessionEvent::ToolResultCorrelated { .. }
        ) {
            self.last_progress = std::time::Instant::now();
        }
        self.wall_limit()
    }
}
