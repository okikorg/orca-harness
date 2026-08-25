use crossterm::event::KeyCode;

use crate::tui::components::picker::PickerEvent;

use super::super::format::byte_index;
use super::super::state::LocationPicker;
use super::effects::After;

pub(super) fn handle_location_mention_key(
    location: &mut LocationPicker,
    composer: &mut String,
    cursor: &mut usize,
    code: KeyCode,
) -> After {
    if code == KeyCode::Tab {
        return selected(location);
    }
    match location.picker.on_key(code) {
        PickerEvent::Activated(_) => selected(location),
        PickerEvent::Moved | PickerEvent::Action { .. } => After::Nothing,
        PickerEvent::Ignored => match code {
            KeyCode::Char(c) => {
                location.query.push(c);
                let at = byte_index(composer, *cursor);
                composer.insert(at, c);
                *cursor += 1;
                location.sync_len();
                After::Nothing
            }
            KeyCode::Backspace if !location.query.is_empty() => {
                location.query.pop();
                let at = byte_index(composer, *cursor - 1);
                composer.remove(at);
                *cursor -= 1;
                location.sync_len();
                After::Nothing
            }
            KeyCode::Backspace | KeyCode::Delete => {
                let at = byte_index(composer, location.token_start);
                composer.remove(at);
                *cursor = location.token_start;
                After::Close
            }
            _ => After::Nothing,
        },
    }
}

fn selected(location: &LocationPicker) -> After {
    match location.selected() {
        Some(entry) => After::InsertLocation {
            token_start: location.token_start,
            entry,
        },
        None => After::Nothing,
    }
}
