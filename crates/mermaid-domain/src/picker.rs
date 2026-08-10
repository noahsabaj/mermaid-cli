//! The shared navigation core of every modal list.
//!
//! Four pickers (model, conversations, rewind, plan config) each hand-rolled
//! the same Up/Down/Enter/Escape state machine over a `cursor` and a row
//! count — the paste-into-the-file-picker bug (#350) came from exactly this
//! family of per-surface duplication. [`picker_step`] is that machine said
//! once: callers keep only their confirm semantics and any extra keys.

use crate::msg::KeyCode;

/// What one keystroke did to a picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerStep {
    /// The cursor moved (or clamped); nothing else to do.
    Moved,
    /// Enter on the row at this index — the caller acts on it.
    Confirm(usize),
    /// Escape — the caller closes the surface.
    Dismiss,
    /// Not a navigation key; the caller may handle it (query typing,
    /// value cycling) or swallow it.
    Other,
}

/// Advance `cursor` over a `len`-row list for one key. Up saturates at the
/// top, Down clamps to the last row (an empty list pins the cursor at 0),
/// Enter confirms the current row, Escape dismisses; anything else is
/// [`PickerStep::Other`].
pub fn picker_step(code: KeyCode, cursor: &mut usize, len: usize) -> PickerStep {
    match code {
        KeyCode::Up => {
            *cursor = cursor.saturating_sub(1);
            PickerStep::Moved
        },
        KeyCode::Down => {
            let max = len.saturating_sub(1);
            if *cursor < max {
                *cursor += 1;
            }
            PickerStep::Moved
        },
        KeyCode::Enter => PickerStep::Confirm(*cursor),
        KeyCode::Escape => PickerStep::Dismiss,
        _ => PickerStep::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_saturates_at_both_ends() {
        let mut cursor = 0;
        assert_eq!(picker_step(KeyCode::Up, &mut cursor, 3), PickerStep::Moved);
        assert_eq!(cursor, 0, "Up at the top stays put");
        cursor = 2;
        assert_eq!(
            picker_step(KeyCode::Down, &mut cursor, 3),
            PickerStep::Moved
        );
        assert_eq!(cursor, 2, "Down at the bottom stays put");
        cursor = 0;
        picker_step(KeyCode::Down, &mut cursor, 3);
        assert_eq!(cursor, 1);
    }

    #[test]
    fn empty_list_pins_the_cursor() {
        let mut cursor = 0;
        picker_step(KeyCode::Down, &mut cursor, 0);
        assert_eq!(cursor, 0);
        assert_eq!(
            picker_step(KeyCode::Enter, &mut cursor, 0),
            PickerStep::Confirm(0),
            "confirm on empty still reports the index; callers get(i) safely"
        );
    }

    #[test]
    fn confirm_dismiss_and_other_pass_through() {
        let mut cursor = 1;
        assert_eq!(
            picker_step(KeyCode::Enter, &mut cursor, 3),
            PickerStep::Confirm(1)
        );
        assert_eq!(
            picker_step(KeyCode::Escape, &mut cursor, 3),
            PickerStep::Dismiss
        );
        assert_eq!(
            picker_step(KeyCode::Char('x'), &mut cursor, 3),
            PickerStep::Other
        );
    }
}
