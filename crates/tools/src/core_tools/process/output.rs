//! Per-process output state: the merged, bounded unread buffer, the
//! separate notification buffer, and the one-shot literal matcher behind
//! `notifyOnMatch`.

pub(super) struct MatchState {
    pub(super) pattern: String,
    needle: Vec<u8>,
    tail: Vec<u8>,
    armed: bool,
    notified: bool,
}

impl MatchState {
    pub(super) fn new(pattern: String) -> Self {
        Self {
            needle: pattern.as_bytes().to_vec(),
            pattern,
            tail: Vec::new(),
            armed: false,
            notified: false,
        }
    }

    pub(super) fn observe(&mut self, chunk: &[u8]) -> bool {
        if !self.armed || self.notified {
            return false;
        }
        let mut window = Vec::with_capacity(self.tail.len() + chunk.len());
        window.extend_from_slice(&self.tail);
        window.extend_from_slice(chunk);
        if window
            .windows(self.needle.len())
            .any(|candidate| candidate == self.needle)
        {
            self.notified = true;
            return true;
        }
        let keep = self.needle.len().saturating_sub(1).min(window.len());
        self.tail.clear();
        self.tail.extend_from_slice(&window[window.len() - keep..]);
        false
    }

    pub(super) fn arm(&mut self) {
        self.tail.clear();
        self.armed = true;
    }
}

/// Merged, bounded, unread output of one process.
pub(super) struct OutBuf {
    data: Vec<u8>,
    dropped: u64,
    cap: usize,
    notification_data: Vec<u8>,
    notification_dropped: u64,
    notification_cap: usize,
    notify_match: Option<MatchState>,
}

impl OutBuf {
    /// `notification_cap` of zero disables the notification buffer;
    /// `notify_match` is the literal to wake the host on, if any.
    pub(super) fn new(cap: usize, notification_cap: usize, notify_match: Option<String>) -> Self {
        Self {
            data: Vec::new(),
            dropped: 0,
            cap,
            notification_data: Vec::new(),
            notification_dropped: 0,
            notification_cap,
            notify_match: notify_match.map(MatchState::new),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub(super) fn push(&mut self, chunk: &[u8]) -> Option<String> {
        let matched = self
            .notify_match
            .as_mut()
            .and_then(|state| state.observe(chunk).then(|| state.pattern.clone()));
        self.data.extend_from_slice(chunk);
        if self.data.len() > self.cap {
            let excess = self.data.len() - self.cap;
            self.data.drain(..excess);
            self.dropped += excess as u64;
        }
        if self.notification_cap > 0 {
            self.notification_data.extend_from_slice(chunk);
            if self.notification_data.len() > self.notification_cap {
                let excess = self.notification_data.len() - self.notification_cap;
                self.notification_data.drain(..excess);
                self.notification_dropped += excess as u64;
            }
        }
        matched
    }

    /// Take up to `max` bytes. Cuts may split a UTF-8 sequence; the lossy
    /// conversion degrades that to a replacement char at the seam only.
    pub(super) fn drain(&mut self, max: usize) -> (String, u64, bool) {
        let dropped = std::mem::take(&mut self.dropped);
        let take = self.data.len().min(max);
        let chunk: Vec<u8> = self.data.drain(..take).collect();
        let more = !self.data.is_empty();
        (String::from_utf8_lossy(&chunk).into_owned(), dropped, more)
    }

    pub(super) fn drain_notification(&mut self) -> (String, u64) {
        let output = String::from_utf8_lossy(&std::mem::take(&mut self.notification_data)).into();
        let dropped = std::mem::take(&mut self.notification_dropped);
        (output, dropped)
    }

    pub(super) fn arm_notifications(&mut self) {
        self.notification_data.clear();
        self.notification_dropped = 0;
        if let Some(state) = &mut self.notify_match {
            state.arm();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MatchState;

    #[test]
    fn output_match_spans_chunks_and_notifies_once() {
        let mut state = MatchState::new("server ready".into());
        state.arm();

        assert!(!state.observe(b"server rea"));
        assert!(state.observe(b"dy on :3000"));
        assert!(!state.observe(b" server ready again"));
    }

    #[test]
    fn output_match_ignores_everything_before_it_is_armed() {
        let mut state = MatchState::new("ready".into());

        assert!(!state.observe(b"ready"));
        state.arm();
        assert!(!state.observe(b"ady"), "pre-arm tail is discarded");
        assert!(state.observe(b"ready"));
    }
}
