use ratatui::text::{Line, Span};

use crate::msg::{Provider, ProviderAuth};
use crate::tui::components::picker::ListPicker;
use crate::tui::components::transcript::{transcript_spacing, TranscriptSpacing};
use crate::tui::components::tray::Tray;
use crate::view::{self, theme};

use super::super::format::{age_label, redact_command, size};
use super::super::{App, ViewMode, PICKER_ROWS, SESSIONS_WINDOW};
pub(crate) fn provider_lines(picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let rows = Provider::ALL.iter().map(|provider| {
        let key_note = match provider.auth() {
            ProviderAuth::None => "no key needed".to_string(),
            ProviderAuth::OAuth => {
                let status = crate::auth::status_summary(*provider);
                format!("subscription OAuth · {status}")
            }
            ProviderAuth::ApiKey { environment } if provider.env_key().is_some() => {
                format!("key from ${environment}")
            }
            ProviderAuth::ApiKey { environment } => {
                format!("${environment} not set — will ask")
            }
        };
        format!(
            "{:<12} {:<36} {key_note}",
            provider.label(),
            provider.base_url()
        )
    });
    picker.lines(
        "Select provider · ↑↓ navigate · enter use · esc close",
        rows,
        width,
    )
}

/// The /usage panel: same tray styling as the provider and theme
/// pickers, but read-only — nothing to select, esc/enter/q closes.
pub(crate) fn usage_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let total = app.tokens_in + app.tokens_out + app.cache_read_total + app.cache_write_total;
    let context = match app.context_window {
        Some(window) if window > 0 => format!(
            "{} / {} ({}%)",
            app.context_tokens,
            window,
            (100 * app.context_tokens / window).min(999),
        ),
        _ => format!("~{} (window unknown)", app.context_tokens),
    };
    let mut tray = Tray::new("Session usage · esc close", t.dim);
    let rows = [
        ("model", app.cfg.model_name.clone()),
        ("context", context),
        ("input", app.tokens_in.to_string()),
        ("output", app.tokens_out.to_string()),
        ("cache read", app.cache_read_total.to_string()),
        ("cache write", app.cache_write_total.to_string()),
        ("total", total.to_string()),
        ("model steps", app.usage_steps.to_string()),
    ];
    for (label, value) in rows {
        let text = format!("  {label:<12} {value}");
        tray.push(Line::from(Span::styled(
            view::truncate_line(&text, width),
            t.dim,
        )));
    }
    tray.lines()
}

pub(crate) fn theme_picker_lines(picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let current = view::theme_name();
    let rows = view::ThemeName::ALL.iter().map(|name| {
        let note = if *name == current { "current" } else { "" };
        format!("{:<16} {:<16} {note}", name.label(), name.slug())
    });
    picker.lines(
        "Select theme · ↑↓ navigate · enter use · esc close",
        rows,
        width,
    )
}

pub(crate) fn view_picker_lines(
    current: ViewMode,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let rows = ViewMode::ALL.iter().map(|mode| {
        let note = if *mode == current { "current" } else { "" };
        let description = match mode {
            ViewMode::Classic => "transcript with inline work rail",
            ViewMode::Split => "tool rail with connected inspector",
        };
        format!("{:<10} {:<38} {note}", mode.label(), description)
    });
    picker.lines(
        "Select view · ↑↓ navigate · enter use · esc close",
        rows,
        width,
    )
}

/// The /settings tray: current values for the persisted preferences,
/// enter drills into the matching picker.
pub(crate) fn settings_lines(app: &App, picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let provider = app.cfg.provider;
    let key_status = match provider.key_env() {
        None => "not needed".to_string(),
        Some(env) if provider.env_key().is_some() => format!("from ${env}"),
        Some(_) if provider.stored_key().is_some() => "saved in config".to_string(),
        Some(_) => "not set".to_string(),
    };
    let approvals = crate::config::stored_approvals(&app.cfg.workspace_root);
    let approvals_status = if approvals.is_empty() {
        "none saved".to_string()
    } else {
        approvals.join(", ")
    };
    let rows = [
        ("provider", provider.label().to_string()),
        ("model", app.cfg.model_name.clone()),
        ("theme", view::theme_name().label().to_string()),
        ("view", app.view_mode.label().to_string()),
        ("api key", key_status),
        ("approvals", approvals_status),
        ("spacing", transcript_spacing().label().to_string()),
    ];
    let mut lines = picker.lines(
        "Settings · ↑↓ navigate · enter change · esc close",
        rows.iter()
            .map(|(name, value)| format!("{name:<10} {value}")),
        width,
    );
    if let Some(path) = crate::config::config_path() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            view::truncate_line(&format!("  saved to {}", path.display()), width),
            t.dim,
        )));
    }
    lines
}

pub(crate) fn transcript_spacing_lines(picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let current = transcript_spacing();
    let rows = TranscriptSpacing::ALL.iter().map(|spacing| {
        let note = if *spacing == current { "current" } else { "" };
        let description = match spacing {
            TranscriptSpacing::Compact => "no blank rows between sections",
            TranscriptSpacing::Comfortable => "one blank row between sections",
        };
        format!("{:<14} {:<36} {note}", spacing.label(), description)
    });
    picker.lines(
        "Transcript spacing · ↑↓ navigate · enter use · esc close",
        rows,
        width,
    )
}

/// This workspace's saved always-allowed tools; enter revokes the
/// selected one so it prompts again.
pub(crate) fn approvals_lines(
    tools: &[String],
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    picker.lines(
        "Saved approvals (this workspace) · ↑↓ navigate · enter revoke · esc close",
        tools.iter().cloned(),
        width,
    )
}

/// The harness extension catalog with live on/off state; enter toggles
/// the selected extension and the list stays open.
pub(crate) fn extensions_picker_lines(picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let rows = crate::extensions::EXTENSIONS.iter().map(|spec| {
        let state = if crate::extensions::is_enabled(spec) {
            "on "
        } else {
            "off"
        };
        format!("{:<12} {state}  {}", spec.name, spec.description)
    });
    picker.lines(
        "Extensions · ↑↓ navigate · enter toggle · esc close",
        rows,
        width,
    )
}

/// The configured MCP servers: state, tool count, launch command.
///
/// On/off comes from `servers` (the overlay's own copy, updated the
/// instant the toggle is saved) so a press redraws now; the tool count
/// comes from the shared handle and lags by one reconnect, showing `…`
/// until the worker reports. A server that failed to connect shows why
/// instead of a count. Commands are redacted: the config may hold a
/// literal token, and this list is the one place it would be on screen.
pub(crate) fn mcp_picker_lines(
    servers: &[crate::config::McpServer],
    mcp: &crate::mcp::McpServers,
    filter: &str,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let name_width = servers
        .iter()
        .map(|server| server.name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(4, 16);
    let indices = matching_indices(servers, filter, |server| &server.name);
    let rows = indices.into_iter().map(|index| {
        let server = &servers[index];
        let (count, why) = if !server.enabled {
            (String::new(), String::new())
        } else {
            match mcp.state(&server.name) {
                Some(crate::mcp::McpState::Connected(1)) => ("1 tool".into(), String::new()),
                Some(crate::mcp::McpState::Connected(n)) => (format!("{n} tools"), String::new()),
                // The reason goes after the command, not in the count
                // column: an error is far too long to keep the columns
                // aligned, and it would push the command off the row.
                Some(crate::mcp::McpState::Failed(err)) => ("failed".into(), format!("  — {err}")),
                None => ("…".into(), String::new()),
            }
        };
        let state = if server.enabled { "on " } else { "off" };
        format!(
            "{:<name_width$}  {state}  {:<9}  {}{why}",
            server.name,
            count,
            redact_command(&server.command)
        )
    });
    let filter_note = if filter.is_empty() {
        "type to filter".into()
    } else {
        format!("filter: {filter}")
    };
    picker.windowed_lines(
        &format!("MCP servers · {filter_note} · ↑↓ navigate · space toggle · esc close"),
        rows,
        width,
        PICKER_ROWS,
    )
}

/// The skills found on disk: state, where each came from, and what it
/// is for.
///
/// On/off comes from `entries` (the overlay's own copy, updated the
/// instant the toggle is saved) so a press redraws now. Rows that could
/// not load, or that lost a name collision to an earlier root, are
/// listed too — a skill that silently is not there is the failure mode
/// worth spending a row on.
pub(crate) fn skills_picker_lines(
    entries: &[crate::skills::SkillEntry],
    filter: &str,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let name_width = entries
        .iter()
        .map(|entry| entry.name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(4, 20);
    let indices = matching_indices(entries, filter, |entry| &entry.name);
    let rows = indices.into_iter().map(|index| {
        let entry = &entries[index];
        let (state, detail) = match &entry.state {
            crate::skills::SkillState::Loaded { root, bytes } => (
                if entry.enabled { "on " } else { "off" },
                format!("{root}  {}  {}", size(*bytes), entry.description),
            ),
            crate::skills::SkillState::Shadowed { root, by } => {
                ("—  ", format!("{root}  shadowed by {by}"))
            }
            crate::skills::SkillState::Failed { root, reason } => {
                ("—  ", format!("{root}  failed — {reason}"))
            }
        };
        format!("{:<name_width$}  {state}  {detail}", entry.name)
    });
    let filter_note = if filter.is_empty() {
        "type to filter".into()
    } else {
        format!("filter: {filter}")
    };
    picker.windowed_lines(
        &format!("Skills · {filter_note} · ↑↓ navigate · enter toggle · esc close"),
        rows,
        width,
        PICKER_ROWS,
    )
}

pub(crate) fn matching_indices<T, F>(items: &[T], filter: &str, label: F) -> Vec<usize>
where
    F: Fn(&T) -> &str,
{
    let needle = filter.to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| needle.is_empty() || label(item).to_lowercase().contains(&needle))
        .map(|(index, _)| index)
        .collect()
}

/// Recorded sessions for this workspace, newest first; enter resumes
/// the selected one. Same navigation grammar as the /models picker:
/// the newest sessions fill a bounded window, and ↑↓/PgUp/PgDn move
/// the cursor through the entire list with the position in the header.
pub(crate) fn sessions_picker_lines(
    sessions: &[orca_harness_extensions::SessionFile],
    current: Option<&str>,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let rows = sessions.iter().map(|session| {
        let note = if current == Some(session.meta.id.as_str()) {
            "  (current)"
        } else {
            ""
        };
        format!(
            "{}  {:<8} {}{note}",
            session.meta.id,
            age_label(session.meta.created_at),
            session.meta.model,
        )
    });
    picker.windowed_lines(
        "Sessions (this workspace) · ↑↓ navigate · PgUp/PgDn page · enter resume · esc close",
        rows,
        width,
        SESSIONS_WINDOW,
    )
}

pub(crate) fn api_key_lines(provider: Provider, input: &str) -> Vec<Line<'static>> {
    let t = theme();
    let mut tray = Tray::new(
        format!(
            "{} API key (saved for future sessions) · enter confirm · esc cancel",
            provider.label()
        ),
        t.warn,
    );
    tray.push(Line::from(vec![
        Span::styled("  key: ", t.dim),
        Span::raw("•".repeat(input.chars().count())),
    ]));
    tray.lines()
}
