//! Input router — keys to focused text_input vs bubble to Lua.
//!
//! Browser-style policy from spec §"Input routing model":
//!
//! - Engine inspects the description tree once per reconcile and caches
//!   the path to the first `focused = true` text_input (by tree order).
//!   Subsequent focused inputs are user error: a `tracing::warn!` fires.
//! - On each key event, the router asks: is this an editing key (printable
//!   char, backspace, delete, arrows, home/end, enter w/o shift, ctrl+a,
//!   ctrl+c, ctrl+v, ctrl+z, ctrl+y, shift+enter when multi-line)?
//!   If yes AND a focused text_input exists → route to it; otherwise
//!   bubble to Lua's `update` as a `key.<name>` message.
//!
//! Modifier-prefixed keys (Ctrl+B, Esc, Tab, F-keys, PgUp/PgDn) and
//! release/repeat events ALWAYS bubble — even when a text_input is
//! focused. This is the same shape as a focused `<input>` in a browser.

use crate::desc::WidgetDescription;
use crate::input::KeyMessage;
use crate::instance::{InstanceKind, InstanceState, WidgetInstance};
use crate::text_input::{allows_newline_insert, EditOutcome, TextInputState};

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
        "backspace" | "delete" | "left" | "right" | "up" | "down" | "home" | "end" => {
            !has_alt && !has_super
        }
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
        "a" | "c" | "v" | "z" | "y" if has_ctrl && !has_alt && !has_super => true,
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

/// Apply an editing key to `state`, given the description's bookkeeping.
/// Returns the `EditOutcome` so the caller can fire `on_change`/`on_submit`.
pub fn apply_editing_key(
    state: &mut TextInputState,
    key: &KeyMessage,
    max_lines: u16,
) -> EditOutcome {
    let has_shift = key.mods.contains(&"shift");
    let has_ctrl = key.mods.contains(&"ctrl");

    match key.name.as_str() {
        "backspace" => state.backspace(),
        "delete" => state.delete_forward(),
        "left" => state.move_left(has_shift),
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
        "a" if has_ctrl => state.select_all(),
        "c" if has_ctrl => {
            // Copy: no system clipboard in v1; treat as no-op. Bracketed
            // paste lands in v1 via terminal sequences; the OS-level
            // copy belongs in a later phase.
            EditOutcome::default()
        }
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
        }
    }

    fn column(children: Vec<WidgetDescription>) -> WidgetDescription {
        WidgetDescription::Column {
            children,
            gap: 0,
            key: None,
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
        assert!(is_editing_key(&key("c", vec!["ctrl"]), 1));
        assert!(is_editing_key(&key("v", vec!["ctrl"]), 1));
        assert!(is_editing_key(&key("z", vec!["ctrl"]), 1));
        assert!(is_editing_key(&key("y", vec!["ctrl"]), 1));
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
        // Default cursor is 0 (fresh state), so 'a' inserts at the
        // start — the test asserts the absorption shape, not the
        // cursor management Lua would normally drive.
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
                assert_eq!(value, "ahi");
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
        for k in ["b", "s", "k"] {
            let decision = route_key(root, &key(k, vec!["ctrl"]));
            assert_eq!(decision, RouteDecision::BubbleToLua, "ctrl+{k}");
        }
        let escape = route_key(root, &key("escape", vec![]));
        assert_eq!(escape, RouteDecision::BubbleToLua);
        let tab = route_key(root, &key("tab", vec![]));
        assert_eq!(tab, RouteDecision::BubbleToLua);
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
                // sync sets cursor=0 (default); inserting at 0 prepends.
                assert_eq!(value, "yX");
            }
            other => panic!("expected HandledByTextInput, got {other:?}"),
        }
        let unfocused = &r.root.as_ref().unwrap().children[0];
        match &unfocused.state {
            InstanceState::TextInput(s) => assert_eq!(s.last_value, ""),
            _ => panic!("expected text_input state on unfocused child"),
        }
    }
}
