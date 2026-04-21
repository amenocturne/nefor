//! Newtype wrappers for domain identifiers.
//!
//! Per spec §Code-Level Conventions — "Newtypes for every domain ID." Post-NCP
//! the engine only tracks one concept of its own: the plugin id. Session /
//! turn / capability ids were part of the pre-NCP world and belong to
//! plugins now (D-06 / D-07).

#![allow(dead_code)]

use std::fmt;

/// Identifier for a configured plugin. Matches the plugin's NCP `attach.name`
/// claim for spawned plugins, but is independent of NCP naming — this is the
/// engine-side identity used by spawn config and log tagging before the
/// plugin has attached.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginId(pub String);

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn plugin_id_display_eq_hash() {
        let a = PluginId("mock-plugin".to_string());
        let b = PluginId("mock-plugin".to_string());
        let c = PluginId("nefor-tui".to_string());
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(format!("{a}"), "mock-plugin");

        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }
}
