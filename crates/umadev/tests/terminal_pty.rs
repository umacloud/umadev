//! Cross-platform real-process terminal smoke test.
//!
//! Recording-backend tests prove UmaDev's byte contract, but they cannot prove
//! that the complete binary recognizes an OS terminal, enters its event loop,
//! accepts input, and restores the terminal on exit. `portable-pty` maps this
//! test to Unix PTYs on Linux/macOS and ConPTY on Windows.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tempfile::TempDir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_umadev"))
}

#[test]
fn tui_handles_resize_multiline_cjk_paste_and_quit_through_native_pty() {
    let sandbox = TempDir::new().expect("temporary terminal sandbox");
    let home = sandbox.path().join("home");
    let config_dir = home.join(".umadev");
    std::fs::create_dir_all(&config_dir).expect("create isolated UmaDev config");
    std::fs::write(
        config_dir.join("config.toml"),
        "backend = \"offline\"\nlang = \"en\"\n",
    )
    .expect("write isolated offline config");
    let empty_model = home.join("empty-embed-model");
    std::fs::create_dir_all(&empty_model).expect("create empty embedding model dir");

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open native pseudo-terminal");

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("clone pseudo-terminal reader");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_capture = Arc::clone(&captured);
    let reader_thread = thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        while let Ok(count) = reader.read(&mut chunk) {
            if count == 0 {
                break;
            }
            reader_capture
                .lock()
                .expect("lock terminal capture")
                .extend_from_slice(&chunk[..count]);
        }
    });

    let mut command = CommandBuilder::new(bin());
    command.cwd(sandbox.path());
    command.env("HOME", &home);
    command.env("USERPROFILE", &home);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("UMADEV_EMBED_MODEL_DIR", &empty_model);
    command.env_remove("XDG_CONFIG_HOME");
    for secret in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "OPENAI_EMBED_KEY",
        "XAI_API_KEY",
        "UMADEV_ALLOW_CLOUD_EMBED",
        "OPENAI_EMBED_BASE",
    ] {
        command.env_remove(secret);
    }

    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn UmaDev in native pseudo-terminal");
    drop(pair.slave);

    let mut writer = pair
        .master
        .take_writer()
        .expect("take pseudo-terminal writer");

    // Wait for a rendered frame rather than racing startup on slower Windows CI,
    // then prove that the full input decoder and slash-command route can quit it.
    let render_deadline = Instant::now() + Duration::from_secs(15);
    let mut cursor_position_answered = false;
    loop {
        let (rendered, requested_cursor_position) = {
            let bytes = captured.lock().expect("lock terminal capture");
            (
                bytes
                    .windows(b"UmaDev".len())
                    .any(|window| window == b"UmaDev"),
                bytes.windows(4).any(|window| window == b"\x1b[6n"),
            )
        };
        if requested_cursor_position && !cursor_position_answered {
            // A real terminal emulator answers DSR cursor-position queries. ConPTY
            // transports the bytes but portable-pty does not emulate that reply.
            writer
                .write_all(b"\x1b[1;1R")
                .expect("answer terminal cursor-position query");
            writer
                .flush()
                .expect("flush terminal cursor-position response");
            cursor_position_answered = true;
        }
        if rendered {
            break;
        }
        if Instant::now() >= render_deadline {
            let _ = child.kill();
            let _ = child.wait();
            drop(writer);
            drop(pair.master);
            reader_thread.join().expect("join terminal reader");
            let bytes = captured.lock().expect("lock terminal capture");
            panic!(
                "UmaDev did not render through the native pseudo-terminal: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }

    let bytes_before_resize = captured.lock().expect("lock terminal capture").len();
    pair.master
        .resize(PtySize {
            rows: 18,
            cols: 64,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("shrink native pseudo-terminal");
    pair.master
        .resize(PtySize {
            rows: 40,
            cols: 132,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("expand native pseudo-terminal");

    let repaint_deadline = Instant::now() + Duration::from_secs(5);
    while captured.lock().expect("lock terminal capture").len() <= bytes_before_resize {
        assert!(
            Instant::now() < repaint_deadline,
            "UmaDev did not repaint after a native pseudo-terminal resize"
        );
        thread::sleep(Duration::from_millis(25));
    }

    // Put `/quit` on the first line deliberately. If a terminal or decoder
    // mistakes the embedded pasted newline for a submit key, the process exits
    // here. A correct bracketed paste stays one atomic edit, preserves CJK and
    // emoji, and leaves the session alive. This runs through a Unix PTY on
    // Linux/macOS and ConPTY on Windows rather than calling App methods directly.
    const PASTE_PROBE: &str = "/quit\n你好 UmaDev 🙂 multiline-paste-probe";
    writer
        .write_all(format!("\u{1b}[200~{PASTE_PROBE}\u{1b}[201~").as_bytes())
        .expect("send bracketed multiline CJK paste");
    writer.flush().expect("flush bracketed multiline CJK paste");

    let paste_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rendered = captured
            .lock()
            .expect("lock terminal capture")
            .as_slice()
            .windows("你".len())
            .any(|window| window == "你".as_bytes());
        let rendered_second_cjk = captured
            .lock()
            .expect("lock terminal capture")
            .as_slice()
            .windows("好".len())
            .any(|window| window == "好".as_bytes());
        let rendered_emoji = captured
            .lock()
            .expect("lock terminal capture")
            .as_slice()
            .windows("🙂".len())
            .any(|window| window == "🙂".as_bytes());
        // Ratatui may cursor-reanchor between adjacent wide cells, so each
        // Unicode scalar is searched independently rather than requiring the
        // UTF-8 payload to remain contiguous in the terminal byte stream.
        if rendered && rendered_second_cjk && rendered_emoji {
            break;
        }
        if Instant::now() >= paste_deadline {
            let bytes = captured.lock().expect("lock terminal capture").clone();
            panic!(
                "UmaDev did not render a bracketed CJK paste through the native pseudo-terminal: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        child
            .try_wait()
            .expect("poll after embedded pasted newline")
            .is_none(),
        "an embedded newline in bracketed paste submitted /quit instead of remaining atomic"
    );

    // Clear the entire pasted draft before sending the intentional command.
    // Idle Ctrl+C is UmaDev's cross-platform whole-input clear gesture; Ctrl+U
    // only clears the current line and leaves earlier bracketed-paste lines.
    writer.write_all(&[0x03]).expect("clear pasted draft");
    writer.flush().expect("flush whole-draft clear key");

    let command_capture_start = captured.lock().expect("lock terminal capture").len();
    const SUBMIT_SYNC: &str = "@";
    writer
        .write_all(format!("/quit {SUBMIT_SYNC}").as_bytes())
        .expect("type synchronized /quit command");
    writer.flush().expect("flush synchronized /quit text");
    let command_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rendered = {
            let bytes = captured.lock().expect("lock terminal capture");
            bytes[command_capture_start..]
                .windows(SUBMIT_SYNC.len())
                .any(|window| window == SUBMIT_SYNC.as_bytes())
        };
        if rendered {
            break;
        }
        if Instant::now() >= command_deadline {
            let bytes = captured.lock().expect("lock terminal capture").clone();
            let raw = String::from_utf8_lossy(&bytes[command_capture_start..]);
            let visible = umadev_agent::base_error::strip_ansi(&raw);
            panic!(
                "UmaDev did not render the synchronized /quit command; visible terminal tail: {visible}"
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
    // Windows intentionally treats a sub-30ms Enter as part of a raw console
    // paste burst so an embedded pasted newline cannot submit a partial prompt.
    // Model a real, distinct submit keypress outside that documented window.
    thread::sleep(Duration::from_millis(75));
    // ConPTY's UTF-8 pipe needs the Windows CRLF line ending once the input
    // frame is fully rendered; Unix PTYs represent Enter as a lone CR.
    let submit: &[u8] = if cfg!(windows) { b"\r\n" } else { b"\r" };
    writer.write_all(submit).expect("press Enter after /quit");
    writer.flush().expect("flush /quit submit key");

    let deadline = Instant::now() + Duration::from_secs(15);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().expect("poll UmaDev child") {
            break (status, false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break (child.wait().expect("reap timed-out UmaDev child"), true);
        }
        thread::sleep(Duration::from_millis(50));
    };

    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join terminal reader");
    let output_bytes = captured.lock().expect("lock terminal capture");
    let output = String::from_utf8_lossy(&output_bytes);

    assert!(!timed_out, "UmaDev did not accept /quit: {output}");
    assert!(status.success(), "UmaDev exited unsuccessfully: {status}");
    assert!(
        output.contains("UmaDev"),
        "the real TUI never rendered its identity; captured {} bytes",
        output.len()
    );
    assert!(
        !output.contains("panicked at") && !output.contains("thread 'main' panicked"),
        "the real TUI panicked under the native pseudo-terminal: {output}"
    );
}
