//! Newtype wrappers for domain identifiers.
//!
//! Per spec §Code-Level Conventions — "Newtypes for every domain ID." Post-NCP
//! the engine only tracks one concept of its own: the plugin id. Session /
//! turn / capability ids were part of the pre-NCP world and belong to
//! plugins now (D-06 / D-07).

#![allow(dead_code)]

use std::fmt;

/// Identifier for a configured plugin. Set by spawn config (Lua
/// `nefor.plugins.spawn { name = ... }`) and stamped as the `from`
/// identity on every envelope the corresponding connection emits.
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
