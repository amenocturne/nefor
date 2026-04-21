//! Crossterm input → NCP event body translation.
//!
//! Pure functions (no IO) so we can unit-test the edge cases that drove the
//! pre-refactor bug inventory: shift + letter, arrow keys, function keys,
//! bracketed paste, mouse, resize.
//!
//! The functions here produce a `serde_json::Map` body; the caller wraps
//! that in `PluginOutgoing::event(body)` with a `kind` field already set.

use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MediaKeyCode, ModifierKeyCode, MouseButton, MouseEvent,
    MouseEventKind,
};
use serde_json::{json, Map, Value};

/// Event body for `nefor-tui.input.key`. Returns `None` for key events we
/// don't forward (release/repeat kinds, pure modifier-only presses).
pub fn key_body(evt: &KeyEvent) -> Option<Map<String, Value>> {
    use crossterm::event::KeyEventKind;
    if evt.kind != KeyEventKind::Press {
        return None;
    }

    let (key_name, shift_already_applied) = key_code_name(evt.code)?;
    let mut modifiers: Vec<&'static str> = Vec::new();
    if evt.modifiers.contains(KeyModifiers::SHIFT) {
        modifiers.push("shift");
    }
    if evt.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers.push("ctrl");
    }
    if evt.modifiers.contains(KeyModifiers::ALT) {
        modifiers.push("alt");
    }
    if evt.modifiers.contains(KeyModifiers::SUPER) {
        modifiers.push("super");
    }

    // Shift + letter → uppercase. Crossterm on many terminals delivers
    // KeyCode::Char('a') with SHIFT; we map that to "A". We still keep
    // "shift" in modifiers so downstream consumers that care about the
    // distinction (ctrl-shift-a vs ctrl-A) can tell.
    let key = if !shift_already_applied && evt.modifiers.contains(KeyModifiers::SHIFT) {
        shift_letter(&key_name)
    } else {
        key_name
    };

    let mut body = Map::new();
    body.insert("kind".into(), Value::String("nefor-tui.input.key".into()));
    body.insert("key".into(), Value::String(key));
    body.insert(
        "modifiers".into(),
        Value::Array(
            modifiers
                .into_iter()
                .map(|s| Value::String(s.into()))
                .collect(),
        ),
    );
    Some(body)
}

/// Named main key for a crossterm [`KeyCode`]. Returns `(name, shift_already_applied)`:
/// the second tuple element is `true` when the name already reflects shift
/// semantics (e.g. a shifted symbol like `"!"` came through as `Char('!')`
/// with SHIFT), so the caller should NOT re-uppercase.
///
/// Returns `None` for modifier-only presses, which we suppress: the event
/// contract prefers one logical key per `input.key`.
fn key_code_name(code: KeyCode) -> Option<(String, bool)> {
    let name = match code {
        KeyCode::Char(c) => {
            // Ambiguous case: Char('A') can come from shift+a on some
            // terminals *without* SHIFT in modifiers, and from pure
            // Char('A') on others with SHIFT. We treat any alphabetic char
            // we see as "already the final form" — the caller's shift
            // logic only re-uppercases when the code is an ASCII lower
            // letter.
            let s = c.to_string();
            let shift_applied = !c.is_ascii_lowercase();
            return Some((s, shift_applied));
        }
        KeyCode::Enter => "enter",
        KeyCode::Tab => "tab",
        KeyCode::BackTab => "backtab",
        KeyCode::Backspace => "backspace",
        KeyCode::Esc => "escape",
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "pageup",
        KeyCode::PageDown => "pagedown",
        KeyCode::Delete => "delete",
        KeyCode::Insert => "insert",
        KeyCode::F(n) => return Some((format!("f{n}"), true)),
        KeyCode::Null => "null",
        KeyCode::CapsLock => "capslock",
        KeyCode::ScrollLock => "scrolllock",
        KeyCode::NumLock => "numlock",
        KeyCode::PrintScreen => "printscreen",
        KeyCode::Pause => "pause",
        KeyCode::Menu => "menu",
        KeyCode::KeypadBegin => "keypad_begin",
        KeyCode::Media(m) => return Some((media_name(m).to_string(), true)),
        KeyCode::Modifier(ModifierKeyCode::LeftShift)
        | KeyCode::Modifier(ModifierKeyCode::RightShift)
        | KeyCode::Modifier(ModifierKeyCode::LeftControl)
        | KeyCode::Modifier(ModifierKeyCode::RightControl)
        | KeyCode::Modifier(ModifierKeyCode::LeftAlt)
        | KeyCode::Modifier(ModifierKeyCode::RightAlt)
        | KeyCode::Modifier(ModifierKeyCode::LeftSuper)
        | KeyCode::Modifier(ModifierKeyCode::RightSuper)
        | KeyCode::Modifier(ModifierKeyCode::LeftHyper)
        | KeyCode::Modifier(ModifierKeyCode::RightHyper)
        | KeyCode::Modifier(ModifierKeyCode::LeftMeta)
        | KeyCode::Modifier(ModifierKeyCode::RightMeta)
        | KeyCode::Modifier(ModifierKeyCode::IsoLevel3Shift)
        | KeyCode::Modifier(ModifierKeyCode::IsoLevel5Shift) => return None,
    };
    Some((name.to_string(), true))
}

fn media_name(m: MediaKeyCode) -> &'static str {
    match m {
        MediaKeyCode::Play => "media_play",
        MediaKeyCode::Pause => "media_pause",
        MediaKeyCode::PlayPause => "media_play_pause",
        MediaKeyCode::Reverse => "media_reverse",
        MediaKeyCode::Stop => "media_stop",
        MediaKeyCode::FastForward => "media_fast_forward",
        MediaKeyCode::Rewind => "media_rewind",
        MediaKeyCode::TrackNext => "media_next",
        MediaKeyCode::TrackPrevious => "media_previous",
        MediaKeyCode::Record => "media_record",
        MediaKeyCode::LowerVolume => "media_volume_down",
        MediaKeyCode::RaiseVolume => "media_volume_up",
        MediaKeyCode::MuteVolume => "media_volume_mute",
    }
}

fn shift_letter(s: &str) -> String {
    if s.len() == 1 {
        // ASCII ASCII single-char optimisation.
        let c = s.chars().next().expect("len == 1");
        if c.is_ascii_lowercase() {
            return c.to_ascii_uppercase().to_string();
        }
    }
    // Non-ASCII: full uppercase transform. `to_uppercase` handles most
    // Unicode cases; acceptable for chat-style input.
    s.to_uppercase()
}

/// Event body for `nefor-tui.input.paste` from a bracketed-paste event.
pub fn paste_body(text: &str) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("kind".into(), Value::String("nefor-tui.input.paste".into()));
    body.insert("text".into(), Value::String(text.to_owned()));
    body
}

/// Event body for `nefor-tui.input.resize`.
pub fn resize_body(cols: u16, rows: u16) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert(
        "kind".into(),
        Value::String("nefor-tui.input.resize".into()),
    );
    body.insert("cols".into(), Value::Number(u32::from(cols).into()));
    body.insert("rows".into(), Value::Number(u32::from(rows).into()));
    body
}

/// Event body for `nefor-tui.ready`.
pub fn ready_body(cols: u16, rows: u16) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("kind".into(), Value::String("nefor-tui.ready".into()));
    body.insert("cols".into(), Value::Number(u32::from(cols).into()));
    body.insert("rows".into(), Value::Number(u32::from(rows).into()));
    body
}

/// Event body for `nefor-tui.input.mouse`. Returns `None` when the mouse
/// event is one we don't forward (e.g. `Moved` without a button).
pub fn mouse_body(evt: &MouseEvent) -> Option<Map<String, Value>> {
    let (action, button): (&'static str, Option<&'static str>) = match evt.kind {
        MouseEventKind::Down(b) => ("down", Some(button_name(b))),
        MouseEventKind::Up(b) => ("up", Some(button_name(b))),
        MouseEventKind::Drag(b) => ("drag", Some(button_name(b))),
        MouseEventKind::Moved => return None,
        MouseEventKind::ScrollDown => ("scroll_down", None),
        MouseEventKind::ScrollUp => ("scroll_up", None),
        MouseEventKind::ScrollLeft => ("scroll_left", None),
        MouseEventKind::ScrollRight => ("scroll_right", None),
    };

    let mut modifiers: Vec<&'static str> = Vec::new();
    if evt.modifiers.contains(KeyModifiers::SHIFT) {
        modifiers.push("shift");
    }
    if evt.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers.push("ctrl");
    }
    if evt.modifiers.contains(KeyModifiers::ALT) {
        modifiers.push("alt");
    }
    if evt.modifiers.contains(KeyModifiers::SUPER) {
        modifiers.push("super");
    }

    let mut body = Map::new();
    body.insert("kind".into(), Value::String("nefor-tui.input.mouse".into()));
    body.insert("action".into(), Value::String(action.into()));
    if let Some(b) = button {
        body.insert("button".into(), Value::String(b.into()));
    }
    body.insert("row".into(), json!(evt.row));
    body.insert("col".into(), json!(evt.column));
    body.insert(
        "modifiers".into(),
        Value::Array(
            modifiers
                .into_iter()
                .map(|s| Value::String(s.into()))
                .collect(),
        ),
    );
    Some(body)
}

fn button_name(b: MouseButton) -> &'static str {
    match b {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn release(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn plain_letter_is_lowercase() {
        let body = key_body(&press(KeyCode::Char('a'), KeyModifiers::NONE)).expect("forwarded");
        assert_eq!(body["key"], Value::String("a".into()));
        assert_eq!(body["modifiers"], json!([]));
    }

    #[test]
    fn shift_plus_letter_is_uppercase_with_shift_modifier() {
        let body = key_body(&press(KeyCode::Char('a'), KeyModifiers::SHIFT)).expect("forwarded");
        assert_eq!(body["key"], Value::String("A".into()));
        assert_eq!(body["modifiers"], json!(["shift"]));
    }

    #[test]
    fn already_uppercase_char_with_shift_stays_uppercase() {
        // Some terminals deliver Char('A') directly with SHIFT.
        let body = key_body(&press(KeyCode::Char('A'), KeyModifiers::SHIFT)).expect("forwarded");
        assert_eq!(body["key"], Value::String("A".into()));
        assert!(body["modifiers"]
            .as_array()
            .unwrap()
            .contains(&Value::String("shift".into())));
    }

    #[test]
    fn ctrl_letter_stays_lowercase_with_ctrl_modifier() {
        let body = key_body(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)).expect("forwarded");
        assert_eq!(body["key"], Value::String("c".into()));
        assert_eq!(body["modifiers"], json!(["ctrl"]));
    }

    #[test]
    fn ctrl_shift_letter_is_uppercase_with_both() {
        let body = key_body(&press(
            KeyCode::Char('a'),
            KeyModifiers::SHIFT | KeyModifiers::CONTROL,
        ))
        .expect("forwarded");
        assert_eq!(body["key"], Value::String("A".into()));
        // ordering: shift first, ctrl second
        assert_eq!(body["modifiers"], json!(["shift", "ctrl"]));
    }

    #[test]
    fn named_keys_have_descriptive_strings() {
        for (code, expected) in [
            (KeyCode::Enter, "enter"),
            (KeyCode::Esc, "escape"),
            (KeyCode::Backspace, "backspace"),
            (KeyCode::Tab, "tab"),
            (KeyCode::Left, "left"),
            (KeyCode::Right, "right"),
            (KeyCode::Up, "up"),
            (KeyCode::Down, "down"),
            (KeyCode::PageUp, "pageup"),
            (KeyCode::PageDown, "pagedown"),
            (KeyCode::Home, "home"),
            (KeyCode::End, "end"),
            (KeyCode::Delete, "delete"),
            (KeyCode::Insert, "insert"),
        ] {
            let body = key_body(&press(code, KeyModifiers::NONE)).expect("forwarded");
            assert_eq!(
                body["key"],
                Value::String(expected.into()),
                "unexpected name for {code:?}"
            );
        }
    }

    #[test]
    fn function_keys_named() {
        for n in 1..=12 {
            let body = key_body(&press(KeyCode::F(n), KeyModifiers::NONE)).expect("forwarded");
            assert_eq!(body["key"], Value::String(format!("f{n}")));
        }
    }

    #[test]
    fn release_event_is_dropped() {
        assert!(key_body(&release(KeyCode::Char('a'), KeyModifiers::NONE)).is_none());
    }

    #[test]
    fn pure_modifier_press_is_dropped() {
        use crossterm::event::ModifierKeyCode;
        assert!(key_body(&press(
            KeyCode::Modifier(ModifierKeyCode::LeftShift),
            KeyModifiers::SHIFT
        ))
        .is_none());
    }

    #[test]
    fn paste_body_is_plain_text() {
        let b = paste_body("hello\nworld");
        assert_eq!(b["kind"], Value::String("nefor-tui.input.paste".into()));
        assert_eq!(b["text"], Value::String("hello\nworld".into()));
    }

    #[test]
    fn resize_body_carries_dims() {
        let b = resize_body(100, 42);
        assert_eq!(b["cols"], json!(100));
        assert_eq!(b["rows"], json!(42));
    }

    #[test]
    fn ready_body_shape() {
        let b = ready_body(80, 24);
        assert_eq!(b["kind"], Value::String("nefor-tui.ready".into()));
        assert_eq!(b["cols"], json!(80));
        assert_eq!(b["rows"], json!(24));
    }

    #[test]
    fn mouse_down_left() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let evt = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let b = mouse_body(&evt).expect("forwarded");
        assert_eq!(b["action"], Value::String("down".into()));
        assert_eq!(b["button"], Value::String("left".into()));
        assert_eq!(b["row"], json!(3));
        assert_eq!(b["col"], json!(5));
    }

    #[test]
    fn mouse_moved_is_dropped() {
        let evt = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(mouse_body(&evt).is_none());
    }

    #[test]
    fn mouse_scroll_has_no_button() {
        let evt = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        let b = mouse_body(&evt).expect("forwarded");
        assert!(b.get("button").is_none());
        assert_eq!(b["action"], Value::String("scroll_down".into()));
    }
}
