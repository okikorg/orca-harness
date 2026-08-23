//! Getting text out of the TUI and into the user's clipboard.
//!
//! The terminal is the only clipboard we can reach: an alternate-screen
//! app has no window, and shelling out to `pbcopy`/`xclip` breaks the
//! moment the session is over SSH. OSC 52 hands the payload to whatever
//! terminal is actually in front of the user — local or at the far end of
//! the connection — so one code path covers both.
//!
//! Everything here is pure. The TUI builds the escape sequence during
//! event handling and writes it between frames; see `tui::run`.

/// Wrap `text` in an OSC 52 clipboard write for the `c` (system
/// clipboard) selection.
///
/// Terminated with BEL rather than ST: both are legal, BEL is the older
/// form and the one every terminal that implements OSC 52 accepts.
///
/// Under tmux this is silently dropped unless the user has
/// `set -g set-clipboard on` — the single most common "nothing
/// happened" report, so `/copy` says so in its confirmation.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Some terminals truncate very large OSC 52 payloads. Refuse past this
/// rather than copying a silently-clipped fragment the user would paste
/// without noticing.
///
/// Measured against the *raw* text, not the sequence that goes out:
/// base64 inflates by 4/3, so this 64KB ceiling is ~87KB on the wire —
/// still inside the ~100KB terminals typically accept. Raising it means
/// checking the inflated figure, not this one.
pub const MAX_COPY_BYTES: usize = 64 * 1024;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding. Hand-rolled to keep the dependency
/// list as it is for thirty lines of table lookup.
fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The body of the last fenced code block in `markdown`, fence lines
/// excluded.
///
/// The last block rather than the first: when an answer explains a
/// problem and then shows the fix, the fix is what the user reaches for.
/// An unterminated fence still yields its body — a copy interrupted
/// mid-stream is more useful than nothing.
pub fn last_code_block(markdown: &str) -> Option<String> {
    let mut last: Option<String> = None;
    let mut body: Option<Vec<&str>> = None;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            match body.take() {
                // A closing fence. An empty block is not worth copying,
                // and treating it as one would shadow a real block above.
                Some(lines) if !lines.is_empty() => last = Some(lines.join("\n")),
                Some(_) => {}
                None => body = Some(Vec::new()),
            }
        } else if let Some(lines) = body.as_mut() {
            lines.push(line);
        }
    }
    // An unterminated block at end of input is still worth returning.
    if let Some(lines) = body {
        if !lines.is_empty() {
            last = Some(lines.join("\n"));
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_round_trips_multibyte_text() {
        // Non-ASCII is where a byte/char confusion would show up.
        assert_eq!(base64("héllo → ✓".as_bytes()), "aMOpbGxvIOKGkiDinJM=");
    }

    #[test]
    fn osc52_wraps_the_payload_for_the_system_selection() {
        assert_eq!(osc52("foo"), "\x1b]52;c;Zm9v\x07");
    }

    #[test]
    fn last_code_block_takes_the_final_fence() {
        let md = "intro\n```rust\nlet a = 1;\n```\nmiddle\n```sh\ncargo test\n```\nend";
        assert_eq!(last_code_block(md).as_deref(), Some("cargo test"));
    }

    #[test]
    fn last_code_block_keeps_interior_blank_lines() {
        let md = "```\nfirst\n\nsecond\n```";
        assert_eq!(last_code_block(md).as_deref(), Some("first\n\nsecond"));
    }

    #[test]
    fn last_code_block_returns_an_unterminated_block() {
        // Streaming was interrupted before the closing fence arrived.
        let md = "here you go\n```rust\nfn main() {}";
        assert_eq!(last_code_block(md).as_deref(), Some("fn main() {}"));
    }

    #[test]
    fn last_code_block_is_none_without_a_fence() {
        assert_eq!(last_code_block("just prose\nand more prose"), None);
    }

    #[test]
    fn last_code_block_ignores_an_empty_fence_pair() {
        assert_eq!(last_code_block("```\n```"), None);
    }
}
