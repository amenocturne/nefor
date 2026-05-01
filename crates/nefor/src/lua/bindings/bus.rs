//! `nefor.bus.on_event` — register a Lua handler against an event-kind pattern.
//!
//! This is the engine's only built-in subscription primitive. Its job is
//! pure routing into Lua: keep a small, ordered list of (pattern, handler)
//! pairs; when an envelope is routed (after `step`), match its `body.kind`
//! against each pattern and invoke matching handlers.
//!
//! The match is intentionally trivial:
//! - pattern with no `*` → exact-string equality.
//! - pattern ending in `*` → prefix match on everything before the `*`.
//!
//! No globbing, no regex, no double-`*`. Per D-17, the routing primitive
//! stays as small as it can be — richer filtering belongs to the Lua
//! handler itself, which can re-inspect `body.from`, sub-fields of `body`,
//! or compose with other handlers.
//!
//! Handlers run in the engine's Lua VM (single-threaded), in the same
//! call site as `step`. Errors raised inside a handler are logged and
//! swallowed so a faulty handler can't take down the engine loop or
//! starve subsequent handlers.

use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, RegistryKey, Table, Value};

/// One subscription kept in the registry. The function lives in the Lua
/// registry so its lifetime tracks the VM, not Rust references; we hold
/// the key. Pattern is parsed once at registration.
#[derive(Debug)]
pub struct Subscription {
    /// Match shape (exact or prefix).
    pub pattern: KindPattern,
    /// Lua function to invoke. Borrowed back into a `Function` via the
    /// VM at dispatch time.
    pub handler: Arc<RegistryKey>,
}

/// Closed set of supported pattern shapes. D-16 — match logic is enum
/// variants, not stringly-typed sniffing of the original pattern at
/// dispatch time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindPattern {
    /// Exact equality: `body.kind == s`.
    Exact(String),
    /// Trailing-`*` wildcard: `body.kind` starts with `prefix`.
    Prefix(String),
}

impl KindPattern {
    /// Parse a pattern string. Trailing `*` becomes [`KindPattern::Prefix`];
    /// anything else is [`KindPattern::Exact`]. We don't reject mid-pattern
    /// `*` here — they fall through to exact match against the literal,
    /// which is the right "do nothing surprising" behaviour.
    pub fn parse(pattern: &str) -> Self {
        if let Some(prefix) = pattern.strip_suffix('*') {
            KindPattern::Prefix(prefix.to_owned())
        } else {
            KindPattern::Exact(pattern.to_owned())
        }
    }

    /// Returns true iff `kind` matches this pattern.
    pub fn matches(&self, kind: &str) -> bool {
        match self {
            KindPattern::Exact(s) => kind == s.as_str(),
            KindPattern::Prefix(p) => kind.starts_with(p.as_str()),
        }
    }
}

/// Shared subscription registry. Wrapped in an `Arc<Mutex<...>>` so the
/// `nefor.bus.on_event` binding (which writes) and the broker's
/// post-step dispatch (which reads) can share it without ownership
/// gymnastics.
#[derive(Debug, Default)]
pub struct EventSubscriptions {
    subs: Vec<Subscription>,
}

impl EventSubscriptions {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of every (pattern, handler) pair, in registration order.
    /// The dispatcher iterates this snapshot rather than holding the lock
    /// across handler invocations — handlers may register or remove
    /// subscriptions of their own.
    pub fn snapshot(&self) -> Vec<(KindPattern, Arc<RegistryKey>)> {
        self.subs
            .iter()
            .map(|s| (s.pattern.clone(), Arc::clone(&s.handler)))
            .collect()
    }

    /// Append a subscription.
    pub fn push(&mut self, sub: Subscription) {
        self.subs.push(sub);
    }
}

/// Shared handle alias.
pub type SharedSubscriptions = Arc<Mutex<EventSubscriptions>>;

/// Install `nefor.bus.on_event` onto `nefor_tbl`.
pub fn install_bus(
    lua: &Lua,
    nefor_tbl: &Table,
    subscriptions: SharedSubscriptions,
) -> mlua::Result<()> {
    let bus = lua.create_table()?;

    let on_event = lua.create_function(move |lua, (pattern, handler): (Value, Value)| {
        let pattern = match pattern {
            Value::String(s) => s.to_str()?.to_owned(),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.bus.on_event: pattern must be a string (got {})",
                    other.type_name(),
                )));
            }
        };
        let handler: Function = match handler {
            Value::Function(f) => f,
            other => {
                return Err(mlua::Error::runtime(format!(
                    "nefor.bus.on_event: handler must be a function (got {})",
                    other.type_name(),
                )));
            }
        };
        let key = lua.create_registry_value(handler)?;
        let sub = Subscription {
            pattern: KindPattern::parse(&pattern),
            handler: Arc::new(key),
        };
        let mut guard = match subscriptions.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push(sub);
        Ok(())
    })?;
    bus.set("on_event", on_event)?;

    nefor_tbl.set("bus", bus)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Lua, SharedSubscriptions) {
        let lua = Lua::new();
        let subs: SharedSubscriptions = Arc::new(Mutex::new(EventSubscriptions::new()));
        let nefor = lua.create_table().unwrap();
        install_bus(&lua, &nefor, Arc::clone(&subs)).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        (lua, subs)
    }

    #[test]
    fn parse_exact_pattern() {
        assert_eq!(
            KindPattern::parse("chat.input"),
            KindPattern::Exact("chat.input".into())
        );
    }

    #[test]
    fn parse_prefix_pattern() {
        assert_eq!(
            KindPattern::parse("chat.*"),
            KindPattern::Prefix("chat.".into())
        );
    }

    #[test]
    fn matches_exact() {
        let p = KindPattern::Exact("chat.input".into());
        assert!(p.matches("chat.input"));
        assert!(!p.matches("chat.input.x"));
        assert!(!p.matches("chat"));
    }

    #[test]
    fn matches_prefix_includes_empty_suffix() {
        // `"chat.*"` accepts the bare prefix `"chat."` (no remaining tail).
        let p = KindPattern::Prefix("chat.".into());
        assert!(p.matches("chat."));
        assert!(p.matches("chat.input"));
        assert!(p.matches("chat.stream.delta"));
        assert!(!p.matches("chatx.input"));
    }

    #[test]
    fn empty_prefix_matches_everything() {
        // `"*"` → Prefix("") matches any string.
        let p = KindPattern::parse("*");
        assert!(p.matches(""));
        assert!(p.matches("anything"));
    }

    #[test]
    fn on_event_registers_subscription() {
        let (lua, subs) = setup();
        lua.load(r#"nefor.bus.on_event("chat.*", function(env) end)"#)
            .exec()
            .expect("ok");
        let guard = subs.lock().unwrap();
        let snap = guard.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, KindPattern::Prefix("chat.".into()));
    }

    #[test]
    fn on_event_rejects_non_string_pattern() {
        let (lua, _) = setup();
        let err = lua
            .load(r#"nefor.bus.on_event(42, function() end)"#)
            .exec()
            .expect_err("must reject");
        assert!(err.to_string().contains("pattern must be a string"));
    }

    #[test]
    fn on_event_rejects_non_function_handler() {
        let (lua, _) = setup();
        let err = lua
            .load(r#"nefor.bus.on_event("k", 42)"#)
            .exec()
            .expect_err("must reject");
        assert!(err.to_string().contains("handler must be a function"));
    }
}
