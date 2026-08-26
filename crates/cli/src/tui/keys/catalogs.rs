use super::effects::After;
use super::*;

pub(super) fn handle_help_key(
    filter: &mut String,
    picker: &mut ListPicker,
    code: KeyCode,
) -> After {
    match code {
        KeyCode::PageUp => {
            picker.move_by(-(PICKER_ROWS as isize));
            After::Nothing
        }
        KeyCode::PageDown => {
            picker.move_by(PICKER_ROWS as isize);
            After::Nothing
        }
        _ => match picker.on_key(code) {
            PickerEvent::Activated(index) => {
                let commands = filter_commands(filter);
                let spec = commands[index];
                let suffix = if spec.takes_args { " " } else { "" };
                After::CloseAndCompose(format!("/{}{suffix}", spec.name))
            }
            PickerEvent::Ignored => {
                match code {
                    KeyCode::Char(c) => filter.push(c),
                    KeyCode::Backspace => {
                        filter.pop();
                    }
                    _ => return After::Nothing,
                }
                *picker = ListPicker::new(filter_commands(filter).len());
                After::Nothing
            }
            _ => After::Nothing,
        },
    }
}

pub(super) fn handle_model_key(picker: &mut ModelPicker, code: KeyCode) -> After {
    match code {
        KeyCode::PageUp => {
            picker.picker.move_by(-(PICKER_ROWS as isize));
            After::Nothing
        }
        KeyCode::PageDown => {
            picker.picker.move_by(PICKER_ROWS as isize);
            After::Nothing
        }
        _ => match picker.picker.on_key(code) {
            PickerEvent::Activated(_) => match picker.selected_model() {
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
            PickerEvent::Ignored => {
                match code {
                    KeyCode::Char(c) => picker.filter.push(c),
                    KeyCode::Backspace => {
                        picker.filter.pop();
                    }
                    _ => return After::Nothing,
                }
                picker.reset_filtered_selection();
                After::Nothing
            }
            _ => After::Nothing,
        },
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
