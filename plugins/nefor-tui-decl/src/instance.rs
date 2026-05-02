//! Widget instances — the reconciler-owned tree that mirrors the latest
//! description tree. An instance survives across renders when its
//! `(type_tag, key)` matches in both the previous and the new tree; per-
//! instance state inside `InstanceState` is moved verbatim across the
//! rebuild.

use crate::animation::AnimationState;
use crate::desc::WidgetDescription;
use crate::layout::{Rect, Size};
use crate::text_input::TextInputState;

/// Composite reconciler key. Two stages compose it:
/// - `type_tag` — static string, never reused across primitive types
/// - `id` — `User(s)` if `desc.key = Some(s)`, else `Position(i)` from
///   the parent's child slot.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct InstanceKey {
    pub type_tag: &'static str,
    pub id: KeyId,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum KeyId {
    User(String),
    Position(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceKind {
    Text,
    Spans,
    Markdown,
    Animation,
    Column,
    Row,
    Padding,
    Stack,
    Expanded,
    Spacer,
    Constrained,
    Align,
    Anchored,
    TextInput,
}

/// Per-primitive internal state preserved across `view` rebuilds. Phase 1
/// has no widgets that carry meaningful state, but the slot is plumbed so
/// the state-preservation invariant can be exercised under test.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum InstanceState {
    /// Phase-1 placeholder. Reserved for cached wrap result.
    #[default]
    Text,
    Spans,
    Markdown,
    /// Per-instance animation state — `mount_time_ms` is recorded on
    /// first observation by the engine. Survives across re-renders via
    /// the reconciler key, so a re-rendered animation does not jump
    /// back to frame 0.
    Animation(AnimationState),
    Column,
    Row,
    Padding,
    Stack,
    Expanded,
    Spacer,
    Constrained,
    Align,
    Anchored,
    /// Per-instance editing state for `text_input`. Survives across
    /// re-renders via the reconciler key (per spec: cursor, selection,
    /// scroll offset, IME composition, undo stack).
    TextInput(TextInputState),
}

/// Layout side-effect storage on each instance — set by the measure pass,
/// read by the paint pass. Reset before each measure to drop stale data
/// from prior frames (the reconciler preserves state across rebuilds, so
/// nothing else clears it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutResult {
    /// Size returned by the most recent `layout` call.
    pub size: Size,
    /// For row/column: per-child main-axis size, ordered by child index.
    /// Empty for non-flex parents.
    pub flex_main_sizes: Vec<u16>,
    /// For `anchored`: the resolved (width, height) the child should use.
    /// `None` when the instance is not an anchored.
    pub anchored_child_size: Option<Size>,
    /// Rect the instance occupied during the most recent paint pass.
    /// `None` when the instance was clipped (zero-area rect) or never
    /// painted. Populated by `layout::paint` so the mouse hit-test can
    /// resolve a screen coord to the deepest enclosing instance.
    pub painted_rect: Option<Rect>,
}

impl LayoutResult {
    pub fn reset(&mut self) {
        self.size = Size::default();
        self.flex_main_sizes.clear();
        self.anchored_child_size = None;
        self.painted_rect = None;
    }
}

#[derive(Debug)]
pub struct WidgetInstance {
    pub key: InstanceKey,
    pub children: Vec<WidgetInstance>,
    pub state: InstanceState,
    pub last_desc: WidgetDescription,
    pub layout: LayoutResult,
}

impl WidgetInstance {
    /// Static type tag of the underlying primitive — sourced from the
    /// stored description, since description and instance-kind are kept
    /// in lockstep by the reconciler.
    pub fn kind(&self) -> InstanceKind {
        kind_of(&self.last_desc)
    }
}

/// Default state slot for a freshly mounted instance.
pub fn default_state(kind: InstanceKind) -> InstanceState {
    match kind {
        InstanceKind::Text => InstanceState::Text,
        InstanceKind::Spans => InstanceState::Spans,
        InstanceKind::Markdown => InstanceState::Markdown,
        InstanceKind::Animation => InstanceState::Animation(AnimationState::default()),
        InstanceKind::Column => InstanceState::Column,
        InstanceKind::Row => InstanceState::Row,
        InstanceKind::Padding => InstanceState::Padding,
        InstanceKind::Stack => InstanceState::Stack,
        InstanceKind::Expanded => InstanceState::Expanded,
        InstanceKind::Spacer => InstanceState::Spacer,
        InstanceKind::Constrained => InstanceState::Constrained,
        InstanceKind::Align => InstanceState::Align,
        InstanceKind::Anchored => InstanceState::Anchored,
        InstanceKind::TextInput => InstanceState::TextInput(TextInputState::default()),
    }
}

pub fn kind_of(desc: &WidgetDescription) -> InstanceKind {
    match desc {
        WidgetDescription::Text { .. } => InstanceKind::Text,
        WidgetDescription::Spans { .. } => InstanceKind::Spans,
        WidgetDescription::Markdown { .. } => InstanceKind::Markdown,
        WidgetDescription::Animation { .. } => InstanceKind::Animation,
        WidgetDescription::Column { .. } => InstanceKind::Column,
        WidgetDescription::Row { .. } => InstanceKind::Row,
        WidgetDescription::Padding { .. } => InstanceKind::Padding,
        WidgetDescription::Stack { .. } => InstanceKind::Stack,
        WidgetDescription::Expanded { .. } => InstanceKind::Expanded,
        WidgetDescription::Spacer { .. } => InstanceKind::Spacer,
        WidgetDescription::Constrained { .. } => InstanceKind::Constrained,
        WidgetDescription::Align { .. } => InstanceKind::Align,
        WidgetDescription::Anchored { .. } => InstanceKind::Anchored,
        WidgetDescription::TextInput { .. } => InstanceKind::TextInput,
    }
}

/// Compose the `(type_tag, key_id)` for a description at a given parent
/// child-slot index.
pub fn instance_key(desc: &WidgetDescription, position: usize) -> InstanceKey {
    let type_tag = desc.type_tag();
    let id = match desc.user_key() {
        Some(s) => KeyId::User(s.to_string()),
        None => KeyId::Position(position),
    };
    InstanceKey { type_tag, id }
}

/// Walk the instance tree and reconcile each `text_input`'s internal
/// editing state with its description. Called once after every
/// reconcile so the input router and the layout/paint pass observe a
/// fresh `last_value` and `focused` flag.
pub fn sync_text_inputs(inst: &mut WidgetInstance) {
    if matches!(inst.kind(), InstanceKind::TextInput) {
        if let WidgetDescription::TextInput { value, focused, .. } = &inst.last_desc {
            let v = value.clone();
            let f = *focused;
            if let InstanceState::TextInput(s) = &mut inst.state {
                s.sync_with_desc(&v, f);
            }
        }
    }
    for c in inst.children.iter_mut() {
        sync_text_inputs(c);
    }
}
