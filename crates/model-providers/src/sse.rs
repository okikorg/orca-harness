//! Byte framing shared by the adapters; their distinct SSE rules stay explicit.
use orca_harness_core::ModelError;

/// Chat Completions accepts individual data lines, including a final line at EOF.
#[derive(Default)]
pub(crate) struct SseLineBuffer {
    buf: Vec<u8>,
    scanned: usize,
}

impl SseLineBuffer {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut payloads = Vec::new();
        let mut consumed = 0;
        // Previously inspected bytes cannot contain a newline. Decode only
        // complete lines so network splits cannot corrupt UTF-8 codepoints.
        for pos in self.scanned..self.buf.len() {
            if self.buf[pos] == b'\n' {
                if let Some(payload) = Self::payload(&self.buf[consumed..pos]) {
                    payloads.push(payload);
                }
                consumed = pos + 1;
            }
        }
        self.buf.drain(..consumed);
        self.scanned = self.buf.len();
        payloads
    }

    pub(crate) fn finish(&mut self) -> Vec<String> {
        self.scanned = 0;
        let line = std::mem::take(&mut self.buf);
        Self::payload(&line).into_iter().collect()
    }

    fn payload(line: &[u8]) -> Option<String> {
        String::from_utf8_lossy(line)
            .trim()
            .strip_prefix("data:")
            .map(|payload| payload.trim().to_string())
    }
}

/// Responses requires blank-line-delimited, strictly UTF-8 frames and joins
/// multiple data lines. Unterminated frames remain buffered, including at EOF.
#[derive(Default)]
pub(crate) struct SseBuffer {
    buf: Vec<u8>,
    scanned: usize,
}

impl SseBuffer {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, ModelError> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut consumed = 0;
        let mut pos = self.scanned;
        while pos < self.buf.len() {
            let rest = &self.buf[pos..];
            let delimiter = if rest.starts_with(b"\n\n") {
                2
            } else if rest.starts_with(b"\r\n\r\n") {
                4
            } else {
                pos += 1;
                continue;
            };
            let end = pos + delimiter;
            let frame = match std::str::from_utf8(&self.buf[consumed..end]) {
                Ok(frame) => frame,
                Err(_) => {
                    // As before, consume the invalid frame but retain any
                    // later frames if the caller chooses to continue.
                    self.buf.drain(..end);
                    self.scanned = 0;
                    return Err(ModelError::InvalidResponse(
                        "Codex SSE frame is not valid UTF-8".into(),
                    ));
                }
            };
            let mut data = String::new();
            let mut first = true;
            for line in frame.lines().filter_map(|line| line.strip_prefix("data:")) {
                if !first {
                    data.push('\n');
                }
                first = false;
                data.push_str(line.trim());
            }
            if !data.is_empty() {
                out.push(data);
            }
            consumed = end;
            pos = end;
        }
        // Compact once per network chunk, not once per frame. Retain enough
        // search overlap to recognize a CRLF delimiter split across chunks.
        self.buf.drain(..consumed);
        self.scanned = self.buf.len().saturating_sub(3);
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
