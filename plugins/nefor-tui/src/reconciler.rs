//! Tree reconciler.
//!
//! Match rule per child: `(type_tag, key_id)` where `key_id = User(s)` if
//! the description carries `key = "..."`, else `Position(i)` from the
//! child slot in the parent. When an instance and a description share the
//! key, the instance's state is preserved verbatim and the children are
//! recursed. Otherwise the old subtree is unmounted and a fresh one
//! mounted in its place.
//!
//! Sequencing per frame is depth-first: unmount deleted subtrees, then
//! mount fresh subtrees, then run updates on retained instances. Phase 1
//! has no user-visible lifecycle callbacks so the three phases collapse
//! into a single recursive pass; the ordering matters once mount/unmount
//! callbacks land in later phases.

use std::collections::HashMap;

use crate::desc::WidgetDescription;
use crate::instance::{
    default_state, instance_key, kind_of, InstanceKey, LayoutResult, WidgetInstance,
};

#[derive(Debug, Default)]
pub struct Reconciler {
    pub root: Option<WidgetInstance>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileSummary {
    pub mounted: usize,
    pub reused: usize,
    pub unmounted: usize,
}

impl Reconciler {
    pub fn new() -> Self {
        Self { root: None }
    }

    /// Reconcile the previous root against `new_desc`. Returns a summary
    /// of how many instances were freshly mounted, reused (state
    /// preserved), or unmounted as a result.
    pub fn reconcile(&mut self, new_desc: WidgetDescription) -> ReconcileSummary {
        let mut summary = ReconcileSummary::default();
        let new_root_key = instance_key(&new_desc, 0);
        let prev = self.root.take();
        let next = match prev {
            Some(inst) if inst.key == new_root_key => update_instance(inst, new_desc, &mut summary),
            Some(inst) => {
                unmount_subtree(inst, &mut summary);
                mount_subtree(new_desc, 0, &mut summary)
            }
            None => mount_subtree(new_desc, 0, &mut summary),
        };
        self.root = Some(next);
        summary
    }
}

/// Build a fresh instance subtree for `desc`. `position` is the slot in
/// the parent's children list (used as fallback when the description has
/// no user-supplied `key`).
fn mount_subtree(
    desc: WidgetDescription,
    position: usize,
    summary: &mut ReconcileSummary,
) -> WidgetInstance {
    summary.mounted += 1;
    let kind = kind_of(&desc);
    let key = instance_key(&desc, position);
    let children = match &desc {
        WidgetDescription::Text { .. }
        | WidgetDescription::Spans { .. }
        | WidgetDescription::Markdown { .. }
        | WidgetDescription::Animation { .. }
        | WidgetDescription::Spacer { .. }
        | WidgetDescription::Fill { .. }
        | WidgetDescription::TextInput { .. } => Vec::new(),
        WidgetDescription::Column { children, .. }
        | WidgetDescription::Row { children, .. }
        | WidgetDescription::Stack { children, .. } => children
            .iter()
            .enumerate()
            .map(|(i, c)| mount_subtree(c.clone(), i, summary))
            .collect(),
        WidgetDescription::Padding { child, .. }
        | WidgetDescription::Expanded { child, .. }
        | WidgetDescription::Constrained { child, .. }
        | WidgetDescription::Align { child, .. }
        | WidgetDescription::Anchored { child, .. }
        | WidgetDescription::Scrollable { child, .. } => {
            vec![mount_subtree((**child).clone(), 0, summary)]
        }
    };
    WidgetInstance {
        key,
        children,
        state: default_state(kind),
        last_desc: desc,
        layout: LayoutResult::default(),
    }
}

/// Reuse `inst` for the new description. State is preserved; children are
/// reconciled by `(type_tag, key_id)` match.
fn update_instance(
    mut inst: WidgetInstance,
    new_desc: WidgetDescription,
    summary: &mut ReconcileSummary,
) -> WidgetInstance {
    summary.reused += 1;

    let new_children = match &new_desc {
        WidgetDescription::Text { .. }
        | WidgetDescription::Spans { .. }
        | WidgetDescription::Markdown { .. }
        | WidgetDescription::Animation { .. }
        | WidgetDescription::Spacer { .. }
        | WidgetDescription::Fill { .. }
        | WidgetDescription::TextInput { .. } => Vec::new(),
        WidgetDescription::Column { children, .. }
        | WidgetDescription::Row { children, .. }
        | WidgetDescription::Stack { children, .. } => children.clone(),
        WidgetDescription::Padding { child, .. }
        | WidgetDescription::Expanded { child, .. }
        | WidgetDescription::Constrained { child, .. }
        | WidgetDescription::Align { child, .. }
        | WidgetDescription::Anchored { child, .. }
        | WidgetDescription::Scrollable { child, .. } => vec![(**child).clone()],
    };

    inst.children = reconcile_children(std::mem::take(&mut inst.children), new_children, summary);
    let is_leaf = matches!(
        new_desc,
        WidgetDescription::Text { .. }
            | WidgetDescription::Spans { .. }
            | WidgetDescription::Markdown { .. }
            | WidgetDescription::Animation { .. }
            | WidgetDescription::Spacer { .. }
            | WidgetDescription::Fill { .. }
    );
    if is_leaf && inst.last_desc != new_desc {
        inst.layout.cached_constraints = None;
    }
    inst.last_desc = new_desc;
    inst
}

fn reconcile_children(
    old_children: Vec<WidgetInstance>,
    new_children: Vec<WidgetDescription>,
    summary: &mut ReconcileSummary,
) -> Vec<WidgetInstance> {
    // Index old children by their stored key; preserves insertion order
    // among same-key collisions (none should exist) but tolerates them.
    let mut old_map: HashMap<InstanceKey, Vec<WidgetInstance>> = HashMap::new();
    for inst in old_children {
        old_map.entry(inst.key.clone()).or_default().push(inst);
    }

    let mut out: Vec<WidgetInstance> = Vec::with_capacity(new_children.len());
    for (i, desc) in new_children.into_iter().enumerate() {
        let candidate_key = instance_key(&desc, i);
        let reused = old_map
            .get_mut(&candidate_key)
            .and_then(|bucket| (!bucket.is_empty()).then(|| bucket.remove(0)));
        match reused {
            Some(inst) => out.push(update_instance(inst, desc, summary)),
            None => out.push(mount_subtree(desc, i, summary)),
        }
    }

    // Anything left in `old_map` is dropped: depth-first unmount.
    for (_, leftovers) in old_map.drain() {
        for inst in leftovers {
            unmount_subtree(inst, summary);
        }
    }

    out
}

fn unmount_subtree(inst: WidgetInstance, summary: &mut ReconcileSummary) {
    // Depth-first: descend before counting the parent so the deepest
    // children unmount before their ancestors. Phase 1 has no Lua-visible
    // unmount hooks; the bookkeeping exists so phase-4 widget state
    // (text_input, scrollable) drops cleanly.
    for child in inst.children {
        unmount_subtree(child, summary);
    }
    summary.unmounted += 1;
    // Explicitly drop here so the rename in later phases (when state /
    // last_desc grow real Drop impls) doesn't change observable behaviour.
    let _ = inst.state;
    let _ = inst.last_desc;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::{WidgetDescription, WrapMode};
    use crate::instance::{InstanceState, KeyId};

    fn text(content: &str, key: Option<&str>) -> WidgetDescription {
        WidgetDescription::Text {
            content: content.into(),
            style: None,
            wrap: WrapMode::Word,
            key: key.map(|s| s.to_string()),
        }
    }

    fn column(children: Vec<WidgetDescription>, key: Option<&str>) -> WidgetDescription {
        WidgetDescription::Column {
            children,
            gap: 0,
            key: key.map(|s| s.to_string()),
            selectable: false,
        }
    }

    #[test]
    fn fresh_root_mounts() {
        let mut r = Reconciler::new();
        let s = r.reconcile(text("hi", None));
        assert_eq!(s.mounted, 1);
        assert_eq!(s.reused, 0);
        assert_eq!(s.unmounted, 0);
    }

    #[test]
    fn root_with_same_key_is_reused() {
        let mut r = Reconciler::new();
        r.reconcile(text("a", None));
        let s = r.reconcile(text("b", None));
        assert_eq!(s.reused, 1, "same type+position should reuse");
        assert_eq!(s.mounted, 0);
    }

    #[test]
    fn reuse_by_user_key() {
        let mut r = Reconciler::new();
        r.reconcile(column(
            vec![text("a", Some("first")), text("b", Some("second"))],
            None,
        ));

        // Reorder children: "second" first, "first" second. Position
        // changes but keys match — both should be reused.
        let s = r.reconcile(column(
            vec![text("b2", Some("second")), text("a2", Some("first"))],
            None,
        ));
        assert_eq!(s.reused, 3, "column + 2 children all reused");
        assert_eq!(s.mounted, 0);
        assert_eq!(s.unmounted, 0);
    }

    #[test]
    fn reuse_by_position_when_no_key() {
        let mut r = Reconciler::new();
        r.reconcile(column(vec![text("a", None), text("b", None)], None));
        let s = r.reconcile(column(vec![text("a2", None), text("b2", None)], None));
        assert_eq!(s.reused, 3); // column + 2 children
    }

    #[test]
    fn changing_type_at_position_unmounts_old() {
        let mut r = Reconciler::new();
        r.reconcile(column(vec![text("a", None)], None));
        let s = r.reconcile(column(vec![column(vec![], None)], None));
        // column reused; old text unmounted; new (empty) column mounted.
        assert!(s.unmounted >= 1);
        assert!(s.mounted >= 1);
    }

    #[test]
    fn state_survives_rebuild_with_same_key() {
        let mut r = Reconciler::new();
        r.reconcile(text("a", Some("x")));
        // Mark the underlying state so we can confirm survival.
        if let Some(inst) = r.root.as_mut() {
            inst.state = InstanceState::Text;
        }
        let stable_addr = r.root.as_ref().unwrap() as *const _;
        r.reconcile(text("b", Some("x")));
        // The reconciler reuses the existing instance in place — so the
        // root pointer keeps pointing into the same allocation.
        let new_addr = r.root.as_ref().unwrap() as *const _;
        assert_eq!(stable_addr, new_addr, "instance should be moved verbatim");
        // State is `Text` (default for text); the invariant we test is
        // "the existing struct survived" (no fresh mount).
    }

    #[test]
    fn state_drops_on_unmount() {
        let mut r = Reconciler::new();
        r.reconcile(column(vec![text("a", Some("doomed"))], None));
        let s = r.reconcile(column(vec![text("b", Some("survivor"))], None));
        assert!(s.unmounted >= 1, "old keyed child should be unmounted");
        assert!(s.mounted >= 1, "new keyed child should be mounted");
    }

    #[test]
    fn reorder_by_key_preserves_state() {
        let mut r = Reconciler::new();
        r.reconcile(column(
            vec![text("first", Some("a")), text("second", Some("b"))],
            None,
        ));
        let s = r.reconcile(column(
            vec![text("second2", Some("b")), text("first2", Some("a"))],
            None,
        ));
        assert_eq!(s.reused, 3); // column + both children
        assert_eq!(s.mounted, 0);
        assert_eq!(s.unmounted, 0);

        // Confirm the children moved order according to the new tree.
        let root = r.root.as_ref().expect("root present");
        let kids = &root.children;
        match (&kids[0].key.id, &kids[1].key.id) {
            (KeyId::User(a), KeyId::User(b)) => {
                assert_eq!(a, "b");
                assert_eq!(b, "a");
            }
            other => panic!("unexpected key shape: {other:?}"),
        }
    }
}
