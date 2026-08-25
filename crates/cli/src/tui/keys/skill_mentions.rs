use crossterm::event::KeyCode;

use crate::tui::components::picker::PickerEvent;

use super::super::format::byte_index;
use super::super::state::SkillMentionPicker;
use super::super::PICKER_ROWS;
use super::effects::After;

pub(super) fn handle_skill_mention_key(
    skill: &mut SkillMentionPicker,
    composer: &mut String,
    cursor: &mut usize,
    code: KeyCode,
) -> After {
    if code == KeyCode::Tab {
        return selected(skill);
    }
    if code == KeyCode::PageUp {
        skill.picker.move_by(-(PICKER_ROWS as isize));
        return After::Nothing;
    }
    if code == KeyCode::PageDown {
        skill.picker.move_by(PICKER_ROWS as isize);
        return After::Nothing;
    }
    match skill.picker.on_key(code) {
        PickerEvent::Activated(_) => selected(skill),
        PickerEvent::Moved | PickerEvent::Action { .. } => After::Nothing,
        PickerEvent::Ignored => match code {
            KeyCode::Char(c) => {
                skill.query.push(c);
                let at = byte_index(composer, *cursor);
                composer.insert(at, c);
                *cursor += 1;
                skill.sync_len();
                After::Nothing
            }
            KeyCode::Backspace if !skill.query.is_empty() => {
                skill.query.pop();
                let at = byte_index(composer, *cursor - 1);
                composer.remove(at);
                *cursor -= 1;
                skill.sync_len();
                After::Nothing
            }
            KeyCode::Backspace | KeyCode::Delete => {
                let at = byte_index(composer, skill.token_start);
                composer.remove(at);
                *cursor = skill.token_start;
                After::Close
            }
            _ => After::Nothing,
        },
    }
}

fn selected(skill: &SkillMentionPicker) -> After {
    match skill.selected() {
        Some(name) => After::InsertSkill {
            token_start: skill.token_start,
            name,
        },
        None => After::Nothing,
    }
}
