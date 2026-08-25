//! Pure formatting and redaction helpers for the terminal UI: human
//! readable sizes, token counts, durations, elapse labels, paths, and
//! credential redaction for display-only strings. None of these touch
//! `App` state — they are the most self-contained pieces of the TUI and
//! the natural first seam to split off the monolithic module.

use std::path::Path;
use std::time::Duration;

/// The basename of a workspace path for the status line, falling back to
/// the full path when it has no usable file name.
pub(super) fn workspace_status_name(workspace: &str) -> &str {
    Path::new(workspace)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(workspace)
}

/// Short human label for an elapsed duration.
pub(super) fn elapsed_label(elapsed: Duration) -> String {
    if elapsed.as_secs() >= 60 {
        let seconds = elapsed.as_secs();
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else if elapsed.as_secs() > 0 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else if elapsed.as_millis() > 0 {
        format!("{}ms", elapsed.as_millis())
    } else if elapsed.as_micros() > 0 {
        format!("{}µs", elapsed.as_micros())
    } else {
        format!("{}ns", elapsed.as_nanos())
    }
}

/// Chunk-invariant token estimate used for live turn progress. Each
/// whitespace-delimited byte span costs ceil(bytes / 4), matching FX.
#[derive(Default)]
pub(super) struct TokenEstimator {
    settled_tokens: u64,
    span_bytes: u64,
}

impl TokenEstimator {
    pub(super) fn consume(&mut self, text: &str) {
        for byte in text.bytes() {
            if byte.is_ascii_whitespace() {
                self.finish_span();
            } else {
                self.span_bytes = self.span_bytes.saturating_add(1);
            }
        }
    }

    pub(super) fn estimate(&self) -> u64 {
        self.settled_tokens
            .saturating_add(self.span_bytes.div_ceil(4))
    }

    fn finish_span(&mut self) {
        self.settled_tokens = self
            .settled_tokens
            .saturating_add(self.span_bytes.div_ceil(4));
        self.span_bytes = 0;
    }
}

pub(super) fn estimate_tokens(text: &str) -> u64 {
    let mut estimator = TokenEstimator::default();
    estimator.consume(text);
    estimator.estimate()
}

/// `1 item` vs `2 items`.
pub(super) fn plural(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

/// Relative age of a unix-seconds timestamp (session file creation).
pub(super) fn age_label(created_at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let delta = now.saturating_sub(created_at);
    match delta {
        0..=59 => format!("{delta}s ago"),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86_399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86_400),
    }
}

/// Compact token count: `999`, `12.3k`, `1.2m`.
pub(super) fn fmt_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}m", n as f64 / 1_000_000.0),
    }
}

/// FX-style live token count: exact below 1k, one decimal below 10k
/// only when useful, then whole thousands.
pub(super) fn fmt_turn_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    let whole = tokens / 1_000;
    let tenths = (tokens % 1_000) / 100;
    if whole < 10 && tenths > 0 {
        format!("{whole}.{tenths}k")
    } else {
        format!("{whole}k")
    }
}

/// Compact byte count: `512b`, `4.2k`.
pub(super) fn size(bytes: u64) -> String {
    match bytes {
        0..=1023 => format!("{bytes}b"),
        _ => format!("{:.1}k", bytes as f64 / 1024.0),
    }
}

/// Byte offset into a string given a character index (used for composer
/// cursor arithmetic on pastes, where indices are character-based).
pub(super) fn byte_index(s: &str, char_index: usize) -> usize {
    s.char_indices()
        .nth(char_index)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

const REDACTED: &str = "<redacted>";

/// Argument flags whose value is a credential.
const SECRET_FLAGS: &[&str] = &[
    "--header",
    "-H",
    "--api-key",
    "--apikey",
    "--auth",
    "--bearer",
    "--password",
    "--secret",
    "--token",
];

/// Query-parameter and header names whose value is a credential.
const SECRET_NAMES: &[&str] = &[
    "access_token",
    "api-key",
    "api_key",
    "apikey",
    "authorization",
    "key",
    "password",
    "proxy-authorization",
    "secret",
    "token",
    "x-api-key",
];

/// Literal credential shapes recognized wherever they appear, so a bare
/// token pasted as a positional argument is caught too.
const SECRET_PREFIXES: &[&str] = &[
    "AIza",
    "AKIA",
    "Bearer",
    "dop_v1_",
    "ghp_",
    "gho_",
    "ghr_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "glpat-",
    "hf_",
    "sk-",
    "sk_",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "xoxs-",
    "ya29.",
];

/// True for a value that is really an environment reference (`${VAR}`).
/// These name a variable rather than carrying its value, so masking them
/// would hide the one thing the user needs to see — which variable the
/// server depends on — while protecting nothing.
pub(super) fn is_env_reference(value: &str) -> bool {
    value.contains("${")
}

/// Mask a value known to be a credential, keeping env references.
pub(super) fn mask_secret(value: &str) -> String {
    if value.is_empty() || is_env_reference(value) {
        value.to_string()
    } else {
        REDACTED.to_string()
    }
}

/// Mask `name:value` / `name=value` when the name is credential-bearing,
/// keeping the name so the row still says what is being sent.
pub(super) fn mask_pair(word: &str, separators: &[char]) -> Option<String> {
    let (name, value) = word.split_at_checked(word.find(separators)?)?;
    let (sep, value) = value.split_at(1);
    SECRET_NAMES
        .contains(&name.to_ascii_lowercase().as_str())
        .then(|| format!("{name}{sep}{}", mask_secret(value)))
}

/// A launch command with literal credentials masked, for display only.
/// The stored command is untouched — this exists so a token pasted into
/// `/mcp add` is not left on screen for anyone glancing at the terminal.
pub(super) fn redact_command(command: &str) -> String {
    let mut words = Vec::new();
    let mut value_is_secret = false;
    for word in command.split_whitespace() {
        let rendered = if value_is_secret {
            // The value of a `--header`-style flag: `Name:value` keeps
            // its name, anything else is masked whole.
            mask_pair(word, &[':', '=']).unwrap_or_else(|| mask_secret(word))
        } else if let Some(masked) = mask_flag_value(word).or_else(|| mask_url(word)) {
            masked
        } else if SECRET_PREFIXES
            .iter()
            .any(|prefix| word.starts_with(prefix) && word.len() > prefix.len())
        {
            mask_secret(word)
        } else {
            word.to_string()
        };
        value_is_secret = SECRET_FLAGS.contains(&word);
        words.push(rendered);
    }
    words.join(" ")
}

/// `--api-key=secret` and friends, where flag and value share a word.
pub(super) fn mask_flag_value(word: &str) -> Option<String> {
    let (flag, value) = word.split_once('=')?;
    SECRET_FLAGS
        .contains(&flag)
        .then(|| format!("{flag}={}", mask_secret(value)))
}

/// Credentials carried inside a URL: `https://user:token@host` userinfo
/// and `?api_key=…` query parameters.
pub(super) fn mask_url(word: &str) -> Option<String> {
    let (scheme, rest) = word.split_once("://")?;
    let (authority, path) = match rest.find('/') {
        Some(cut) => rest.split_at(cut),
        None => (rest, ""),
    };
    let authority = match authority.rsplit_once('@') {
        // Keep the user, mask the password half of `user:password`.
        Some((userinfo, host)) => match userinfo.split_once(':') {
            Some((user, password)) => format!("{user}:{}@{host}", mask_secret(password)),
            None => format!("{userinfo}@{host}"),
        },
        None => authority.to_string(),
    };
    let path = match path.split_once('?') {
        Some((route, query)) => {
            let masked: Vec<String> = query
                .split('&')
                .map(|param| mask_pair(param, &['=']).unwrap_or_else(|| param.to_string()))
                .collect();
            format!("{route}?{}", masked.join("&"))
        }
        None => path.to_string(),
    };
    Some(format!("{scheme}://{authority}{path}"))
}
