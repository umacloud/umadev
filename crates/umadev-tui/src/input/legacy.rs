//! The legacy `crossterm::EventStream` input path, plus the Windows key-burst
//! coalescer that gives a console paste the same protections as a bracketed
//! paste.
//!
//! The Windows console never produces `Event::Paste`: pasted text arrives as
//! one key event per character. Handled key by key, a long paste was cut at the
//! composer's 8,192-character limit without notice, every pasted Tab was
//! dropped (or accepted a completion and rewrote the text), and the large-paste
//! chip never appeared. So when the first key read is a printable character and
//! more text keys are ALREADY queued behind it, the queued run is delivered as
//! one `Event::Paste` — the path a bracketed paste takes (chip, tabs, the 1 MiB
//! cap with its notice, CRLF handling). Nothing ever waits for input: a key
//! with nothing queued behind it — ordinary typing — is returned untouched.

use std::task::Poll;
use std::time::Instant;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::{Stream, StreamExt};

use super::keymap::normalize_key;

/// Size ceiling for one coalesced burst: the 1 MiB envelope the owned decoder
/// applies to a bracketed paste. The app rejects an over-cap paste with a
/// visible notice.
const KEY_BURST_CAP: usize = 1024 * 1024;

/// The legacy crossterm stream, optionally coalescing queued key bursts into
/// pastes (Windows only — see the module docs).
pub struct LegacyInput<S = EventStream> {
    /// The crossterm event stream (a scripted stream in tests).
    stream: S,
    /// Whether queued key bursts are delivered as pastes.
    coalesce: bool,
    /// Byte ceiling for one burst ([`KEY_BURST_CAP`] outside tests).
    cap: usize,
    /// An event read while draining a burst that did not belong to it; handed
    /// out by the next call.
    pending: Option<std::io::Result<Event>>,
    /// When the last burst was handed out. A console paste can reach the queue
    /// in chunks, and a chunk that follows right behind one may begin with the
    /// Enter or Tab of the same paste.
    last_burst: Option<Instant>,
    /// The last burst overflowed the cap: chunks continuing it are dropped,
    /// the way the owned decoder drops an oversized paste to its end marker.
    discarding: bool,
}

impl<S> LegacyInput<S>
where
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    /// Wrap `stream`; `coalesce` turns on key-burst pastes.
    pub fn new(stream: S, coalesce: bool) -> Self {
        Self {
            stream,
            coalesce,
            cap: KEY_BURST_CAP,
            pending: None,
            last_burst: None,
            discarding: false,
        }
    }

    /// Yield the next event: a queued key burst as one `Event::Paste`,
    /// everything else unchanged. `None` only when the stream ends.
    pub async fn next(&mut self) -> Option<std::io::Result<Event>> {
        loop {
            let (item, queued) = match self.pending.take() {
                Some(item) => (item, true),
                None => match poll_ready(&mut self.stream).await {
                    Some(item) => (item?, true),
                    None => (self.stream.next().await?, false),
                },
            };
            let first = match item {
                Ok(first) if self.coalesce => first,
                other => return Some(other),
            };
            let continuation = self.last_burst.is_some_and(|at| {
                queued || at.elapsed() <= crate::app::PASTE_BURST_GAP
            });
            let Some(c) = burst_char(&first, continuation) else {
                self.last_burst = None;
                self.discarding = false;
                return Some(Ok(first));
            };
            let mut burst = KeyBurst::new(c, self.cap);
            self.drain_burst(&mut burst).await;
            if burst.chars == 1 && !continuation {
                // A lone key with nothing queued behind it: ordinary typing.
                self.last_burst = None;
                self.discarding = false;
                return Some(Ok(first));
            }
            self.last_burst = Some(Instant::now());
            if self.discarding && continuation {
                continue;
            }
            self.discarding = burst.overflowed;
            return Some(Ok(Event::Paste(burst.text)));
        }
    }

    /// Move every event ALREADY queued behind a burst's first key into `burst`:
    /// text keys join it, key releases are skipped, and the first other event
    /// (or error) is parked for the next call. Never waits for input.
    async fn drain_burst(&mut self, burst: &mut KeyBurst) {
        while let Some(item) = poll_ready(&mut self.stream).await {
            let event = match item {
                Some(Ok(event)) => event,
                Some(Err(error)) => {
                    self.pending = Some(Err(error));
                    return;
                }
                // The stream ended; the next call reports it.
                None => return,
            };
            if matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Release) {
                continue;
            }
            match burst_char(&event, true) {
                Some(c) => burst.push(c),
                None => {
                    self.pending = Some(Ok(event));
                    return;
                }
            }
        }
    }
}

/// Poll `stream` once without waiting: `None` when nothing is queued right
/// now. The poll uses the caller's task context, so a `Pending` leaves the
/// stream's wake-up registered for the task's next real wait.
async fn poll_ready<S>(stream: &mut S) -> Option<Option<S::Item>>
where
    S: Stream + Unpin,
{
    std::future::poll_fn(|cx| {
        Poll::Ready(match stream.poll_next_unpin(cx) {
            Poll::Ready(item) => Some(item),
            Poll::Pending => None,
        })
    })
    .await
}

/// The text one key adds to a burst, or `None` when it ends the burst. A burst
/// OPENS only on a printable character typed without Ctrl/Alt, so an Enter or
/// Tab read first stays a key and a real submit is never swallowed. Inside a
/// burst (or a chunk continuing one) Enter is a line break — ConPTY encodes a
/// pasted LF as Ctrl+Enter — Ctrl+J is a newline, and Tab is a tab.
fn burst_char(event: &Event, inside: bool) -> Option<char> {
    let Event::Key(key) = event else {
        return None;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    let (code, mods) = normalize_key(key.code, key.modifiers);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    match code {
        KeyCode::Char(c) if !ctrl && !alt => Some(c),
        _ if !inside || alt => None,
        KeyCode::Enter if ctrl => Some('\n'),
        KeyCode::Enter => Some('\r'),
        KeyCode::Char('j') if ctrl => Some('\n'),
        KeyCode::Tab if !ctrl => Some('\t'),
        _ => None,
    }
}

/// Text accumulated from one key burst, bounded by a byte cap.
struct KeyBurst {
    text: String,
    chars: usize,
    cap: usize,
    /// The text crossed the cap; later keys are drained but not kept.
    overflowed: bool,
}

impl KeyBurst {
    fn new(first: char, cap: usize) -> Self {
        let mut burst = Self {
            text: String::new(),
            chars: 0,
            cap,
            overflowed: false,
        };
        burst.push(first);
        burst
    }

    /// Append `c` unless the burst already overflowed. The char that crosses
    /// the cap is kept, so the app sees an over-cap paste and rejects it with
    /// its notice instead of inserting a silently truncated one.
    fn push(&mut self, c: char) {
        if self.overflowed {
            return;
        }
        self.text.push(c);
        self.chars += 1;
        self.overflowed = self.text.len() > self.cap;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::Context;
    use std::time::Duration;

    use crossterm::event::KeyEvent;

    use super::*;

    /// A scripted event stream: `Some(event)` is queued input, `None` is a
    /// moment with nothing queued (one `Pending`), and the end of the script
    /// ends the stream.
    struct Scripted(VecDeque<Option<std::io::Result<Event>>>);

    impl Stream for Scripted {
        type Item = std::io::Result<Event>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.0.pop_front() {
                Some(Some(item)) => Poll::Ready(Some(item)),
                Some(None) => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                None => Poll::Ready(None),
            }
        }
    }

    fn press(code: KeyCode, mods: KeyModifiers) -> Option<std::io::Result<Event>> {
        Some(Ok(Event::Key(KeyEvent::new(code, mods))))
    }

    fn release(code: KeyCode) -> Option<std::io::Result<Event>> {
        Some(Ok(Event::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ))))
    }

    fn chars(text: &str) -> Vec<Option<std::io::Result<Event>>> {
        text.chars()
            .flat_map(|c| [press(KeyCode::Char(c), KeyModifiers::NONE), release(KeyCode::Char(c))])
            .collect()
    }

    fn legacy(script: Vec<Option<std::io::Result<Event>>>) -> LegacyInput<Scripted> {
        LegacyInput::new(Scripted(script.into()), true)
    }

    async fn next_event(input: &mut LegacyInput<Scripted>) -> Event {
        input.next().await.expect("an event").expect("no io error")
    }

    #[tokio::test]
    async fn a_queued_console_paste_arrives_as_one_paste_with_its_tabs() {
        // 9,000 pasted chars plus a Tab, all queued at once (each key with its
        // release record): ONE paste, nothing cut, the tab kept.
        let mut script = chars(&"x".repeat(9_000));
        script.push(press(KeyCode::Tab, KeyModifiers::NONE));
        script.extend(chars("end"));
        let mut input = legacy(script);
        let Event::Paste(text) = next_event(&mut input).await else {
            panic!("a queued key burst must become one paste");
        };
        assert_eq!(text.chars().count(), 9_004);
        assert!(text.ends_with("\tend"), "the pasted tab survives");
        assert!(input.next().await.is_none(), "the whole burst was consumed");

        // Through the app: the paste becomes a chip whose stash keeps the tab.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = crate::app::App::new(
            "burst",
            crate::config::UserConfig::default(),
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        app.handle_paste(&text);
        assert_eq!(app.text_stash.len(), 1, "a 9,004-char paste is chipped");
        assert_eq!(app.text_stash[0], text, "all of it, tab included");
    }

    #[tokio::test]
    async fn ordinary_typing_stays_one_key_per_keystroke() {
        let mut input = legacy(vec![
            press(KeyCode::Char('h'), KeyModifiers::NONE),
            None,
            release(KeyCode::Char('h')),
            None,
            press(KeyCode::Char('i'), KeyModifiers::NONE),
            None,
        ]);
        assert!(matches!(
            next_event(&mut input).await,
            Event::Key(k) if k.code == KeyCode::Char('h') && k.kind == KeyEventKind::Press
        ));
        assert!(matches!(
            next_event(&mut input).await,
            Event::Key(k) if k.kind == KeyEventKind::Release
        ));
        assert!(matches!(
            next_event(&mut input).await,
            Event::Key(k) if k.code == KeyCode::Char('i')
        ));
    }

    #[tokio::test]
    async fn an_enter_read_first_stays_a_submit_key() {
        // ConPTY encodes one Enter press as CR LF (Enter, then Ctrl+Enter). A
        // burst never opens on Enter, so the submit is not turned into text.
        let mut input = legacy(vec![
            press(KeyCode::Enter, KeyModifiers::NONE),
            press(KeyCode::Enter, KeyModifiers::CONTROL),
        ]);
        assert!(matches!(
            next_event(&mut input).await,
            Event::Key(k) if k.code == KeyCode::Enter && k.modifiers.is_empty()
        ));
        assert!(matches!(
            next_event(&mut input).await,
            Event::Key(k) if k.code == KeyCode::Enter
        ));
    }

    #[tokio::test]
    async fn a_burst_ends_at_the_first_non_text_event() {
        let mut script = chars("ab");
        script.push(press(KeyCode::Left, KeyModifiers::NONE));
        script.extend(chars("c"));
        let mut input = legacy(script);
        assert_eq!(next_event(&mut input).await, Event::Paste("ab".into()));
        assert!(matches!(
            next_event(&mut input).await,
            Event::Key(k) if k.code == KeyCode::Left
        ));
        assert!(matches!(
            next_event(&mut input).await,
            Event::Key(k) if k.code == KeyCode::Char('c')
        ));
    }

    #[tokio::test]
    async fn line_breaks_inside_a_burst_and_its_next_chunk_stay_text() {
        // A multi-line paste reaching the queue in two chunks: the CR LF inside
        // the first stays text, and the second chunk (right behind the first)
        // may begin with Enter without submitting.
        let mut script = chars("/quit");
        script.push(press(KeyCode::Enter, KeyModifiers::NONE));
        script.push(press(KeyCode::Enter, KeyModifiers::CONTROL));
        script.extend(chars("第二行"));
        script.push(None);
        script.push(press(KeyCode::Enter, KeyModifiers::NONE));
        script.extend(chars("end"));
        let mut input = legacy(script);
        assert_eq!(
            next_event(&mut input).await,
            Event::Paste("/quit\r\n第二行".into())
        );
        assert_eq!(next_event(&mut input).await, Event::Paste("\rend".into()));
    }

    #[tokio::test]
    async fn an_enter_long_after_a_burst_is_a_real_submit() {
        let mut script = chars("ab");
        script.push(None);
        script.push(None);
        script.push(press(KeyCode::Enter, KeyModifiers::NONE));
        let mut input = legacy(script);
        assert_eq!(next_event(&mut input).await, Event::Paste("ab".into()));
        // The Enter arrives after the app went idle, well past the burst gap.
        input.last_burst = Instant::now().checked_sub(Duration::from_millis(250));
        assert!(matches!(
            next_event(&mut input).await,
            Event::Key(k) if k.code == KeyCode::Enter
        ));
    }

    #[tokio::test]
    async fn an_oversized_burst_is_rejected_whole_not_truncated() {
        let mut script = chars("abcdefgh");
        script.push(None);
        script.extend(chars("tail"));
        script.push(None);
        script.push(Some(Ok(Event::FocusGained)));
        script.extend(chars("typed"));
        let mut input = legacy(script);
        input.cap = 4;
        let Event::Paste(text) = next_event(&mut input).await else {
            panic!("expected the over-cap burst");
        };
        assert!(text.len() > 4, "over-cap, so the app rejects it with a notice");
        assert!(text.len() < 8, "the rest of the burst is drained, not kept");
        // The chunk continuing the oversized paste is dropped, not inserted;
        // the next unrelated event ends the discard.
        assert_eq!(next_event(&mut input).await, Event::FocusGained);
        assert_eq!(next_event(&mut input).await, Event::Paste("typed".into()));
    }

    #[tokio::test]
    async fn without_coalescing_every_key_passes_through() {
        let mut input = LegacyInput::new(Scripted(chars("ab").into()), false);
        assert!(matches!(
            input.next().await,
            Some(Ok(Event::Key(k))) if k.code == KeyCode::Char('a')
        ));
    }
}
