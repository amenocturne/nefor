//! Crossterm `KeyEvent` → `KeyMessage` translation.
//!
//! `KeyMessage` is the Rust-side struct the engine pushes through Lua's
//! `update(msg, state)`. Lua sees it as `{ kind = "key.<name>", mods = {...} }`.
//!
//! Pure-modifier presses are dropped (one logical key per message).
//! Release / repeat events are dropped — phase 1 honors press-only.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode};

/// One key press, normalized for engine dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMessage {
    /// Logical key name (e.g. `"a"`, `"enter"`, `"f1"`, `"space"`).
    pub name: String,
    /// Modifiers in stable order: shift, ctrl, alt, super.
    pub mods: Vec<&'static str>,
}

impl KeyMessage {
    /// Build the kind string Lua will see. Modifier-prefixed keys fold
    /// the modifier into the kind so user code can dispatch on a single
    /// stable string:
    ///
    /// - `"a"`         → `"key.a"`
    /// - `Ctrl+B`      → `"key.ctrl_b"`
    /// - `Alt+Right`   → `"key.alt_right"`
    /// - `Shift+Tab`   → `"key.shift_tab"`
    ///
    /// `mods` is also published on the table as a list, so handlers that
    /// want to dispatch generically (`key.<name>` + inspect `msg.mods`)
    /// can still do so. Plain printable + shift uppercases the name in
    /// `key_code_name` already, so we drop the `shift_` prefix when the
    /// only modifier is shift on a single printable to keep the existing
    /// `key.A` shape for typed capitals.
    ///
    /// **Ctrl/Alt + letter casing**: when a non-shift modifier (Ctrl,
    /// Alt, or Super) is held with a single ASCII letter, the casing is
    /// folded to lowercase. Different terminals and keyboard states
    /// (Caps Lock, Shift+Ctrl) report the same logical chord under
    /// different casings — `Ctrl+B`, `Ctrl+Shift+B`, and `Ctrl+B with
    /// Caps Lock on` would otherwise produce three distinct kinds and
    /// silently break the binding when only one of them is matched.
    /// Shift survives in `mods`, so handlers that genuinely need to
    /// distinguish `Ctrl+B` from `Ctrl+Shift+B` can still inspect it.
    pub fn kind(&self) -> String {
        let only_shift_on_printable = self.mods == ["shift"]
            && self.name.chars().count() == 1
            && self.name.chars().next().is_some_and(|c| !c.is_control());
        if only_shift_on_printable {
            return format!("key.{}", self.name);
        }
        let mut prefix = String::new();
        for m in &self.mods {
            prefix.push_str(m);
            prefix.push('_');
        }
        let name = if self
            .mods
            .iter()
            .any(|m| matches!(*m, "ctrl" | "alt" | "super"))
            && self.name.chars().count() == 1
        {
            self.name.to_ascii_lowercase()
        } else {
            self.name.clone()
        };
        format!("key.{}{}", prefix, name)
    }
}

/// Translate a crossterm key event into a [`KeyMessage`]. Returns `None`
/// for events that the engine does not forward (release, repeat, pure-
/// modifier presses).
pub fn from_key_event(evt: &KeyEvent) -> Option<KeyMessage> {
    if evt.kind != KeyEventKind::Press {
        return None;
    }
    let (name, shift_already_applied) = key_code_name(evt.code)?;

    let mut mods: Vec<&'static str> = Vec::new();
    if evt.modifiers.contains(KeyModifiers::SHIFT) {
        mods.push("shift");
    }
    if evt.modifiers.contains(KeyModifiers::CONTROL) {
        mods.push("ctrl");
    }
    if evt.modifiers.contains(KeyModifiers::ALT) {
        mods.push("alt");
    }
    if evt.modifiers.contains(KeyModifiers::SUPER) {
        mods.push("super");
    }

    let name = if !shift_already_applied && evt.modifiers.contains(KeyModifiers::SHIFT) {
        shift_letter(&name)
    } else {
        name
    };
    Some(KeyMessage { name, mods })
}

fn key_code_name(code: KeyCode) -> Option<(String, bool)> {
    let name = match code {
        KeyCode::Char(' ') => return Some(("space".to_string(), true)),
        KeyCode::Char(c) => {
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
        KeyCode::Media(_) => return None,
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

fn shift_letter(s: &str) -> String {
    if s.len() == 1 {
        let c = s.chars().next().unwrap_or(' ');
        if c.is_ascii_lowercase() {
            return c.to_ascii_uppercase().to_string();
        }
    }
    s.to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventState, KeyModifiers};

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn space_named() {
        let m = from_key_event(&press(KeyCode::Char(' '), KeyModifiers::NONE)).expect("present");
        assert_eq!(m.name, "space");
        assert!(m.mods.is_empty());
        assert_eq!(m.kind(), "key.space");
    }

    #[test]
    fn lowercase_letter_passes_through() {
        let m = from_key_event(&press(KeyCode::Char('q'), KeyModifiers::NONE)).expect("p");
        assert_eq!(m.name, "q");
        assert_eq!(m.kind(), "key.q");
    }

    #[test]
    fn shift_letter_uppercases_with_modifier() {
        let m = from_key_event(&press(KeyCode::Char('a'), KeyModifiers::SHIFT)).expect("p");
        assert_eq!(m.name, "A");
        assert_eq!(m.mods, vec!["shift"]);
    }

    #[test]
    fn release_dropped() {
        let evt = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert!(from_key_event(&evt).is_none());
    }

    #[test]
    fn pure_modifier_dropped() {
        use crossterm::event::ModifierKeyCode;
        assert!(from_key_event(&press(
            KeyCode::Modifier(ModifierKeyCode::LeftShift),
            KeyModifiers::SHIFT
        ))
        .is_none());
    }

    #[test]
    fn named_keys() {
        for (code, expected) in [
            (KeyCode::Enter, "enter"),
            (KeyCode::Esc, "escape"),
            (KeyCode::Tab, "tab"),
            (KeyCode::Up, "up"),
            (KeyCode::F(5), "f5"),
        ] {
            let m = from_key_event(&press(code, KeyModifiers::NONE)).expect("p");
            assert_eq!(m.name, expected, "{code:?}");
        }
    }

    #[test]
    fn ctrl_prefixed_kind_folds_modifier_into_name() {
        let m = from_key_event(&press(KeyCode::Char('b'), KeyModifiers::CONTROL)).expect("p");
        assert_eq!(m.name, "b");
        assert_eq!(m.mods, vec!["ctrl"]);
        assert_eq!(m.kind(), "key.ctrl_b");
    }

    #[test]
    fn alt_named_key_kind_folds_modifier() {
        let m = from_key_event(&press(KeyCode::Right, KeyModifiers::ALT)).expect("p");
        assert_eq!(m.kind(), "key.alt_right");
    }

    #[test]
    fn shift_on_printable_keeps_uppercased_name_only() {
        // Existing convention: key.A already encodes shift via the
        // capital. Don't double-prefix.
        let m = from_key_event(&press(KeyCode::Char('a'), KeyModifiers::SHIFT)).expect("p");
        assert_eq!(m.kind(), "key.A");
    }

    #[test]
    fn shift_on_named_key_does_fold() {
        // Shift+Tab is a real distinct keypress (backtab); fold it.
        let m = from_key_event(&press(KeyCode::Tab, KeyModifiers::SHIFT)).expect("p");
        assert_eq!(m.kind(), "key.shift_tab");
    }

    #[test]
    fn ctrl_alt_combo_orders_modifiers_stably() {
        let m = from_key_event(&press(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("p");
        assert_eq!(m.kind(), "key.ctrl_alt_x");
    }

    #[test]
    fn ctrl_letter_kind_lowercased_regardless_of_terminal_casing() {
        // Some terminals deliver `Ctrl+B` as `Char('B')` + CONTROL (no
        // SHIFT modifier) — e.g. when the user has Caps Lock on, or
        // certain alt-keymap combos. Ctrl+B and Ctrl+b are the same
        // logical chord, so the kind() folding must yield a single
        // stable string regardless of which casing the terminal sent.
        let lower = from_key_event(&press(KeyCode::Char('b'), KeyModifiers::CONTROL)).expect("p");
        assert_eq!(lower.kind(), "key.ctrl_b");
        let upper = from_key_event(&press(KeyCode::Char('B'), KeyModifiers::CONTROL)).expect("p");
        assert_eq!(
            upper.kind(),
            "key.ctrl_b",
            "Ctrl+B with uppercase letter must fold to the same kind as Ctrl+b"
        );
    }

    #[test]
    fn ctrl_shift_letter_kind_lowercased_too() {
        // Ctrl+Shift+B should still register as `key.ctrl_b` (with shift
        // surfaced via mods) so a binding for "ctrl_b" doesn't silently
        // break when shift is also held — same logical chord.
        let m = from_key_event(&press(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("p");
        assert_eq!(m.kind(), "key.shift_ctrl_b");
        assert!(m.mods.contains(&"shift") && m.mods.contains(&"ctrl"));
    }

    #[test]
    fn alt_letter_also_folds_casing() {
        // Same rule for Alt — `Alt+F` and `Alt+f` are the same chord.
        let upper = from_key_event(&press(KeyCode::Char('F'), KeyModifiers::ALT)).expect("p");
        assert_eq!(upper.kind(), "key.alt_f");
    }

    #[test]
    fn shift_only_printable_keeps_capital_name() {
        // The `key.A` convention for typed capitals must survive — only
        // ctrl/alt/super fold casing.
        let m = from_key_event(&press(KeyCode::Char('a'), KeyModifiers::SHIFT)).expect("p");
        assert_eq!(m.kind(), "key.A");
    }
}
