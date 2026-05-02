//! Widget instances — the reconciler-owned tree that mirrors the latest
//! description tree. An instance survives across renders when its
//! `(type_tag, key)` matches in both the previous and the new tree; per-
//! instance state inside `InstanceState` is moved verbatim across the
//! rebuild.

use crate::desc::WidgetDescription;

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
    Column,
    Padding,
}

/// Per-primitive internal state preserved across `view` rebuilds. Phase 1
/// has no widgets that carry meaningful state, but the slot is plumbed so
/// the state-preservation invariant can be exercised under test.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum InstanceState {
    /// Phase-1 placeholder. Reserved for cached wrap result.
    #[default]
    Text,
    Column,
    Padding,
}

#[derive(Debug)]
pub struct WidgetInstance {
    pub key: InstanceKey,
    pub children: Vec<WidgetInstance>,
    pub state: InstanceState,
    pub last_desc: WidgetDescription,
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
        InstanceKind::Column => InstanceState::Column,
        InstanceKind::Padding => InstanceState::Padding,
    }
}

pub fn kind_of(desc: &WidgetDescription) -> InstanceKind {
    match desc {
        WidgetDescription::Text { .. } => InstanceKind::Text,
        WidgetDescription::Column { .. } => InstanceKind::Column,
        WidgetDescription::Padding { .. } => InstanceKind::Padding,
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
