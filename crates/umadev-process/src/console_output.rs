//! Text a console program wrote into a pipe.
//!
//! On Windows, `cmd.exe` built-ins and many native tools write redirected
//! output in the console's code page (GBK on Simplified-Chinese systems, Big5
//! on Traditional), while Git, Node and most cross-platform tools write UTF-8,
//! and one shell command can mix both. Each line is therefore read as UTF-8
//! when it is valid UTF-8, and otherwise in the console's output code page.

/// Decode captured console output line by line (see the module docs).
///
/// Outside Windows, and for a line the code page cannot decode either, invalid
/// bytes become U+FFFD exactly as with [`String::from_utf8_lossy`].
#[must_use]
pub fn decode(bytes: &[u8]) -> String {
    let code_page = console_code_page();
    decode_lines(bytes, |line| decode_code_page(line, code_page?))
}

fn decode_lines(bytes: &[u8], legacy: impl Fn(&[u8]) -> Option<String>) -> String {
    let mut text = String::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        match std::str::from_utf8(line) {
            Ok(line) => text.push_str(line),
            Err(_) => match legacy(line) {
                Some(line) => text.push_str(&line),
                None => text.push_str(&String::from_utf8_lossy(line)),
            },
        }
    }
    text
}

/// The code page console programs use for redirected output, unless it is
/// UTF-8 already.
#[cfg(windows)]
#[allow(unsafe_code)]
fn console_code_page() -> Option<u32> {
    use windows_sys::Win32::Globalization::GetOEMCP;
    use windows_sys::Win32::System::Console::GetConsoleOutputCP;

    const CP_UTF8: u32 = 65_001;
    // SAFETY: both functions take no arguments and only read process state.
    // Without an attached console the output code page reads as 0, and console
    // programs fall back to the OEM code page.
    let code_page = match unsafe { GetConsoleOutputCP() } {
        0 => unsafe { GetOEMCP() },
        code_page => code_page,
    };
    (code_page != CP_UTF8).then_some(code_page)
}

#[cfg(not(windows))]
fn console_code_page() -> Option<u32> {
    None
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn decode_code_page(line: &[u8], code_page: u32) -> Option<String> {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;

    let length = i32::try_from(line.len()).ok()?;
    // SAFETY: `line` is readable for `length` bytes. A null output buffer with
    // zero capacity only asks for the UTF-16 length the conversion needs.
    let needed = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            line.as_ptr(),
            length,
            std::ptr::null_mut(),
            0,
        )
    };
    let capacity = usize::try_from(needed).ok().filter(|needed| *needed > 0)?;
    let mut wide = vec![0_u16; capacity];
    // SAFETY: `wide` is writable for `needed` UTF-16 units.
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            line.as_ptr(),
            length,
            wide.as_mut_ptr(),
            needed,
        )
    };
    wide.truncate(usize::try_from(written).ok().filter(|written| *written > 0)?);
    Some(String::from_utf16_lossy(&wide))
}

#[cfg(not(windows))]
fn decode_code_page(_line: &[u8], _code_page: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::decode_lines;

    /// A two-character GBK table stands in for the console code page.
    fn fake_gbk(line: &[u8]) -> Option<String> {
        let mut text = String::new();
        let mut bytes = line.iter().copied();
        while let Some(byte) = bytes.next() {
            text.push(match (byte, bytes.clone().next()) {
                (byte, _) if byte.is_ascii() => char::from(byte),
                (0xc4, Some(0xe3)) => '你',
                (0xba, Some(0xc3)) => '好',
                _ => return None,
            });
            if !byte.is_ascii() {
                bytes.next();
            }
        }
        Some(text)
    }

    #[test]
    fn utf8_lines_stay_utf8_and_others_use_the_code_page() {
        let mut output = "git: 已修改 文件.md\r\n".as_bytes().to_vec();
        output.extend_from_slice(b"dir: \xc4\xe3\xba\xc3\r\n");
        output.extend_from_slice("done".as_bytes());

        assert_eq!(
            decode_lines(&output, fake_gbk),
            "git: 已修改 文件.md\r\ndir: 你好\r\ndone"
        );
    }

    #[test]
    fn an_undecodable_line_falls_back_to_replacement_characters() {
        assert_eq!(decode_lines(b"a\xffb\n", |_| None), "a\u{fffd}b\n");
        assert_eq!(super::decode(b"plain\n"), "plain\n");
    }
}
