use ratatui::text::{Line, Span};

use crate::msg::{Provider, ProviderAuth};
use crate::tui::command_catalog::CommandSpec;
use crate::tui::components::picker::ListPicker;
use crate::tui::components::transcript::{transcript_spacing, TranscriptSpacing};
use crate::tui::components::section::Section;
use crate::view::{self, theme};

use super::super::format::age_label;
use super::super::state::SubagentSetting;
use super::super::subagents::{
    subagent_current, subagent_route_description, subagent_setting_label,
};
use super::super::{App, InspectorMode, ViewMode, PICKER_ROWS, SESSIONS_WINDOW};

pub(super) fn command_picker_row(spec: &CommandSpec) -> [String; 3] {
    [
        format!("/{}", spec.name),
        spec.description.to_string(),
        spec.category.to_string(),
    ]
}

pub(crate) fn help_picker_lines(
    filter: &str,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let commands = crate::tui::command_catalog::filter_commands(filter);
    if commands.is_empty() {
        return vec![Line::from(Span::styled(
            format!("  No commands match {filter} · backspace to widen · esc close"),
            theme().dim,
        ))];
    }
    let filter_note = if filter.is_empty() {
        "type to filter".to_string()
    } else {
        format!("filter: {filter}")
    };
    picker.windowed_table_lines(
        &format!("Help · {filter_note} · ↑↓ move · →/enter use · esc close"),
        commands.into_iter().map(command_picker_row),
        [(6, 16), (12, 64), (0, 10)],
        width,
        PICKER_ROWS,
    )
}

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
        [
            provider.label().to_string(),
            provider.base_url().to_string(),
            key_note,
        ]
    });
    picker.table_lines(
        "Select provider · ↑↓ move · →/enter use · esc close",
        rows,
        [(12, 12), (36, 36), (0, usize::MAX)],
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
    let mut tray = Section::tray("Session usage · esc close", t.dim);
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
        [
            name.label().to_string(),
            name.slug().to_string(),
            note.to_string(),
        ]
    });
    picker.table_lines(
        "Select theme · ↑↓ move · →/enter use · esc close",
        rows,
        [(16, 16), (16, 16), (0, usize::MAX)],
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
        [
            mode.label().to_string(),
            description.to_string(),
            note.to_string(),
        ]
    });
    picker.table_lines(
        "Select view · ↑↓ move · →/enter use · esc close",
        rows,
        [(10, 10), (38, 38), (0, usize::MAX)],
        width,
    )
}

/// The `/mode` selector — same grammar as the provider and theme
/// pickers.
pub(crate) fn mode_picker_lines(
    current: crate::mode::Mode,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let rows = crate::mode::Mode::ALL.iter().map(|mode| {
        let label = mode.label();
        let note = if *mode == current { "current" } else { "" };
        let description = match mode {
            crate::mode::Mode::Normal => "gated tools ask for approval",
            crate::mode::Mode::Plan => "read-only: investigate and propose, change nothing",
            crate::mode::Mode::Auto => "safe tools run; unresolved actions get reviewed",
            crate::mode::Mode::Yolo => "every gated tool runs without asking",
        };
        [label.to_string(), description.to_string(), note.to_string()]
    });
    picker.table_lines(
        "Select mode · ↑↓ move · →/enter use · esc close",
        rows,
        [(10, 10), (46, 46), (0, usize::MAX)],
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
        ("inspector", app.inspector_mode.label().to_string()),
        ("api key", key_status),
        ("approvals", approvals_status),
        ("spacing", transcript_spacing().label().to_string()),
    ];
    let mut lines = picker.table_lines(
        "Settings · ↑↓ move · →/enter open · esc close",
        rows.iter()
            .map(|(name, value)| [name.to_string(), value.clone()]),
        [(10, 10), (0, usize::MAX)],
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

pub(crate) fn subagent_settings_lines(
    app: &App,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let rows = SubagentSetting::ALL.iter().map(|setting| {
        [
            subagent_setting_label(*setting).to_string(),
            subagent_current(&app.cfg.subagent_depth, *setting),
        ]
    });
    picker.table_lines(
        "Subagents (this session) · ↑↓ move · →/enter open · esc close",
        rows,
        [(18, 18), (0, usize::MAX)],
        width,
    )
}

pub(crate) fn subagent_value_lines(
    setting: SubagentSetting,
    values: &[String],
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let title = format!(
        "Subagent {} · ↑↓ move · ← back · →/enter use · esc close",
        subagent_setting_label(setting)
    );
    if setting == SubagentSetting::Route {
        return picker.table_lines(
            &title,
            values
                .iter()
                .map(|value| [value.clone(), subagent_route_description(value).to_string()]),
            [(12, 12), (0, usize::MAX)],
            width,
        );
    }
    picker.lines(&title, values.iter().cloned(), width)
}

pub(crate) fn inspector_picker_lines(
    current: InspectorMode,
    picker: &ListPicker,
    width: usize,
) -> Vec<Line<'static>> {
    let rows = InspectorMode::ALL.iter().map(|mode| {
        let note = if *mode == current { "current" } else { "" };
        let description = match mode {
            InspectorMode::Summary => "concise tool-specific rendering",
            InspectorMode::Debug => "exact input and output data",
        };
        [
            mode.label().to_string(),
            description.to_string(),
            note.to_string(),
        ]
    });
    picker.table_lines(
        "Tool Inspector · ↑↓ move · →/enter use · esc close",
        rows,
        [(10, 10), (38, 38), (0, usize::MAX)],
        width,
    )
}

pub(crate) fn transcript_spacing_lines(picker: &ListPicker, width: usize) -> Vec<Line<'static>> {
    let current = transcript_spacing();
    let rows = TranscriptSpacing::ALL.iter().map(|spacing| {
        let note = if *spacing == current { "current" } else { "" };
        let description = match spacing {
            TranscriptSpacing::Compact => "no blank rows between sections",
            TranscriptSpacing::Comfortable => "one blank row between sections",
        };
        [
            spacing.label().to_string(),
            description.to_string(),
            note.to_string(),
        ]
    });
    picker.table_lines(
        "Transcript spacing · ↑↓ move · →/enter use · esc close",
        rows,
        [(14, 14), (36, 36), (0, usize::MAX)],
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
        "Saved approvals (this workspace) · ↑↓ move · →/enter revoke · esc close",
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
        [
            spec.name.to_string(),
            state.to_string(),
            spec.description.to_string(),
        ]
    });
    picker.table_lines(
        "Extensions · ↑↓ move · →/enter toggle · esc close",
        rows,
        [(12, 12), (3, 3), (0, usize::MAX)],
        width,
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
        [
            session.meta.id.clone(),
            age_label(session.meta.created_at),
            format!("{}{note}", session.meta.model),
        ]
    });
    picker.windowed_table_lines(
        "Sessions (this workspace) · ↑↓ move · PgUp/PgDn page · →/enter resume · esc close",
        rows,
        [(36, 36), (8, 8), (0, usize::MAX)],
        width,
        SESSIONS_WINDOW,
    )
}

pub(crate) fn api_key_lines(provider: Provider, input: &str) -> Vec<Line<'static>> {
    let t = theme();
    let mut tray = Section::tray(
        format!(
            "{} API key (saved for future sessions) · enter confirm · esc close",
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
