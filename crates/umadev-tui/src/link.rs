//! Ctrl+click link opening over the transcript.
//!
//! With mouse capture ON (the default) the terminal's own Cmd/Ctrl+click URL
//! opening never fires inside the app, so we provide it ourselves: a left
//! click with the CTRL modifier held hit-tests the clicked cell against the
//! same cached transcript rows the selection layer uses, recovers the token
//! under the cursor, and — when it is an `http(s)://` URL or a file path that
//! actually exists — opens it with the platform opener (`open` on macOS,
//! `cmd /C start` on Windows, `xdg-open` elsewhere), spawned detached with
//! all stdio null so the event loop never blocks.
//!
//! CTRL is the trigger (SGR mouse reports carry it; Windows Terminal and most
//! unix terminals deliver it). macOS terminals usually intercept Cmd+click
//! themselves — iTerm2's native Cmd+click keeps working unchanged because it
//! never reaches the app.
//!
//! Security posture (UD-SEC-aligned, fail-open): only `http`/`https` URLs are
//! ever opened (no `file:` / `javascript:` / other schemes), paths must
//! canonicalize to an existing file/dir, targets with embedded quotes,
//! backticks or control characters are rejected, and the opener is always an
//! argv vector — never a shell-interpolated string. A miss or a failed spawn
//! is a silent no-op / one status note; nothing here can block the UI.

use std::path::{Path, PathBuf};

/// What a Ctrl+click resolved to: a web URL or a file-path token (still
/// unresolved — the caller checks existence via [`resolve_path`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkCandidate {
    /// A validated `http://` / `https://` URL, boundary-trimmed.
    Url(String),
    /// A path-shaped token (absolute, `~/`, drive-letter, or relative); the
    /// caller must resolve + existence-check it before opening.
    Path(String),
}

/// Rejoin the soft-wrapped LOGICAL line containing visual `row`, and map the
/// clicked `(row, col)` (a char index within that visual row, as produced by
/// `selection::screen_to_content`) to a char offset within the joined line.
///
/// Walks back over `wraps` continuations to the line start and forward to its
/// end, concatenating in lockstep with `selection::extract_wrapped` — so a URL
/// the renderer folded across two visual rows is seen whole. Fail-open: an
/// out-of-range row returns `None`; a short/empty `wraps` degrades every
/// boundary to a real line break (single-row line).
#[must_use]
pub fn logical_line_at(
    rows: &[String],
    wraps: &[bool],
    row: usize,
    col: usize,
) -> Option<(String, usize)> {
    if row >= rows.len() {
        return None;
    }
    let is_continuation = |r: usize| wraps.get(r).copied().unwrap_or(false);
    let mut start = row;
    while start > 0 && is_continuation(start) {
        start -= 1;
    }
    let mut end = row;
    while end + 1 < rows.len() && is_continuation(end + 1) {
        end += 1;
    }
    let mut line = String::new();
    let mut offset = 0usize;
    for (r, text) in rows.iter().enumerate().take(end + 1).skip(start) {
        if r == row {
            offset = line.chars().count() + col.min(text.chars().count());
        }
        line.push_str(text);
    }
    Some((line, offset))
}

/// `true` for characters that may appear inside a URL token. ASCII-graphic
/// only (a CJK glyph or a space terminates the URL), excluding quotes,
/// backticks, angle brackets and backslashes — the characters that both never
/// belong in a sane URL and are the classic smuggling vectors.
fn is_url_char(c: char) -> bool {
    c.is_ascii_graphic() && !matches!(c, '"' | '\'' | '`' | '<' | '>' | '\\')
}

/// Case-insensitive ASCII prefix match at char position `i`.
fn starts_with_ci(chars: &[char], i: usize, pat: &str) -> bool {
    let pat: Vec<char> = pat.chars().collect();
    chars.len() >= i + pat.len()
        && chars[i..i + pat.len()]
            .iter()
            .zip(&pat)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Trim the raw URL span `[start, end)` back over trailing sentence
/// punctuation and UNBALANCED closing brackets — `http://a.b/c).` sheds the
/// `).` but a Wikipedia-style `.../Rust_(language)` keeps its balanced `)`.
fn trim_url_end(chars: &[char], start: usize, mut end: usize) -> usize {
    while end > start {
        match chars[end - 1] {
            '.' | ',' | ';' | ':' | '!' | '?' => end -= 1,
            c @ (')' | ']' | '}') => {
                let open = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                let opens = chars[start..end].iter().filter(|&&x| x == open).count();
                let closes = chars[start..end].iter().filter(|&&x| x == c).count();
                if closes > opens {
                    end -= 1;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    end
}

/// Find the `http://` / `https://` URL under char offset `col` of `line`, if
/// any. The clicked position must fall INSIDE the token (a click in the blank
/// area past the text never opens anything); the returned URL is trimmed of
/// trailing punctuation / unbalanced brackets and validated by
/// [`is_safe_url`]. Pure + fail-open: no match returns `None`.
#[must_use]
pub fn url_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let at_scheme = (starts_with_ci(&chars, i, "http://")
            || starts_with_ci(&chars, i, "https://"))
            // Boundary guard: `xhttp://` is not a URL start, but a CJK glyph
            // or punctuation right before the scheme is fine.
            && (i == 0 || !chars[i - 1].is_ascii_alphanumeric());
        if !at_scheme {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        while end < chars.len() && is_url_char(chars[end]) {
            end += 1;
        }
        // The click must land inside the RAW span (trailing punctuation
        // included — forgiving), but the OPENED url is the trimmed token.
        if col >= start && col < end {
            let trimmed = trim_url_end(&chars, start, end);
            let url: String = chars[start..trimmed].iter().collect();
            return is_safe_url(&url).then_some(url);
        }
        i = end.max(i + 1);
    }
    None
}

/// `true` only for an openable, non-smuggling web URL: the scheme is `http`
/// or `https` (anything else — `ftp:`, `file:`, `javascript:` — is rejected
/// by construction), the host part is non-empty, and every character is
/// ASCII-graphic with no quotes / backticks / angle brackets / backslashes
/// (control chars and whitespace are excluded by the same check). The URL is
/// only ever passed as ONE argv element, never through a shell string.
#[must_use]
pub fn is_safe_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
    else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('/') {
        return false;
    }
    url.chars().all(is_url_char)
}

/// `true` for characters that may appear inside a file-path token. Unicode
/// alphanumerics are allowed (CJK file names are real), plus the separator /
/// extension / drive punctuation. Quotes, backticks, brackets and whitespace
/// all terminate the token.
fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '/' | '\\' | '.' | '-' | '_' | '~' | ':' | '+' | '@')
}

/// Does `tok` LOOK like a file path worth an existence check? Absolute unix
/// (`/…`), home (`~/…`), Windows drive (`X:\…` / `X:/…`), explicit relative
/// (`./…` / `../…`), anything with a separator, or a bare file name with an
/// interior extension dot (`Cargo.toml`). URL-shaped tokens (`…://…`) are
/// never paths.
fn looks_like_path(tok: &str) -> bool {
    if tok.contains("://") {
        return false;
    }
    if tok.starts_with('/')
        || tok.starts_with("~/")
        || tok.starts_with("./")
        || tok.starts_with("../")
    {
        return true;
    }
    let chars: Vec<char> = tok.chars().collect();
    if chars.len() >= 3
        && chars[0].is_ascii_alphabetic()
        && chars[1] == ':'
        && (chars[2] == '/' || chars[2] == '\\')
    {
        return true;
    }
    if tok.contains('/') || tok.contains('\\') {
        return true;
    }
    // Bare file name: an interior dot (not leading, not trailing) reads as an
    // extension — `shot.png`, `Cargo.toml`. A lone `.` / `..` does not.
    tok.rfind('.').is_some_and(|p| p > 0 && p + 1 < tok.len())
}

/// Extract the path-shaped token under char offset `col` of `line`, if any.
/// Expands over [`is_path_char`] in both directions, sheds trailing sentence
/// punctuation, and keeps only tokens that pass [`looks_like_path`] and carry
/// at least one alphanumeric char (a lone `/` or `..` is not a click target).
/// The result is a CANDIDATE — [`resolve_path`] decides whether it exists.
#[must_use]
pub fn path_candidate_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    if col >= chars.len() || !is_path_char(chars[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && is_path_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < chars.len() && is_path_char(chars[end]) {
        end += 1;
    }
    // Trailing sentence punctuation is decoration, not path: `see a/b.png,`.
    while end > start && matches!(chars[end - 1], '.' | ',' | ';' | ':' | '!' | '?') {
        end -= 1;
    }
    if end <= start {
        return None;
    }
    let tok: String = chars[start..end].iter().collect();
    if !tok.chars().any(char::is_alphanumeric) {
        return None;
    }
    looks_like_path(&tok).then_some(tok)
}

/// Find the openable candidate under char offset `col` of `line`: URL first
/// (the stronger signal), then a path-shaped token. Pure; the path branch is
/// only a candidate until [`resolve_path`] confirms it exists.
#[must_use]
pub fn find_link(line: &str, col: usize) -> Option<LinkCandidate> {
    if let Some(url) = url_at(line, col) {
        return Some(LinkCandidate::Url(url));
    }
    path_candidate_at(line, col).map(LinkCandidate::Path)
}

/// Strip a trailing `:line(:col)` location suffix — compiler / grep output
/// style (`src/app.rs:120:5` → `src/app.rs`). Leaves anything else (including
/// a Windows drive prefix) untouched.
#[must_use]
pub fn strip_line_suffix(tok: &str) -> &str {
    let mut s = tok;
    while let Some(idx) = s.rfind(':') {
        let digits = &s[idx + 1..];
        if idx > 0 && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            s = &s[..idx];
        } else {
            break;
        }
    }
    s
}

/// `true` when `tok` carries a character we refuse to hand to an opener:
/// control characters (incl. escape sequences) or quote/backtick smuggling.
fn has_forbidden_chars(tok: &str) -> bool {
    tok.chars()
        .any(|c| c.is_control() || matches!(c, '"' | '\'' | '`'))
}

/// The user's home directory, for `~/` expansion. `HOME` on unix,
/// `USERPROFILE` on Windows; `None` when neither is set (fail-open: the
/// candidate is simply not openable).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Strip the Windows verbatim prefix (`\\?\C:\x` → `C:\x`) that
/// `canonicalize` adds — `cmd /C start` and Explorer choke on it. Pure string
/// logic; a non-verbatim path (every unix path) passes through unchanged.
fn undecorate(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.starts_with("UNC") {
            return PathBuf::from(rest);
        }
    }
    p
}

/// Resolve a path candidate to an EXISTING filesystem entry, or `None`.
///
/// Rejects control chars / quotes, expands `~/` to the home dir, anchors a
/// relative token at `workspace_root`, and requires `canonicalize()` to
/// succeed (the existence + normalization proof). A `file.rs:12:5` location
/// suffix is retried without the suffix. Fail-open: any miss is `None`,
/// nothing is created or touched.
#[must_use]
pub fn resolve_path(token: &str, workspace_root: &Path) -> Option<PathBuf> {
    if has_forbidden_chars(token) {
        return None;
    }
    let stripped = strip_line_suffix(token);
    let attempts: &[&str] = if stripped == token {
        &[token]
    } else {
        &[token, stripped]
    };
    for &tok in attempts {
        let expanded: PathBuf = if let Some(rest) = tok.strip_prefix("~/") {
            match home_dir() {
                Some(h) => h.join(rest),
                None => continue,
            }
        } else if Path::new(tok).is_absolute() {
            PathBuf::from(tok)
        } else {
            workspace_root.join(tok)
        };
        if let Ok(canon) = expanded.canonicalize() {
            return Some(undecorate(canon));
        }
    }
    None
}

/// The platform-opener argv for `target` — `(program, args)`, argv-vector
/// only (never a shell string): `open <t>` on macOS, `explorer <t>` on
/// Windows, `xdg-open <t>` elsewhere. Parameterized on `os`
/// (`std::env::consts::OS`) so all three shapes are unit-testable on any
/// host.
///
/// Windows deliberately avoids `cmd /C start`: cmd re-parses its command
/// line, and a legal URL character like `&` (ubiquitous in query strings)
/// arrives unquoted through the std argv builder — cmd would treat it as a
/// command separator, so a crafted "URL" in the transcript could execute a
/// command on click. `explorer.exe` takes the whole argument literally and
/// hands it to the shell-open verb: URLs go to the default browser, paths to
/// the default app/folder — the same end result as `start`, with no shell in
/// between.
#[must_use]
pub fn opener_argv(os: &str, target: &str) -> (String, Vec<String>) {
    match os {
        "macos" => ("open".to_string(), vec![target.to_string()]),
        "windows" => ("explorer".to_string(), vec![target.to_string()]),
        _ => ("xdg-open".to_string(), vec![target.to_string()]),
    }
}

/// Spawn the platform opener for `target`, DETACHED: all stdio null (an
/// opener writing to our TTY would corrupt the frame), no wait on the UI
/// path. The launcher (`open` / `xdg-open` / `explorer`) hands off to the
/// browser / Finder / Explorer and exits within milliseconds; a detached
/// thread reaps it so no zombie accumulates (the same pattern as the
/// `/preview` browser open). Fail-open: the only error surfaced is a failed
/// `spawn`, which the caller turns into one status note.
pub fn spawn_opener(target: &str) -> std::io::Result<()> {
    let (prog, args) = opener_argv(std::env::consts::OS, target);
    let child = std::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── logical-line rejoin ───────────────────────────────────────────────
    #[test]
    fn logical_line_rejoins_soft_wrapped_rows_and_maps_the_offset() {
        // One logical line "see http://a.b/long" folded across three rows.
        let rows = vec![
            "see http://".to_string(),
            "a.b/".to_string(),
            "long".to_string(),
        ];
        let wraps = vec![false, true, true];
        // A click on row 1, col 2 ('b') maps to offset 11 + 2 = 13.
        let (line, off) = logical_line_at(&rows, &wraps, 1, 2).unwrap();
        assert_eq!(line, "see http://a.b/long");
        assert_eq!(off, 13);
        // The rejoined line resolves the WHOLE wrapped URL.
        assert_eq!(url_at(&line, off), Some("http://a.b/long".to_string()));
    }

    #[test]
    fn logical_line_without_wrap_flags_is_the_single_row() {
        let rows = vec!["alpha".to_string(), "beta".to_string()];
        let (line, off) = logical_line_at(&rows, &[], 1, 2).unwrap();
        assert_eq!(line, "beta");
        assert_eq!(off, 2);
        // Out-of-range row fails open.
        assert_eq!(logical_line_at(&rows, &[], 9, 0), None);
    }

    // ── URL extraction ────────────────────────────────────────────────────
    #[test]
    fn url_under_the_click_is_extracted() {
        let line = "preview at http://127.0.0.1:4173/ now";
        // Click on the '1' of 127 (offset 18).
        assert_eq!(url_at(line, 18), Some("http://127.0.0.1:4173/".to_string()));
        // Click on the scheme itself.
        assert_eq!(url_at(line, 11), Some("http://127.0.0.1:4173/".to_string()));
        // Click OUTSIDE the URL (the word "now") is a miss.
        assert_eq!(url_at(line, 35), None);
        // Click before it (the word "preview") is a miss too.
        assert_eq!(url_at(line, 2), None);
    }

    #[test]
    fn url_trailing_punctuation_is_trimmed() {
        assert_eq!(
            url_at("see https://example.com/a.", 10),
            Some("https://example.com/a".to_string())
        );
        assert_eq!(
            url_at("(docs: https://example.com/x).", 12),
            Some("https://example.com/x".to_string())
        );
        // Balanced parens survive (the Wikipedia case).
        assert_eq!(
            url_at("https://en.wikipedia.org/wiki/Rust_(language)", 5),
            Some("https://en.wikipedia.org/wiki/Rust_(language)".to_string())
        );
        // A trailing colon after the URL is shed, but a port colon is kept.
        assert_eq!(
            url_at("open http://x.dev:8080:", 8),
            Some("http://x.dev:8080".to_string())
        );
    }

    #[test]
    fn url_terminates_at_cjk_and_detects_after_cjk() {
        // CJK right AFTER the URL terminates the token (non-ASCII).
        let line = "访问 http://x.dev/页面";
        assert_eq!(url_at(line, 5), Some("http://x.dev/".to_string()));
        // CJK right BEFORE the scheme (no space) still detects.
        let glued = "地址http://x.dev/a完成";
        let idx = glued.chars().position(|c| c == 'h').unwrap();
        assert_eq!(url_at(glued, idx), Some("http://x.dev/a".to_string()));
    }

    #[test]
    fn url_boundary_guard_rejects_glued_ascii() {
        // `xhttp://…` is not a URL start.
        assert_eq!(url_at("xhttp://evil.example/", 3), None);
    }

    // ── http(s)-only + smuggling guard ────────────────────────────────────
    #[test]
    fn only_http_and_https_schemes_are_safe() {
        assert!(is_safe_url("http://example.com"));
        assert!(is_safe_url("https://example.com/a?b=c#d"));
        assert!(is_safe_url("HTTPS://EXAMPLE.COM"));
        assert!(!is_safe_url("ftp://example.com"));
        assert!(!is_safe_url("file:///etc/passwd"));
        assert!(!is_safe_url("javascript:alert(1)"));
        assert!(!is_safe_url("http://"));
        assert!(!is_safe_url("https:///nohost"));
    }

    #[test]
    fn urls_with_quotes_or_control_chars_are_rejected() {
        assert!(!is_safe_url("http://x.dev/\"quote"));
        assert!(!is_safe_url("http://x.dev/'q"));
        assert!(!is_safe_url("http://x.dev/`tick"));
        assert!(!is_safe_url("http://x.dev/\x1b[31m"));
        assert!(!is_safe_url("http://x.dev/a b"));
        assert!(!is_safe_url("http://x.dev/\\back"));
    }

    // ── path candidates ───────────────────────────────────────────────────
    #[test]
    fn path_shapes_are_recognized() {
        let line = "shot saved to /tmp/shots/final.png ok";
        assert_eq!(
            path_candidate_at(line, 20),
            Some("/tmp/shots/final.png".to_string())
        );
        // Home, relative-with-separator, drive-letter, bare-extension shapes.
        assert_eq!(
            path_candidate_at("open ~/proj/a.rs now", 6),
            Some("~/proj/a.rs".to_string())
        );
        assert_eq!(
            path_candidate_at("see src/app.rs here", 5),
            Some("src/app.rs".to_string())
        );
        assert_eq!(
            path_candidate_at(r"at C:\Users\x\shot.png end", 4),
            Some(r"C:\Users\x\shot.png".to_string())
        );
        assert_eq!(
            path_candidate_at("edit Cargo.toml please", 6),
            Some("Cargo.toml".to_string())
        );
        // Trailing sentence punctuation is shed.
        assert_eq!(
            path_candidate_at("wrote /tmp/a.png.", 8),
            Some("/tmp/a.png".to_string())
        );
    }

    #[test]
    fn non_path_words_are_not_candidates() {
        // A plain word has no path shape.
        assert_eq!(path_candidate_at("just a word here", 8), None);
        // A URL is never a path candidate (the URL branch owns it).
        assert_eq!(path_candidate_at("http://x.dev/a", 3), None);
        // Whitespace under the cursor is a miss.
        assert_eq!(path_candidate_at("a b", 1), None);
        // Punctuation-only tokens ("..", "/") are not click targets.
        assert_eq!(path_candidate_at("see .. up", 4), None);
        // A click past the end of the line is a miss.
        assert_eq!(path_candidate_at("/tmp/x.png", 99), None);
    }

    #[test]
    fn strip_line_suffix_removes_locations_only() {
        assert_eq!(strip_line_suffix("src/app.rs:120"), "src/app.rs");
        assert_eq!(strip_line_suffix("src/app.rs:120:5"), "src/app.rs");
        assert_eq!(strip_line_suffix("src/app.rs"), "src/app.rs");
        // A drive prefix is not a location suffix.
        assert_eq!(strip_line_suffix(r"C:\x\y.png"), r"C:\x\y.png");
        // Port-looking hosts are left to the URL branch; a bare host:port
        // token does lose the port here, but it never reaches resolve_path
        // as a URL (the `://` guard filters it earlier).
        assert_eq!(strip_line_suffix("file.rs:"), "file.rs:");
    }

    // ── path resolution (existence-gated) ─────────────────────────────────
    #[test]
    fn resolve_path_accepts_existing_and_rejects_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("proof.png");
        std::fs::write(&file, b"x").unwrap();
        // Absolute existing file resolves.
        let abs = file.display().to_string();
        assert!(resolve_path(&abs, dir.path()).is_some());
        // Relative token anchored at the workspace resolves.
        assert!(resolve_path("proof.png", dir.path()).is_some());
        // A `file:line:col` suffix is retried without the location.
        let with_loc = format!("{abs}:12:5");
        assert!(resolve_path(&with_loc, dir.path()).is_some());
        // Non-existent rejects (no creation, no guessing).
        assert_eq!(resolve_path("missing.png", dir.path()), None);
        assert_eq!(resolve_path("/no/such/dir/x.png", dir.path()), None);
    }

    #[test]
    fn resolve_path_rejects_control_chars_and_quotes() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_path("a\x07b.png", dir.path()), None);
        assert_eq!(resolve_path("a\"b.png", dir.path()), None);
        assert_eq!(resolve_path("a'b.png", dir.path()), None);
        assert_eq!(resolve_path("a`b.png", dir.path()), None);
    }

    #[test]
    fn undecorate_strips_the_windows_verbatim_prefix_only() {
        assert_eq!(
            undecorate(PathBuf::from(r"\\?\C:\x\y.png")),
            PathBuf::from(r"C:\x\y.png")
        );
        // UNC verbatim and plain paths pass through unchanged.
        assert_eq!(
            undecorate(PathBuf::from(r"\\?\UNC\srv\share")),
            PathBuf::from(r"\\?\UNC\srv\share")
        );
        assert_eq!(
            undecorate(PathBuf::from("/tmp/x.png")),
            PathBuf::from("/tmp/x.png")
        );
    }

    // ── opener argv (per-platform, argv-vector only) ──────────────────────
    #[test]
    fn opener_argv_matches_each_platform() {
        let url = "http://127.0.0.1:4173/";
        assert_eq!(
            opener_argv("macos", url),
            ("open".to_string(), vec![url.to_string()])
        );
        assert_eq!(
            opener_argv("linux", url),
            ("xdg-open".to_string(), vec![url.to_string()])
        );
        // Windows: `explorer <target>` — ONE argv element, NO cmd anywhere.
        // cmd re-parses its command line, so a legal `&` in a query string
        // would become a command separator (a crafted "URL" could execute a
        // command on click); explorer takes the argument literally.
        assert_eq!(
            opener_argv("windows", url),
            ("explorer".to_string(), vec![url.to_string()])
        );
        // The injection shape stays one literal argv element on Windows.
        let ambush = "http://x.dev/?a=1&calc";
        assert_eq!(
            opener_argv("windows", ambush),
            ("explorer".to_string(), vec![ambush.to_string()])
        );
    }

    // ── combined dispatch ─────────────────────────────────────────────────
    #[test]
    fn find_link_prefers_url_then_path() {
        assert_eq!(
            find_link("see http://x.dev/a", 6),
            Some(LinkCandidate::Url("http://x.dev/a".to_string()))
        );
        assert_eq!(
            find_link("see /tmp/a.png", 6),
            Some(LinkCandidate::Path("/tmp/a.png".to_string()))
        );
        assert_eq!(find_link("plain words only", 3), None);
    }
}
