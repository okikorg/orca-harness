//! What each `/settings` row opens.

use super::super::effects::After;
use super::super::*;
use crate::msg::ProviderExt as _;

/// The values the rows open on, read before the overlay is borrowed.
pub(super) struct Current {
    pub provider: Provider,
    pub view: ViewMode,
    pub inspector: InspectorMode,
    pub spacing: TranscriptSpacing,
    pub style: UiStyle,
    pub workspace_root: String,
}

pub(super) fn open_row(row: usize, now: &Current) -> After {
    match row {
        0 => After::Push(Overlay::Providers(ProviderPicker::new(Some(now.provider)))),
        1 => After::FetchModels,
        2 => {
            let current = view::theme_name();
            let selected = view::ThemeName::ALL
                .iter()
                .position(|name| *name == current)
                .unwrap_or(0);
            After::Push(Overlay::Themes {
                picker: ListPicker::with_selected(view::ThemeName::ALL.len(), selected),
            })
        }
        3 => {
            let selected = ViewMode::ALL
                .iter()
                .position(|mode| *mode == now.view)
                .unwrap_or(0);
            After::Push(Overlay::Views {
                picker: ListPicker::with_selected(ViewMode::ALL.len(), selected),
            })
        }
        4 => {
            let selected = InspectorMode::ALL
                .iter()
                .position(|mode| *mode == now.inspector)
                .unwrap_or(0);
            After::Push(Overlay::Inspector {
                picker: ListPicker::with_selected(InspectorMode::ALL.len(), selected),
            })
        }
        5 => {
            if now.provider.key_env().is_none() {
                After::CloseWithNote(format!(
                    "the {} endpoint needs no api key",
                    now.provider.label()
                ))
            } else {
                After::Push(Overlay::ApiKey {
                    provider: now.provider,
                    input: String::new(),
                })
            }
        }
        6 => {
            let tools = crate::config::stored_approvals(&now.workspace_root);
            if tools.is_empty() {
                After::CloseWithNote("no saved approvals for this workspace".into())
            } else {
                After::Push(Overlay::Approvals {
                    picker: ListPicker::new(tools.len()),
                    tools,
                })
            }
        }
        7 => {
            let selected = TranscriptSpacing::ALL
                .iter()
                .position(|spacing| *spacing == now.spacing)
                .unwrap_or(1);
            After::Push(Overlay::TranscriptSpacing {
                picker: ListPicker::with_selected(TranscriptSpacing::ALL.len(), selected),
            })
        }
        8 => {
            let selected = UiStyle::ALL
                .iter()
                .position(|style| *style == now.style)
                .unwrap_or(0);
            After::Push(Overlay::Style {
                picker: ListPicker::with_selected(UiStyle::ALL.len(), selected),
            })
        }
        9 => {
            let current = crate::view::glyphs::mark_shape();
            let selected = crate::view::glyphs::MarkShape::ALL
                .iter()
                .position(|shape| *shape == current)
                .unwrap_or(0);
            After::Push(Overlay::Marks {
                picker: ListPicker::with_selected(
                    crate::view::glyphs::MarkShape::ALL.len(),
                    selected,
                ),
            })
        }
        _ => {
            let current = crate::view::glyphs::branch_shape();
            let selected = crate::view::glyphs::BranchShape::ALL
                .iter()
                .position(|shape| *shape == current)
                .unwrap_or(0);
            After::Push(Overlay::Branches {
                picker: ListPicker::with_selected(
                    crate::view::glyphs::BranchShape::ALL.len(),
                    selected,
                ),
            })
        }
    }
}
