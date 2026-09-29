//! Pastes and mouse input while a modal surface covers the transcript.
//!
//! `App::chat_key` routes KEYS to the Ctrl+F / Ctrl+R search bars, help, an
//! overlay, the first-run picker and the prompt-queue pane before the
//! composer, but a paste and a mouse event never pass through it. A bracketed
//! paste, a multi-character IME commit on the owned input path, or a coalesced
//! Windows key burst therefore landed in the hidden composer behind the modal,
//! and with help open a drag selected (and copied) the hidden transcript rows,
//! Ctrl+click opened their links, and the wheel scrolled them instead of help.

use super::{App, AppMode};

impl App {
    /// Give a paste to the modal surface that owns the keyboard. A search bar
    /// takes it as query text (one line: line breaks and tabs become spaces,
    /// terminal escapes and other control characters are dropped) and rescans
    /// once; help, an overlay, the first-run picker and the prompt-queue pane
    /// swallow it the way they swallow typed text. Returns `true` when the
    /// paste was consumed and must not reach the composer.
    pub(crate) fn paste_into_modal(&mut self, pasted: &str) -> bool {
        if self.search.is_some() {
            self.search_input_str(&query_paste_text(pasted));
            return true;
        }
        if self.history_search.is_some() {
            self.history_search_input_str(&query_paste_text(pasted));
            return true;
        }
        self.show_help
            || self.overlay.is_some()
            || matches!(self.mode, AppMode::Picker)
            || self.prompt_queue.is_open()
    }

    /// Append pasted text to the transcript-search query, rescan ONCE, and jump
    /// to the first match (typing rescans per char).
    fn search_input_str(&mut self, text: &str) {
        if let Some(s) = self.search.as_mut() {
            s.query.push_str(text);
            s.current = 0;
        }
        self.recompute_search_matches();
        self.search_focus_current();
    }

    /// Append pasted text to the prompt-history search query and rescan once.
    fn history_search_input_str(&mut self, text: &str) {
        if let Some(st) = self.history_search.as_mut() {
            st.query.push_str(text);
            st.current = 0;
            Self::recompute_history_matches(st);
        }
    }

    /// Whether mouse selection, Ctrl+click links and the scrollbar act on the
    /// transcript: wheel capture is on (`/mouse`), the chat screen is up, and no
    /// overlay or help covers the transcript rows.
    pub(crate) fn transcript_mouse_enabled(&self) -> bool {
        self.mouse_scroll
            && self.overlay.is_none()
            && !self.show_help
            && matches!(self.mode, AppMode::Chat)
    }

    /// Route one mouse-wheel notch (`up` = `ScrollUp`) to the right surface.
    ///
    /// Precedence: a modal **overlay**, when open, owns the viewport and scrolls
    /// regardless of the `/mouse` wheel-capture toggle — it is content the user is
    /// actively reading, so the wheel must move IT, not the transcript hidden
    /// behind it (the reported "overlay won't scroll" was the wheel scrolling that
    /// hidden transcript). Help is the same kind of reading surface and scrolls
    /// next. With neither open, the wheel scrolls the chat transcript, but only
    /// when wheel-capture is enabled (`/mouse`) and we're on the chat screen,
    /// matching the existing chat-mode gating. Returns `true` if the notch was
    /// consumed. Fail-open: an out-of-range notch is clamped by the underlying
    /// scroll helpers, never panics.
    pub fn mouse_wheel(&mut self, up: bool, step: usize) -> bool {
        if let Some(ov) = self.overlay.as_mut() {
            if up {
                ov.scroll_up(step);
            } else {
                ov.scroll_down(step);
            }
            return true;
        }
        if self.show_help {
            let rows = u16::try_from(step).unwrap_or(u16::MAX);
            if up {
                self.help_scroll_up(rows);
            } else {
                self.help_scroll_down(rows);
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
}

/// Flatten a paste into single-line query text: the composer's paste cleanup
/// (CR / CRLF → LF, ANSI sequences removed), then every line break and tab
/// becomes a space and any other control character is dropped. A trailing line
/// break (a copied whole line) adds nothing.
fn query_paste_text(pasted: &str) -> String {
    App::normalize_paste_text(pasted)
        .lines()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::query_paste_text;
    use crate::app::App;
    use crate::config::UserConfig;

    #[test]
    fn query_paste_text_is_one_clean_line() {
        assert_eq!(query_paste_text("错误"), "错误");
        assert_eq!(query_paste_text("line one\r\nline two\n"), "line one line two");
        assert_eq!(query_paste_text("a\rb\tc"), "a b c");
        assert_eq!(query_paste_text("\x1b[31mred\x1b[0m\x07!"), "red!");
    }

    #[test]
    fn with_help_open_the_mouse_scrolls_help_and_never_touches_the_transcript() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "help-mouse",
            UserConfig {
                backend: Some("offline".to_string()),
                ..UserConfig::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        app.mouse_scroll = true;
        assert!(app.transcript_mouse_enabled());
        app.transcript_max_scroll.set(40);
        app.help_max_scroll.set(30);
        app.show_help = true;
        // Selection, links and the scrollbar are off while help covers the rows.
        assert!(!app.transcript_mouse_enabled());
        app.transcript_scrollbar_area.set((79, 0, 1, 20));
        assert!(!app.transcript_scrollbar_begin(79, 5));
        // The wheel scrolls help, not the hidden transcript.
        assert!(app.mouse_wheel(false, 3));
        assert_eq!(app.help_scroll, 3);
        assert_eq!(app.transcript_scroll.get(), 0);
        assert!(app.mouse_wheel(true, 1));
        assert_eq!(app.help_scroll, 2);
        // Closing help hands the mouse back to the transcript.
        app.show_help = false;
        assert!(app.transcript_mouse_enabled());
        assert!(app.mouse_wheel(true, 3));
        assert_eq!(app.transcript_scroll.get(), 3);
    }
}
