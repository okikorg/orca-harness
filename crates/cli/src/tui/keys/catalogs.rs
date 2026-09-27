use super::effects::After;
use super::*;
use crate::msg::{ProviderAuth, ProviderExt as _};

/// What a key did to a searchable picker.
enum Search {
    Activated(usize),
    /// The filter changed; the caller re-counts its matches.
    Edited,
    Nothing,
}

/// Paging, navigation and type-to-filter shared by every searchable picker.
fn search_key(picker: &mut ListPicker, filter: &mut String, code: KeyCode) -> Search {
    match code {
        KeyCode::PageUp => picker.move_by(-(PICKER_ROWS as isize)),
        KeyCode::PageDown => picker.move_by(PICKER_ROWS as isize),
        _ => match picker.on_key(code) {
            PickerEvent::Activated(index) => return Search::Activated(index),
            PickerEvent::Ignored => match code {
                KeyCode::Char(c) => {
                    filter.push(c);
                    return Search::Edited;
                }
                KeyCode::Backspace => {
                    filter.pop();
                    return Search::Edited;
                }
                _ => {}
            },
            _ => {}
        },
    }
    Search::Nothing
}

pub(super) fn handle_help_key(
    filter: &mut String,
    picker: &mut ListPicker,
    code: KeyCode,
) -> After {
    match search_key(picker, filter, code) {
        Search::Activated(index) => {
            let spec = filter_commands(filter)[index];
            let suffix = if spec.takes_args { " " } else { "" };
            After::CloseAndCompose(format!("/{}{suffix}", spec.name))
        }
        Search::Edited => {
            *picker = ListPicker::new(filter_commands(filter).len());
            After::Nothing
        }
        Search::Nothing => After::Nothing,
    }
}

pub(super) fn handle_model_key(picker: &mut ModelPicker, code: KeyCode) -> After {
    match search_key(&mut picker.picker, &mut picker.filter, code) {
        Search::Activated(_) => match picker.selected_model() {
            Some(model) if picker.subagent.is_some() => {
                let (tier, provider) = picker.subagent.clone().unwrap();
                After::CloseAndSend(WorkerCmd::SetSubagentModel {
                    tier,
                    provider,
                    model: model.id,
                })
            }
            Some(model) => match EffortPicker::new(model.clone()) {
                Some(efforts) => After::Push(Overlay::Efforts(efforts)),
                None => After::CloseAndSetModel {
                    id: model.id,
                    window: model.context_length,
                    reasoning_effort: None,
                },
            },
            None => After::Close,
        },
        Search::Edited => {
            picker.reset_filtered_selection();
            After::Nothing
        }
        Search::Nothing => After::Nothing,
    }
}

/// Switch provider, asking for a key or a login first when one is missing.
pub(super) fn handle_provider_key(picker: &mut ProviderPicker, code: KeyCode) -> After {
    match search_key(&mut picker.picker, &mut picker.filter, code) {
        Search::Activated(_) => match picker.selected() {
            Some(provider) => match provider.auth() {
                ProviderAuth::ApiKey { .. } if provider.resolve_key().is_none() => {
                    After::Push(Overlay::ApiKey {
                        provider,
                        input: String::new(),
                    })
                }
                ProviderAuth::OAuth => match crate::auth::status(provider) {
                    Ok(_) => After::CloseAndSend(WorkerCmd::SetProvider {
                        provider,
                        api_key: None,
                    }),
                    Err(_) => After::CloseAndSend(WorkerCmd::LoginProvider { provider }),
                },
                _ => After::CloseAndSend(WorkerCmd::SetProvider {
                    provider,
                    api_key: None,
                }),
            },
            None => After::Nothing,
        },
        Search::Edited => {
            picker.reset_filtered_selection();
            After::Nothing
        }
        Search::Nothing => After::Nothing,
    }
}

pub(super) fn handle_effort_key(picker: &mut EffortPicker, code: KeyCode) -> After {
    match picker.picker.on_key(code) {
        PickerEvent::Activated(_) => match picker.selected() {
            Some(reasoning_effort) => After::CloseAndSetModel {
                id: picker.model_id.clone(),
                window: picker.context_window,
                reasoning_effort: Some(reasoning_effort),
            },
            None => After::Nothing,
        },
        _ => After::Nothing,
    }
}
