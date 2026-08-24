//! Shared normalization for composer location mentions.

/// Drop the `@` marker from `@path` mentions before the prompt reaches the
/// model. The marker is composer syntax for the location picker, not part of
/// the path: left in place the model copies it verbatim into tool arguments
/// and every path lookup fails. The composer and transcript keep the `@` so
/// the user still sees what they typed.
pub(crate) fn strip_location_mentions(prompt: &str) -> String {
    let mut out = String::with_capacity(prompt.len());
    let mut rest = prompt;
    while !rest.is_empty() {
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (token, tail) = rest.split_at(token_end);
        out.push_str(strip_one_mention(token));

        let gap_end = tail
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(tail.len());
        out.push_str(&tail[..gap_end]);
        rest = &tail[gap_end..];
    }
    out
}

/// A mention is a whole token of the form `@path`. A second `@` means the
/// token is an address or handle (`@user@host`), which is left untouched.
pub(crate) fn strip_one_mention(token: &str) -> &str {
    match token.strip_prefix('@') {
        Some(path) if !path.is_empty() && !path.contains('@') => path,
        _ => token,
    }
}
