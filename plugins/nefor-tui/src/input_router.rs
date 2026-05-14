//! Input router — keys to focused text_input vs bubble to Lua.
//!
//! Browser-style policy from spec §"Input routing model":
//!
//! - Engine inspects the description tree once per reconcile and caches
//!   the path to the first `focused = true` text_input (by tree order).
//!   Subsequent focused inputs are user error: a `tracing::warn!` fires.
//! - On each key event, the router asks: is this an editing key (printable
//!   char, backspace, delete, arrows, home/end, enter w/o shift, ctrl+a,
//!   ctrl+v, ctrl+z, ctrl+y, shift+enter when multi-line)?
//!   If yes AND a focused text_input exists → route to it; otherwise
//!   bubble to Lua's `update` as a `key.<name>` message.
//!
//! Ctrl+C bubbles unconditionally — universally "exit/cancel" in raw-mode
//! terminals. The desktop "Ctrl+C = copy" convention does not apply here
//! (no system-clipboard integration in v1, and a text_input that swallows
//! Ctrl+C strands the user with no way out of the app).
//!
//! Modifier-prefixed keys (Ctrl+B, Esc, Tab, F-keys, PgUp/PgDn) and
//! release/repeat events ALWAYS bubble — even when a text_input is
//! focused. This is the same shape as a focused `<input>` in a browser.

use crate::desc::WidgetDescription;
use crate::input::KeyMessage;
use crate::instance::{InstanceKind, InstanceState, WidgetInstance};
use crate::text_input::{
    allows_newline_insert, cursor_in_wrap_for, line_end, wrap_value, EditOutcome, TextInputState,
};

/// Outcome the engine acts on for a single key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// Bubble the key to Lua as `{ kind = "key.<name>", mods = [...] }`.
    BubbleToLua,
    /// Route to the focused text_input. The router has already mutated
    /// the instance's state; the engine should then dispatch any
    /// resulting `on_change` / `on_submit` callback messages to Lua.
    HandledByTextInput {
        /// User key on the focused text_input (always set — input router
        /// only marks an input as focused when its key is present).
        target_key: String,
        /// Stable msg-kind from the description's `on_change`, if any.
        on_change: Option<String>,
        /// Stable msg-kind from the description's `on_submit`, if any
        /// (only set when Enter was pressed without Shift).
        on_submit: Option<String>,
        /// The current value the input now holds (post-mutation).
        value: String,
        /// `true` if `on_change` should fire (value changed).
        value_changed: bool,
        /// `true` if `on_submit` should fire (Enter w/o Shift on a
        /// focused input).
        submitted: bool,
    },
}

/// Walk the instance tree depth-first and return the path (sequence of
/// child indices from the root) to the first text_input whose
/// description carries `focused = true`. Logs a `tracing::warn!` for
/// every additional focused text_input encountered.
pub fn find_focused_path(root: &WidgetInstance) -> Option<Vec<usize>> {
    let mut path = Vec::new();
    let mut found: Option<Vec<usize>> = None;
    let mut extras = 0usize;
    walk_focused(root, &mut path, &mut found, &mut extras);
    if extras > 0 {
        tracing::warn!(
            extras,
            "multiple text_inputs declared focused = true; first by tree order wins"
        );
    }
    found
}

fn walk_focused(
    inst: &WidgetInstance,
    path: &mut Vec<usize>,
    found: &mut Option<Vec<usize>>,
    extras: &mut usize,
) {
    if matches!(inst.kind(), InstanceKind::TextInput) {
        if let WidgetDescription::TextInput { focused, key, .. } = &inst.last_desc {
            if *focused && key.is_some() {
                if found.is_none() {
                    *found = Some(path.clone());
                } else {
                    *extras += 1;
                }
            }
        }
    }
    for (i, child) in inst.children.iter().enumerate() {
        path.push(i);
        walk_focused(child, path, found, extras);
        path.pop();
    }
}

/// Reach into the tree following `path` and return a mutable reference
/// to the targeted instance.
pub fn instance_at_path<'a>(
    root: &'a mut WidgetInstance,
    path: &[usize],
) -> Option<&'a mut WidgetInstance> {
    let mut cur: &mut WidgetInstance = root;
    for &i in path {
        let child = cur.children.get_mut(i)?;
        cur = child;
    }
    Some(cur)
}

/// Whether a key with the given name + mods is an editing key the engine
/// absorbs into a focused text_input. Modifier-prefixed bindings reserved
/// for Lua-defined shortcuts (Ctrl+B, Ctrl+S, Esc, Tab, F-keys, etc.) and
/// release/repeat events always bubble.
pub fn is_editing_key(key: &KeyMessage, max_lines: u16) -> bool {
    let has_shift = key.mods.contains(&"shift");
    let has_ctrl = key.mods.contains(&"ctrl");
    let has_alt = key.mods.contains(&"alt");
    let has_super = key.mods.contains(&"super");
    let solo_modifier = !has_ctrl && !has_alt && !has_super;

    match key.name.as_str() {
        // Plain Backspace and Alt+Backspace (delete-word-back) both edit;
        // Ctrl+Backspace stays free for Lua shortcuts.
        "backspace" => !has_ctrl && !has_super,
        // Plain Delete and Alt+Delete (delete-word-forward) both edit;
        // Ctrl+Delete stays free.
        "delete" => !has_ctrl && !has_super,
        // Plain arrows + Alt+arrows (word-left/right). Ctrl+arrow and
        // Super stay free.
        "left" | "right" => !has_ctrl && !has_super,
        "up" | "down" | "home" | "end" => !has_alt && !has_super,
        "enter" => {
            // Enter without modifiers → submit. Shift+Enter inserts a
            // newline only on multi-line inputs. Ctrl/Alt/Super+Enter
            // bubble (user shortcut).
            if has_ctrl || has_alt || has_super {
                false
            } else if has_shift {
                allows_newline_insert(max_lines)
            } else {
                true
            }
        }
        // Ctrl+C bubbles to Lua as the universal "exit/cancel" gesture
        // for terminal apps; absorbing it here would strand the user.
        // The remaining readline shortcuts (Ctrl+A/E/U/K/W) absorb
        // here so the editor handles them inline.
        "a" | "e" | "u" | "k" | "w" | "v" | "z" | "y" if has_ctrl && !has_alt && !has_super => true,
        "space" => solo_modifier,
        // Single-char printable: route as text input. Names from
        // `from_key_event` are e.g. "a", "A", "1", "?". Excludes named
        // multi-char keys above.
        name => {
            if has_ctrl || has_alt || has_super {
                return false;
            }
            // Printable single grapheme: any name with exactly one
            // user-visible character (chars().count() == 1) that is not
            // a control character.
            let mut chars = name.chars();
            let Some(c) = chars.next() else { return false };
            if chars.next().is_some() {
                return false;
            }
            !c.is_control()
        }
    }
}

/// Whether an Up/Down arrow should bubble to Lua instead of moving the
/// cursor inside a focused text_input. Mac keyboards lack PgUp/PgDn, so
/// the chat surface remaps Up/Down to scroll the active surface — but
/// only when the input has nowhere to move the cursor. Rules:
///
/// - Single-line input (`max_lines == 1`): Up and Down always bubble.
///   Cursor movement on a single visual row would be a no-op, and the
///   user's intent is "scroll the transcript".
/// - Multi-line input on Up: bubble when the cursor sits on the first
///   visual row (soft-wrapped or hard-newline rows count). Else absorb
///   so the cursor walks up.
/// - Multi-line input on Down: bubble when the cursor sits on the last
///   visual row. Else absorb.
///
/// Shift+Up / Shift+Down follow the same rule — cursor-extension
/// selection at a row edge has nowhere to go, so bubbling lets the
/// surface scroll instead. Documented at the call site.
pub fn arrow_should_bubble(state: &TextInputState, key_name: &str, max_lines: u16) -> bool {
    if max_lines <= 1 {
        return true;
    }
    let value = state.last_value.as_str();
    let cursor = state.cursor.min(value.len());
    let (visual_row, last_row) = if state.viewport_width > 0 {
        let rows = wrap_value(value, state.viewport_width);
        if rows.is_empty() {
            return true;
        }
        let (row, _) = cursor_in_wrap_for(value, &rows, cursor);
        (row, rows.len().saturating_sub(1))
    } else {
        // Hard-newline fallback before the first layout pass: count
        // logical rows by scanning `\n`s and locate the cursor's row.
        let mut row = 0usize;
        for &b in &value.as_bytes()[..cursor] {
            if b == b'\n' {
                row += 1;
            }
        }
        let total = value.split('\n').count().saturating_sub(1);
        (row, total)
    };
    match key_name {
        "up" => visual_row == 0,
        "down" => {
            if state.viewport_width > 0 {
                visual_row >= last_row
            } else {
                // Hard-newline fallback: cursor must be on the last
                // logical line AND that line must have no trailing
                // newline (else the cursor could land on the empty post-
                // newline row).
                let line_e = line_end(value, cursor);
                visual_row >= last_row && line_e == value.len()
            }
        }
        _ => false,
    }
}

/// Apply an editing key to `state`, given the description's bookkeeping.
/// Returns the `EditOutcome` so the caller can fire `on_change`/`on_submit`.
///
/// Clears `state.manual_scroll` up front: any editing key (cursor move
/// or content mutation) is a "user wants to see the cursor again" signal,
/// so the auto-pin re-engages on the next layout pass. Mirrors the
/// scrollable container's `was_at_end` flip on user gesture — a one-bit
/// latch that the auto-pin checks before stealing the viewport.
pub fn apply_editing_key(
    state: &mut TextInputState,
    key: &KeyMessage,
    max_lines: u16,
) -> EditOutcome {
    let has_shift = key.mods.contains(&"shift");
    let has_ctrl = key.mods.contains(&"ctrl");
    let has_alt = key.mods.contains(&"alt");
    state.manual_scroll = false;

    match key.name.as_str() {
        "backspace" if has_alt => state.delete_word_backward(),
        "backspace" => state.backspace(),
        "delete" if has_alt => state.delete_word_forward(),
        "delete" => state.delete_forward(),
        "left" if has_alt => state.move_word_left(has_shift),
        "left" => state.move_left(has_shift),
        "right" if has_alt => state.move_word_right(has_shift),
        "right" => state.move_right(has_shift),
        "up" => state.move_up(has_shift),
        "down" => state.move_down(has_shift),
        "home" => state.move_to_line_start(has_shift),
        "end" => state.move_to_line_end(has_shift),
        "enter" => {
            if has_shift && allows_newline_insert(max_lines) {
                state.insert_char('\n')
            } else {
                // Submit: do NOT modify the value.
                EditOutcome {
                    new_value: None,
                    submitted: true,
                }
            }
        }
        "a" if has_ctrl => state.move_to_line_start(false),
        "e" if has_ctrl => state.move_to_line_end(false),
        "u" if has_ctrl => state.delete_to_line_start(),
        "k" if has_ctrl => state.delete_to_line_end(),
        "w" if has_ctrl => state.delete_word_backward(),
        "v" if has_ctrl => {
            // Paste: same shape — terminal-driven bracketed paste lives
            // outside the editing-key path. v1 no-op.
            EditOutcome::default()
        }
        "z" if has_ctrl => state.undo(),
        "y" if has_ctrl => state.redo(),
        "space" => state.insert_char(' '),
        name => {
            // Single printable.
            let mut iter = name.chars();
            if let (Some(c), None) = (iter.next(), iter.next()) {
                if !c.is_control() {
                    return state.insert_char(c);
                }
            }
            EditOutcome::default()
        }
    }
}

/// Outcome for a bracketed-paste event. Mirrors `RouteDecision` but
/// carries no `submitted` slot — paste never submits, even when the
/// pasted text contains an Enter (the terminal converts the LF inside
/// bracketed-paste to the same `\n` that Shift+Enter would insert).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteDecision {
    /// No focused text_input — drop the paste. Bubbling raw bytes to
    /// Lua would let arbitrary terminal chrome inject content into the
    /// composition; the explicit decision is to ignore paste outside
    /// editable surfaces (matches a focused `<input>` losing focus in
    /// a browser — the paste goes nowhere).
    Drop,
    /// Routed into the focused text_input. The router has already
    /// inserted the text in one buffer mutation; the engine should
    /// fire `on_change` once with the post-insert value.
    HandledByTextInput {
        target_key: String,
        on_change: Option<String>,
        value: String,
        value_changed: bool,
    },
}

/// Top-level paste entry: insert `text` at the cursor of the focused
/// text_input in a single buffer mutation, regardless of how many
/// characters or newlines it contains. For single-line inputs
/// (`max_lines == 1`), embedded newlines are flattened to spaces — a
/// single-line input can't visually represent the extra rows, and
/// silently dropping characters would corrupt the user's clipboard.
///
/// Decoupled from `route_key` so the engine can call it directly when
/// crossterm delivers `Event::Paste(String)` with bracketed-paste
/// enabled. Without this path, a 200-character paste would arrive as
/// 200 separate `Event::Key(Char)` events, each driving its own
/// dispatch + reconcile + render — the user sees the text materialise
/// character-by-character with visible lag (issue #36).
pub fn route_paste(root: &mut WidgetInstance, text: &str) -> PasteDecision {
    let Some(path) = find_focused_path(root) else {
        return PasteDecision::Drop;
    };
    let Some(inst) = instance_at_path(root, &path) else {
        return PasteDecision::Drop;
    };
    let (target_key, on_change, max_lines) = match &inst.last_desc {
        WidgetDescription::TextInput {
            key: Some(k),
            on_change,
            max_lines,
            ..
        } => (k.clone(), on_change.clone(), *max_lines),
        _ => return PasteDecision::Drop,
    };
    // Single-line inputs flatten embedded newlines to spaces; multi-
    // line inputs accept newlines verbatim (same shape Shift+Enter
    // would produce one keypress at a time).
    let normalised: String = if max_lines <= 1 && text.contains(['\n', '\r']) {
        text.chars()
            .map(|c| if matches!(c, '\n' | '\r') { ' ' } else { c })
            .collect()
    } else if text.contains('\r') {
        // Strip bare CR — terminals on Windows send CRLF; the LF alone
        // is what `text_input` understands as a row break.
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.to_string()
    };
    let outcome = match &mut inst.state {
        InstanceState::TextInput(s) => {
            // Paste is a content mutation — clear the manual-scroll
            // latch so the cursor-pin re-engages after the insert.
            s.manual_scroll = false;
            s.insert_str(&normalised)
        }
        _ => return PasteDecision::Drop,
    };
    let value_changed = outcome.new_value.is_some();
    let value = outcome.new_value.unwrap_or_else(|| match &inst.state {
        InstanceState::TextInput(s) => s.last_value.clone(),
        _ => String::new(),
    });
    PasteDecision::HandledByTextInput {
        target_key,
        on_change,
        value,
        value_changed,
    }
}

/// Top-level entry: given the current root and a key event, decide
/// whether to bubble or absorb. Mutates the focused instance's state in
/// place when absorbing.
pub fn route_key(root: &mut WidgetInstance, key: &KeyMessage) -> RouteDecision {
    let Some(path) = find_focused_path(root) else {
        return RouteDecision::BubbleToLua;
    };
    let Some(inst) = instance_at_path(root, &path) else {
        return RouteDecision::BubbleToLua;
    };
    let (target_key, on_change, on_submit, max_lines) = match &inst.last_desc {
        WidgetDescription::TextInput {
            key: Some(k),
            on_change,
            on_submit,
            max_lines,
            ..
        } => (k.clone(), on_change.clone(), on_submit.clone(), *max_lines),
        _ => return RouteDecision::BubbleToLua,
    };

    if !is_editing_key(key, max_lines) {
        return RouteDecision::BubbleToLua;
    }

    // Up/Down at the edge of the input's content bubble to Lua so the
    // chat surface can map them to scroll gestures (Mac keyboards lack
    // PgUp/PgDn). Computed against the post-sync TextInputState the
    // engine just refreshed.
    if matches!(key.name.as_str(), "up" | "down") {
        if let InstanceState::TextInput(s) = &inst.state {
            if arrow_should_bubble(s, key.name.as_str(), max_lines) {
                return RouteDecision::BubbleToLua;
            }
        }
    }

    let outcome = match &mut inst.state {
        InstanceState::TextInput(s) => apply_editing_key(s, key, max_lines),
        _ => return RouteDecision::BubbleToLua,
    };

    let value_changed = outcome.new_value.is_some();
    let value = outcome.new_value.unwrap_or_else(|| match &inst.state {
        InstanceState::TextInput(s) => s.last_value.clone(),
        _ => String::new(),
    });
    RouteDecision::HandledByTextInput {
        target_key,
        on_change,
        on_submit,
        value,
        value_changed,
        submitted: outcome.submitted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::WidgetDescription;
    use crate::reconciler::Reconciler;

    fn key(name: &str, mods: Vec<&'static str>) -> KeyMessage {
        KeyMessage {
            name: name.into(),
            mods,
        }
    }

    fn ti(key: &str, focused: bool, value: &str) -> WidgetDescription {
        WidgetDescription::TextInput {
            key: Some(key.into()),
            value: value.into(),
            focused,
            on_change: Some("input.changed".into()),
            on_submit: Some("input.submit".into()),
            min_lines: 1,
            max_lines: 1,
            placeholder: None,
            cursor_blink: false,
            style: None,
            selectable: false,
        }
    }

    fn column(children: Vec<WidgetDescription>) -> WidgetDescription {
        WidgetDescription::Column {
            children,
            gap: 0,
            key: None,
            selectable: false,
        }
    }

    fn build(desc: WidgetDescription) -> Reconciler {
        let mut r = Reconciler::new();
        r.reconcile(desc);
        // Match the engine's per-render contract: sync text_input state
        // with description before the router runs, so `last_value`
        // reflects the controlled value.
        if let Some(root) = r.root.as_mut() {
            crate::instance::sync_text_inputs(root);
        }
        r
    }

    #[test]
    fn editing_key_classifier_identifies_printables() {
        assert!(is_editing_key(&key("a", vec![]), 1));
        assert!(is_editing_key(&key("A", vec!["shift"]), 1));
        assert!(is_editing_key(&key("?", vec![]), 1));
        assert!(is_editing_key(&key("space", vec![]), 1));
    }

    #[test]
    fn editing_key_classifier_identifies_named() {
        assert!(is_editing_key(&key("backspace", vec![]), 1));
        assert!(is_editing_key(&key("left", vec![]), 1));
        assert!(is_editing_key(&key("right", vec!["shift"]), 1));
        assert!(is_editing_key(&key("home", vec![]), 1));
        assert!(is_editing_key(&key("enter", vec![]), 1));
    }

    #[test]
    fn editing_key_classifier_rejects_modifier_prefixed() {
        assert!(!is_editing_key(&key("b", vec!["ctrl"]), 1));
        assert!(!is_editing_key(&key("escape", vec![]), 1));
        assert!(!is_editing_key(&key("tab", vec![]), 1));
        assert!(!is_editing_key(&key("f5", vec![]), 1));
        assert!(!is_editing_key(&key("pageup", vec![]), 1));
    }

    #[test]
    fn editing_key_classifier_handles_ctrl_editing_subset() {
        assert!(is_editing_key(&key("a", vec!["ctrl"]), 1));
        assert!(is_editing_key(&key("v", vec!["ctrl"]), 1));
        assert!(is_editing_key(&key("z", vec!["ctrl"]), 1));
        assert!(is_editing_key(&key("y", vec!["ctrl"]), 1));
    }

    #[test]
    fn editing_key_classifier_lets_ctrl_c_bubble() {
        // Ctrl+C is the universal "exit/cancel" in raw-mode terminals;
        // text_input must not absorb it.
        assert!(!is_editing_key(&key("c", vec!["ctrl"]), 1));
        assert!(!is_editing_key(&key("c", vec!["ctrl"]), 6));
    }

    #[test]
    fn focused_text_input_bubbles_ctrl_c_to_lua() {
        let mut r = build(ti("input", true, "hi"));
        let root = r.root.as_mut().unwrap();
        let decision = route_key(root, &key("c", vec!["ctrl"]));
        assert_eq!(
            decision,
            RouteDecision::BubbleToLua,
            "Ctrl+C must bubble even when a text_input is focused"
        );
    }

    #[test]
    fn editing_key_classifier_shift_enter_only_when_multiline() {
        assert!(!is_editing_key(&key("enter", vec!["shift"]), 1));
        assert!(is_editing_key(&key("enter", vec!["shift"]), 4));
    }

    #[test]
    fn focused_text_input_absorbs_printable() {
        let mut r = build(ti("input", true, "hi"));
        let root = r.root.as_mut().unwrap();
        // After the first sync the cursor sits at the end of the new
        // value ("hi" → cursor=2, browser-input semantics for external
        // value installs). 'a' therefore appends to the end.
        let decision = route_key(root, &key("a", vec![]));
        match decision {
            RouteDecision::HandledByTextInput {
                target_key,
                value,
                value_changed,
                submitted,
                on_change,
                ..
            } => {
                assert_eq!(target_key, "input");
                assert_eq!(value, "hia");
                assert!(value_changed);
                assert!(!submitted);
                assert_eq!(on_change.as_deref(), Some("input.changed"));
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
    }

    #[test]
    fn unfocused_input_bubbles_all_keys() {
        let mut r = build(ti("input", false, "hi"));
        let root = r.root.as_mut().unwrap();
        let decision = route_key(root, &key("a", vec![]));
        assert_eq!(decision, RouteDecision::BubbleToLua);
    }

    #[test]
    fn focused_input_bubbles_modifier_prefixed_keys() {
        let mut r = build(ti("input", true, "hi"));
        let root = r.root.as_mut().unwrap();
        // Ctrl+B/S/G are NOT readline editing keys — they bubble so a
        // composition above can use them as shortcuts (Ctrl+B = sidebar
        // toggle, etc.). Ctrl+A/E/U/K/W now ABSORB into the editor as
        // their readline equivalents (line start/end, kill-to-start,
        // kill-to-end, delete-word-back) — covered separately below.
        for k in ["b", "s", "g"] {
            let decision = route_key(root, &key(k, vec!["ctrl"]));
            assert_eq!(decision, RouteDecision::BubbleToLua, "ctrl+{k}");
        }
        let escape = route_key(root, &key("escape", vec![]));
        assert_eq!(escape, RouteDecision::BubbleToLua);
        let tab = route_key(root, &key("tab", vec![]));
        assert_eq!(tab, RouteDecision::BubbleToLua);
    }

    #[test]
    fn focused_input_absorbs_readline_editing_chords() {
        // Ctrl+A/E/U/K/W are readline editing chords — absorb them
        // into the focused text_input.
        for k in ["a", "e", "u", "k", "w"] {
            assert!(
                is_editing_key(&key(k, vec!["ctrl"]), 1),
                "ctrl+{k} should be an editing key"
            );
        }
    }

    #[test]
    fn focused_input_absorbs_alt_word_motion_chords() {
        // Alt+Backspace / Alt+Delete delete-word; Alt+Left/Right move
        // by word. All absorb into the focused text_input.
        for name in ["left", "right", "backspace", "delete"] {
            assert!(
                is_editing_key(&key(name, vec!["alt"]), 1),
                "alt+{name} should be an editing key"
            );
        }
    }

    #[test]
    fn alt_left_moves_cursor_word_back() {
        // End-to-end via the router: Alt+Left should land the cursor
        // at the start of the previous word.
        let multi = WidgetDescription::TextInput {
            key: Some("input".into()),
            value: "foo bar baz".into(),
            focused: true,
            on_change: None,
            on_submit: None,
            min_lines: 1,
            max_lines: 1,
            placeholder: None,
            cursor_blink: false,
            style: None,
            selectable: false,
        };
        let mut r = build(multi);
        let root = r.root.as_mut().unwrap();
        // The first sync seeds cursor at end (=11). Alt+Left → start
        // of "baz" (=8).
        let _ = route_key(root, &key("left", vec!["alt"]));
        match &root.state {
            InstanceState::TextInput(s) => assert_eq!(s.cursor, 8),
            _ => panic!("expected text_input state"),
        }
    }

    #[test]
    fn alt_backspace_deletes_word_back() {
        let single = WidgetDescription::TextInput {
            key: Some("input".into()),
            value: "foo bar".into(),
            focused: true,
            on_change: Some("input.changed".into()),
            on_submit: None,
            min_lines: 1,
            max_lines: 1,
            placeholder: None,
            cursor_blink: false,
            style: None,
            selectable: false,
        };
        let mut r = build(single);
        let root = r.root.as_mut().unwrap();
        let decision = route_key(root, &key("backspace", vec!["alt"]));
        match decision {
            RouteDecision::HandledByTextInput {
                value,
                value_changed,
                ..
            } => {
                assert_eq!(value, "foo ");
                assert!(value_changed);
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_focused_input_submits_without_changing_value() {
        let mut r = build(ti("input", true, "hello"));
        let root = r.root.as_mut().unwrap();
        let decision = route_key(root, &key("enter", vec![]));
        match decision {
            RouteDecision::HandledByTextInput {
                value,
                value_changed,
                submitted,
                on_submit,
                ..
            } => {
                assert!(submitted);
                assert!(!value_changed);
                assert_eq!(value, "hello");
                assert_eq!(on_submit.as_deref(), Some("input.submit"));
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
    }

    #[test]
    fn shift_enter_inserts_newline_on_multiline() {
        let multi = WidgetDescription::TextInput {
            key: Some("input".into()),
            value: "hi".into(),
            focused: true,
            on_change: None,
            on_submit: None,
            min_lines: 1,
            max_lines: 4,
            placeholder: None,
            cursor_blink: false,
            style: None,
            selectable: false,
        };
        let mut r = build(multi);
        let root = r.root.as_mut().unwrap();
        // Cursor starts at 0 by default; sync sets last_value.
        let _ = route_key(root, &key("end", vec![])); // jump to end
        let decision = route_key(root, &key("enter", vec!["shift"]));
        match decision {
            RouteDecision::HandledByTextInput {
                value,
                value_changed,
                ..
            } => {
                assert_eq!(value, "hi\n");
                assert!(value_changed);
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
    }

    #[test]
    fn first_focused_wins_when_multiple_declared() {
        // Both inputs declare focused=true. The router picks the first.
        let mut r = build(column(vec![
            ti("first", true, "a"),
            ti("second", true, "b"),
        ]));
        let root = r.root.as_mut().unwrap();
        let decision = route_key(root, &key("x", vec![]));
        match decision {
            RouteDecision::HandledByTextInput { target_key, .. } => {
                assert_eq!(target_key, "first");
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
    }

    #[test]
    fn single_focused_input_inside_column_absorbs_keys() {
        let mut r = build(column(vec![ti("a", false, ""), ti("b", true, "X")]));
        let root = r.root.as_mut().unwrap();
        let decision = route_key(root, &key("y", vec![]));
        match decision {
            RouteDecision::HandledByTextInput {
                target_key, value, ..
            } => {
                assert_eq!(target_key, "b");
                // First sync lands cursor at end of installed value
                // (browser-input semantics for an external value), so 'y'
                // appends after "X".
                assert_eq!(value, "Xy");
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
        let unfocused = &r.root.as_ref().unwrap().children[0];
        match &unfocused.state {
            InstanceState::TextInput(s) => assert_eq!(s.last_value, ""),
            _ => panic!("expected text_input state on unfocused child"),
        }
    }

    // ── Up/Down arrow bubbling at content edges (Mac PgUp/PgDn parity) ─

    fn ti_state(value: &str, cursor: usize, viewport_width: u16) -> TextInputState {
        TextInputState {
            last_value: value.into(),
            cursor,
            viewport_width,
            ..TextInputState::default()
        }
    }

    #[test]
    fn up_arrow_bubbles_when_cursor_on_first_line() {
        // Multi-line input, cursor anywhere on visual row 0 → Up bubbles
        // so the chat surface can scroll the transcript.
        let s = ti_state("first\nsecond\nthird", 2, 80);
        assert!(arrow_should_bubble(&s, "up", 8));
    }

    #[test]
    fn up_arrow_moves_cursor_when_cursor_on_middle_line() {
        // Cursor on the middle row → Up has somewhere to go, absorb.
        let value = "first\nsecond\nthird";
        let mid = "first\n".len() + 2; // inside "second"
        let s = ti_state(value, mid, 80);
        assert!(!arrow_should_bubble(&s, "up", 8));
    }

    #[test]
    fn down_arrow_bubbles_when_cursor_on_last_line() {
        // Cursor on the final visual row → Down bubbles.
        let value = "first\nsecond\nthird";
        let last = value.len() - 1; // inside "third"
        let s = ti_state(value, last, 80);
        assert!(arrow_should_bubble(&s, "down", 8));
    }

    #[test]
    fn single_line_input_bubbles_both_up_and_down() {
        // max_lines == 1 → arrows always bubble regardless of cursor.
        let s = ti_state("hello", 3, 80);
        assert!(arrow_should_bubble(&s, "up", 1));
        assert!(arrow_should_bubble(&s, "down", 1));
    }

    #[test]
    fn arrow_should_bubble_handles_zero_viewport_via_hard_newlines() {
        // Pre-layout (viewport_width == 0): fall back to logical rows.
        let s = ti_state("abc\ndef\nghi", 5, 0); // cursor in "def" (mid)
        assert!(!arrow_should_bubble(&s, "up", 8));
        assert!(!arrow_should_bubble(&s, "down", 8));
        let s_top = ti_state("abc\ndef\nghi", 1, 0); // cursor in "abc"
        assert!(arrow_should_bubble(&s_top, "up", 8));
        let s_bot = ti_state("abc\ndef\nghi", 9, 0); // cursor in "ghi"
        assert!(arrow_should_bubble(&s_bot, "down", 8));
    }

    #[test]
    fn focused_multiline_bubbles_up_at_first_row() {
        // route_key end-to-end: focused multi-line input with cursor on
        // the first visual row — Up must bubble, not absorb. The first
        // sync lands the cursor at the end of the value, so we manually
        // park it on row 0 before exercising the router.
        let multi = WidgetDescription::TextInput {
            key: Some("input".into()),
            value: "abc\ndef".into(),
            focused: true,
            on_change: None,
            on_submit: None,
            min_lines: 1,
            max_lines: 4,
            placeholder: None,
            cursor_blink: false,
            style: None,
            selectable: false,
        };
        let mut r = build(multi);
        let root = r.root.as_mut().unwrap();
        match &mut root.state {
            InstanceState::TextInput(s) => s.cursor = 0,
            _ => panic!("expected text_input state"),
        }
        let decision = route_key(root, &key("up", vec![]));
        assert_eq!(
            decision,
            RouteDecision::BubbleToLua,
            "Up at first visual row must bubble to Lua"
        );
    }

    #[test]
    fn focused_multiline_absorbs_up_when_cursor_on_second_row() {
        let multi = WidgetDescription::TextInput {
            key: Some("input".into()),
            value: "abc\ndef".into(),
            focused: true,
            on_change: None,
            on_submit: None,
            min_lines: 1,
            max_lines: 4,
            placeholder: None,
            cursor_blink: false,
            style: None,
            selectable: false,
        };
        let mut r = build(multi);
        let root = r.root.as_mut().unwrap();
        // Walk cursor to the second row first by pressing End then Down.
        let _ = route_key(root, &key("end", vec![]));
        let _ = route_key(root, &key("down", vec![]));
        let decision = route_key(root, &key("up", vec![]));
        match decision {
            RouteDecision::HandledByTextInput { .. } => {}
            other => panic!("Up on second row should be absorbed, got {other:?}"),
        }
    }

    #[test]
    fn focused_singleline_bubbles_arrow_up_and_down() {
        // Single-line input absorbs printables but bubbles Up/Down so
        // they can drive transcript scrolling.
        let mut r = build(ti("input", true, "hello"));
        let root = r.root.as_mut().unwrap();
        assert_eq!(
            route_key(root, &key("up", vec![])),
            RouteDecision::BubbleToLua
        );
        assert_eq!(
            route_key(root, &key("down", vec![])),
            RouteDecision::BubbleToLua
        );
    }

    // ── bracketed-paste routing (issue #36) ─────────────────────────

    fn ti_multiline(key: &str, focused: bool, value: &str, max_lines: u16) -> WidgetDescription {
        WidgetDescription::TextInput {
            key: Some(key.into()),
            value: value.into(),
            focused,
            on_change: Some("input.changed".into()),
            on_submit: Some("input.submit".into()),
            min_lines: 1,
            max_lines,
            placeholder: None,
            cursor_blink: false,
            style: None,
            selectable: false,
        }
    }

    #[test]
    fn paste_inserts_entire_string_in_one_buffer_mutation() {
        // Regression for issue #36: pasted text must land via insert_str
        // (one mutation), not as N separate insert_char calls. The
        // assertion is structural — the post-paste value contains the
        // full 200-char paste, not a prefix that grew character-by-
        // character through a per-key path.
        let mut r = build(ti_multiline("input", true, "", 6));
        let root = r.root.as_mut().unwrap();
        let payload: String = "x".repeat(200);
        let decision = route_paste(root, &payload);
        match decision {
            PasteDecision::HandledByTextInput {
                target_key,
                value,
                value_changed,
                on_change,
            } => {
                assert_eq!(target_key, "input");
                assert!(value_changed);
                assert_eq!(value.len(), 200);
                assert_eq!(value, payload);
                assert_eq!(on_change.as_deref(), Some("input.changed"));
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
    }

    #[test]
    fn paste_with_newlines_lands_verbatim_on_multiline_input() {
        // A 5-line paste into a multi-line input keeps the embedded
        // \n separators — they look the same as Shift+Enter.
        let mut r = build(ti_multiline("input", true, "", 6));
        let root = r.root.as_mut().unwrap();
        let payload = "line1\nline2\nline3\nline4\nline5";
        match route_paste(root, payload) {
            PasteDecision::HandledByTextInput { value, .. } => {
                assert_eq!(value, payload);
                assert_eq!(value.matches('\n').count(), 4);
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
    }

    #[test]
    fn paste_with_newlines_into_single_line_input_flattens_to_spaces() {
        // Single-line input can't represent multi-row content; flatten
        // \n / \r to spaces rather than silently dropping characters
        // (which would corrupt the user's clipboard payload).
        let mut r = build(ti_multiline("input", true, "", 1));
        let root = r.root.as_mut().unwrap();
        let payload = "alpha\nbeta\rgamma";
        match route_paste(root, payload) {
            PasteDecision::HandledByTextInput { value, .. } => {
                assert_eq!(value, "alpha beta gamma");
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
    }

    #[test]
    fn paste_strips_carriage_return_pairs_in_multiline() {
        // Windows-style CRLF normalises to LF so wrap_value's hard-
        // newline detection sees the row break.
        let mut r = build(ti_multiline("input", true, "", 6));
        let root = r.root.as_mut().unwrap();
        let payload = "a\r\nb\r\nc";
        match route_paste(root, payload) {
            PasteDecision::HandledByTextInput { value, .. } => {
                assert_eq!(value, "a\nb\nc");
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
    }

    #[test]
    fn paste_with_no_focused_input_drops_silently() {
        let mut r = build(ti_multiline("input", false, "", 6));
        let root = r.root.as_mut().unwrap();
        let decision = route_paste(root, "anything");
        assert_eq!(decision, PasteDecision::Drop);
    }

    #[test]
    fn paste_inserts_at_cursor_replacing_selection_semantics() {
        // insert_str is the same primitive Shift+Enter and printable
        // chars use, so paste must inherit cursor-position semantics:
        // insert at cursor, append after the existing value when the
        // cursor sits at the end (browser-input default after sync).
        let mut r = build(ti_multiline("input", true, "prefix-", 6));
        let root = r.root.as_mut().unwrap();
        match route_paste(root, "PASTE") {
            PasteDecision::HandledByTextInput { value, .. } => {
                assert_eq!(value, "prefix-PASTE");
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
    }
}
