//! Bounded buffers and terminal cleanup for the REPL's output streams.

pub(super) fn push_bounded(buffer: &mut Vec<u8>, chunk: &[u8], cap: usize) -> u64 {
    buffer.extend_from_slice(chunk);
    trim_front(buffer, cap)
}

pub(super) fn trim_front(buffer: &mut Vec<u8>, cap: usize) -> u64 {
    if buffer.len() <= cap {
        return 0;
    }
    let excess = buffer.len() - cap;
    buffer.drain(..excess);
    excess as u64
}

pub(super) fn truncate_tail(text: &mut String, max_bytes: usize) -> u64 {
    if text.len() <= max_bytes {
        return 0;
    }
    let mut cut = text.len() - max_bytes;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    text.drain(..cut);
    cut as u64
}

pub(super) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Bun's line editor redraws piped input with CR + CSI 2K. Those rows are
/// input echo, not program output, so discard them along with the banner and
/// `.load` notice. User output is otherwise preserved as plain text.
pub(super) fn clean_stdout(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .split_inclusive('\n')
        .filter(|line| !line.contains("\u{1b}[2K"))
        .map(strip_ansi)
        .filter(|line| {
            let line = line.trim();
            !line.starts_with("Welcome to Bun")
                && !line.starts_with("Type .copy")
                && !line.starts_with("Loading ")
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

pub(super) fn clean_text(raw: &[u8]) -> String {
    strip_ansi(&String::from_utf8_lossy(raw)).trim().to_owned()
}

pub(super) fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else if ch != '\r' {
            output.push(ch);
        }
    }
    output
}
