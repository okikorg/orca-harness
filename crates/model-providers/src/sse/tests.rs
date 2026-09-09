use super::*;

#[test]
fn framing_rules_remain_distinct_at_every_chunk_size() {
    let bytes = b": ignored\r\n data: leading\r\ndata: one\r\ndata:\r\ndata: two\r\n\r\ndata: \n\ndata: last";
    for size in 1..=bytes.len() {
        let mut chat = SseLineBuffer::default();
        let mut responses = SseBuffer::default();
        let mut lines = Vec::new();
        let mut frames = Vec::new();
        for chunk in bytes.chunks(size) {
            lines.extend(chat.push(chunk));
            frames.extend(responses.push(chunk).unwrap());
        }
        lines.extend(chat.finish());
        assert_eq!(
            lines,
            ["leading", "one", "", "two", "", "last"],
            "size {size}"
        );
        assert_eq!(frames, ["one\n\ntwo"], "size {size}");
        // Responses retains an unterminated frame rather than emitting it.
        assert_eq!(responses.push(b"\n\n").unwrap(), ["last"]);
    }
}

#[test]
fn responses_preserves_empty_data_lines_and_mixed_delimiter_order() {
    let bytes = b"data:\ndata:\n\ndata: a\r\n\r\ndata: b\n\ndata:\ndata: c\n\n";
    for split in 0..=bytes.len() {
        let mut buffer = SseBuffer::default();
        let mut frames = buffer.push(&bytes[..split]).unwrap();
        frames.extend(buffer.push(&bytes[split..]).unwrap());
        assert_eq!(frames, ["\n", "a", "b", "\nc"], "split {split}");
    }
}

#[test]
fn invalid_utf8_is_lossy_for_chat_and_consumed_error_for_responses() {
    let bytes = b"data: before\n\ndata: \xff\n\ndata: after\n\n";
    let mut chat = SseLineBuffer::default();
    assert_eq!(chat.push(bytes), ["before", "\u{fffd}", "after"]);
    let mut responses = SseBuffer::default();
    assert!(
        matches!(responses.push(bytes), Err(ModelError::InvalidResponse(message))
        if message == "Codex SSE frame is not valid UTF-8")
    );
    assert_eq!(responses.push(b"").unwrap(), ["after"]);
}

#[test]
fn large_fragmented_frames_and_coalesced_frames_are_equivalent() {
    let text = "café東京".repeat(4096);
    let frame = format!("data: {text}\r\n\r\n");
    for size in [1, 7, 1024, frame.len()] {
        let mut chat = SseLineBuffer::default();
        let mut responses = SseBuffer::default();
        let mut lines = Vec::new();
        let mut frames = Vec::new();
        for chunk in frame.as_bytes().chunks(size) {
            lines.extend(chat.push(chunk));
            frames.extend(responses.push(chunk).unwrap());
        }
        assert_eq!(lines, [text.as_str()]);
        assert_eq!(frames, [text.as_str()]);
    }
    let mut responses = SseBuffer::default();
    assert_eq!(
        responses
            .push("data: x\n\n".repeat(4096).as_bytes())
            .unwrap(),
        vec!["x"; 4096]
    );
}

#[test]
fn chat_finish_resets_search_state_for_reuse() {
    let mut buffer = SseLineBuffer::default();
    assert!(buffer.push(b"data: unfinished").is_empty());
    assert_eq!(buffer.finish(), ["unfinished"]);
    assert_eq!(buffer.push(b"data: next\n"), ["next"]);
    assert!(buffer.finish().is_empty());
}
