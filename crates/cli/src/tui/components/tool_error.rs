//! Compact error previews; the inspector retains the complete payload.
use ratatui::text::Line;
use serde_json::Value;

pub(crate) fn lines(output: &Value, width: usize, continuation: &str) -> Vec<Line<'static>> {
    let message = output
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| output.get("message").and_then(Value::as_str))
        .or_else(|| output.as_str())
        .or_else(|| {
            output
                .get("stderr")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        });
    let message = message
        .filter(|text| !text.trim().is_empty())
        .unwrap_or("Tool failed · inspect for details");
    let message = message.strip_prefix("read failed: ").unwrap_or(message);
    let message = if let Some(rest) = message.strip_prefix("No such file or directory (os error 2)")
    {
        format!(
            "File not found{}",
            rest.replace("; nearest existing directory ", " · ")
        )
    } else {
        message.to_string()
    };
    let message = match output.get("exitCode").and_then(Value::as_i64) {
        Some(code) => format!("Exit {code} · {message}"),
        None => message,
    };
    let prefix = format!("    {continuation}   ");
    let message = crate::view::sanitize_cells(&message);
    let body_width = width
        .saturating_sub(crate::view::cell_width(&prefix))
        .max(1);
    let mut lines: Vec<_> = textwrap::wrap(&message, body_width)
        .into_iter()
        .map(|part| {
            super::layout::fit(
                Line::from(vec![
                    ratatui::text::Span::styled(prefix.clone(), crate::view::theme().dim),
                    ratatui::text::Span::styled(part.into_owned(), crate::view::theme().error),
                ]),
                width,
            )
        })
        .collect();
    if lines.len() > 3 {
        lines.truncate(2);
        lines.extend(
            super::layout::wrapped(
                "… inspect for full error",
                &prefix,
                width,
                crate::view::theme().dim,
            )
            .into_iter()
            .take(1),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_message_is_readable_without_json_and_keeps_continuation() {
        let output = serde_json::json!({"error": "read failed: No such file or directory (os error 2); nearest existing directory docs/architecture is empty"});
        let rows = lines(&output, 80, "│ ");
        assert!(rows.iter().all(|row| row.width() <= 80));
        assert!(rows[0].to_string().starts_with("    │    File not found"));
        assert!(!rows.iter().any(|row| row.to_string().contains("\"error\"")));
        assert!(rows.iter().any(|row| row.to_string().contains("empty")));
        assert!(output.get("error").is_some());
    }
    #[test]
    fn long_and_unknown_errors_are_bounded() {
        for output in [
            serde_json::json!({"error": "failure ".repeat(100)}),
            serde_json::json!({"nested": {"code": 5}}),
        ] {
            let rows = lines(&output, 40, "  ");
            assert!(rows.len() <= 3);
            assert!(rows.iter().all(|row| row.width() <= 40));
            assert!(rows.iter().any(|row| row.to_string().contains("inspect")));
        }
    }
}
