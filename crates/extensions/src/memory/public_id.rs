use rusqlite::Connection;
use serde_json::Value;

use orca_harness_core::ToolError;

const PREFIX: &str = "mem_";
const RANDOM_HEX_LEN: usize = 32;

pub(super) fn generate(connection: &Connection) -> rusqlite::Result<String> {
    connection.query_row("SELECT 'mem_' || lower(hex(randomblob(16)))", [], |row| {
        row.get(0)
    })
}

pub(super) fn required(input: &Value) -> Result<&str, ToolError> {
    let id = input
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| is_valid(id))
        .ok_or_else(|| ToolError::msg("`id` (opaque mem_ identifier) is required"))?;
    Ok(id)
}

pub(super) fn is_valid(id: &str) -> bool {
    id.len() == PREFIX.len() + RANDOM_HEX_LEN
        && id.starts_with(PREFIX)
        && id[PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_only_canonical_ids() {
        assert!(is_valid("mem_0123456789abcdef0123456789abcdef"));
        assert!(!is_valid("1"));
        assert!(!is_valid("mem_0123456789ABCDEF0123456789ABCDEF"));
        assert!(!is_valid("mem_0123456789abcdef"));
    }
}
