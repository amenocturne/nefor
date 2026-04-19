//! Newtype wrappers for domain identifiers.
//!
//! Per spec §Code-Level Conventions — "Newtypes for every domain ID." Stringly
//! typed IDs across a big system is how `plugin_id` and `session_id` get
//! swapped by accident. Each ID carries meaning through its type.
//!
//! Serde derives are omitted until a concrete caller serializes them.
//!
//! Declared up-front per spec; concrete constructors land with the modules
//! that own the corresponding domain (plugin loader, session manager, etc.).
#![allow(dead_code)]

use std::fmt;

/// Identifier for a loaded plugin.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginId(pub String);

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier for a nefor session (one TUI run = one session).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Monotonically-increasing turn index within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TurnId(pub u64);

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifier for a WIT capability (e.g., `"nefor:fs/scoped@1"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CapabilityId(pub String);

impl fmt::Display for CapabilityId {
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
        let c = PluginId("example-harness".to_string());
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(format!("{a}"), "mock-plugin");

        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }

    #[test]
    fn session_id_display_eq_hash() {
        let a = SessionId("s1".to_string());
        let b = SessionId("s1".to_string());
        assert_eq!(a, b);
        assert_eq!(format!("{a}"), "s1");
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn turn_id_display_eq_hash_ord() {
        let a = TurnId(1);
        let b = TurnId(1);
        let c = TurnId(2);
        assert_eq!(a, b);
        assert!(a < c);
        assert_eq!(format!("{a}"), "1");
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn capability_id_display_eq_hash() {
        let a = CapabilityId("nefor:fs/scoped@1".to_string());
        let b = CapabilityId("nefor:fs/scoped@1".to_string());
        let c = CapabilityId("nefor:net/http@1".to_string());
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(format!("{a}"), "nefor:fs/scoped@1");
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }
}
