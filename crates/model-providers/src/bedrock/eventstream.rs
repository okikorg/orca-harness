//! AWS event-stream framing: 12-byte prelude, typed headers, payload, CRC32.
use orca_harness_core::ModelError;
use serde_json::Value;

fn invalid(msg: impl Into<String>) -> ModelError {
    ModelError::InvalidResponse(format!("Bedrock eventstream: {}", msg.into()))
}
fn word(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().unwrap())
}
#[derive(Default)]
pub(super) struct Decoder {
    buffer: Vec<u8>,
}
impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, ModelError> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            if self.buffer.len() < 12 {
                break;
            }
            let size = word(&self.buffer[..4]) as usize;
            let headers_len = word(&self.buffer[4..8]) as usize;
            if !(16..=16 * 1024 * 1024).contains(&size) || headers_len > size - 16 {
                return Err(invalid("invalid frame length"));
            }
            if crc32fast::hash(&self.buffer[..8]) != word(&self.buffer[8..12]) {
                return Err(invalid("prelude CRC mismatch"));
            }
            if self.buffer.len() < size {
                break;
            }
            let frame = &self.buffer[..size];
            if crc32fast::hash(&frame[..size - 4]) != word(&frame[size - 4..]) {
                return Err(invalid("frame CRC mismatch"));
            }
            let mut headers = &frame[12..12 + headers_len];
            let mut event_type = None;
            let mut message_type = None;
            let mut exception_type = None;
            while !headers.is_empty() {
                let n = headers[0] as usize;
                if headers.len() < 2 + n || n == 0 {
                    return Err(invalid("truncated header name"));
                }
                let name = std::str::from_utf8(&headers[1..1 + n])
                    .map_err(|_| invalid("header name UTF-8"))?;
                let tag = headers[1 + n];
                headers = &headers[2 + n..];
                // AWS headers may include non-string values; skip all documented types.
                let len = match tag {
                    0 | 1 => 0,
                    2 => 1,
                    3 => 2,
                    4 => 4,
                    5 | 8 => 8,
                    9 => 16,
                    6 | 7 => {
                        if headers.len() < 2 {
                            return Err(invalid("truncated header length"));
                        }
                        let n = u16::from_be_bytes([headers[0], headers[1]]) as usize;
                        headers = &headers[2..];
                        n
                    }
                    _ => return Err(invalid("unknown header type")),
                };
                if headers.len() < len {
                    return Err(invalid("truncated header value"));
                }
                if matches!(
                    name,
                    ":event-type" | ":message-type" | ":exception-type" | ":error-code"
                ) {
                    if tag != 7 {
                        return Err(invalid("invalid event header type"));
                    }
                    let value = std::str::from_utf8(&headers[..len])
                        .map_err(|_| invalid("header value UTF-8"))?;
                    if name == ":event-type" {
                        event_type = Some(value.to_owned());
                    } else if name == ":message-type" {
                        message_type = Some(value.to_owned());
                    } else {
                        exception_type = Some(value.to_owned());
                    }
                }
                headers = &headers[len..];
            }
            let payload = &frame[12 + headers_len..size - 4];
            let json: Value = serde_json::from_slice(payload)
                .map_err(|e| invalid(format!("payload JSON: {e}")))?;
            match message_type.as_deref() {
                Some("event") => {
                    let kind = event_type.ok_or_else(|| invalid("missing event type"))?;
                    let data = json.get(&kind).cloned().unwrap_or(json);
                    let mut event = serde_json::json!({"type":kind});
                    event[&kind] = data;
                    events.push(event);
                }
                Some("exception" | "error") => {
                    let kind = exception_type
                        .as_deref()
                        .or(event_type.as_deref())
                        .unwrap_or("unknown");
                    let status = match kind.to_ascii_lowercase().as_str() {
                        "throttlingexception" => 429,
                        "serviceunavailableexception" => 503,
                        _ => return Err(invalid(format!("stream exception {kind}: {json}"))),
                    };
                    return Err(crate::http_error::stream_error(&serde_json::json!({
                        "status": status, "code": kind, "message": json["message"]
                    })));
                }
                _ => return Err(invalid("missing message type")),
            }
            self.buffer.drain(..size);
        }
        Ok(events)
    }
    pub fn finish(&self) -> Result<(), ModelError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(ModelError::IncompleteResponse {
                message: "truncated Bedrock eventstream frame".into(),
                usage: None,
            })
        }
    }
}

/// Frame builders for tests of this decoder and of the Bedrock wire.
#[cfg(test)]
pub(super) mod testing {
    pub fn header(name: &str, value: &str) -> Vec<u8> {
        let mut out = vec![name.len() as u8];
        out.extend(name.as_bytes());
        out.push(7);
        out.extend((value.len() as u16).to_be_bytes());
        out.extend(value.as_bytes());
        out
    }
    pub fn frame(kind: &str, payload: &str) -> Vec<u8> {
        let mut h = header(":message-type", "event");
        h.extend(header(":event-type", kind));
        frame_with_headers(h, payload)
    }
    pub fn frame_with_headers(h: Vec<u8>, payload: &str) -> Vec<u8> {
        let size = 16 + h.len() + payload.len();
        let mut out = Vec::new();
        out.extend((size as u32).to_be_bytes());
        out.extend((h.len() as u32).to_be_bytes());
        out.extend(crc32fast::hash(&out).to_be_bytes());
        out.extend(h);
        out.extend(payload.as_bytes());
        out.extend(crc32fast::hash(&out).to_be_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    #[test]
    fn exception_headers_preserve_retry_classification() {
        for (kind, status) in [
            ("throttlingException", 429),
            ("serviceUnavailableException", 503),
        ] {
            // The last row has no exception header and falls back to :event-type.
            for (message_type, code_header) in [
                ("exception", ":exception-type"),
                ("error", ":error-code"),
                ("error", ":event-type"),
            ] {
                let mut headers = header(":message-type", message_type);
                headers.extend(header(code_header, kind));
                let bytes = frame_with_headers(headers, r#"{"message":"retry later"}"#);
                let error = Decoder::default().push(&bytes).unwrap_err();
                let ModelError::Request(message) = error else {
                    panic!("expected retryable request error")
                };
                assert!(
                    message.contains(&format!("\"status\":{status}")),
                    "{message}"
                );
                assert!(message.contains(kind));
                assert!(message.contains("retry later"));
            }
        }
    }
    #[test]
    fn exception_and_invalid_lengths() {
        let mut bad = frame("messageStart", "{}");
        bad[..4].copy_from_slice(&15u32.to_be_bytes());
        let checksum = crc32fast::hash(&bad[..8]);
        bad[8..12].copy_from_slice(&checksum.to_be_bytes());
        assert!(Decoder::default().push(&bad).is_err());
        assert!(Decoder::default().push(&[1, 2, 3]).unwrap().is_empty());
    }
    #[test]
    fn fragmentation_crc_and_truncation() {
        let bytes = frame("messageStop", r#"{"stopReason":"end_turn"}"#);
        let mut d = Decoder::default();
        for b in &bytes[..bytes.len() - 1] {
            assert!(d.push(&[*b]).unwrap().is_empty());
        }
        assert!(d.finish().is_err());
        assert_eq!(
            d.push(&bytes[bytes.len() - 1..]).unwrap()[0]["messageStop"]["stopReason"],
            "end_turn"
        );
        d.finish().unwrap();
        let mut bad = bytes.clone();
        bad[10] ^= 1;
        assert!(Decoder::default().push(&bad).is_err());
        let mut bad = bytes;
        bad[20] ^= 1;
        assert!(Decoder::default().push(&bad).is_err());
    }
}
