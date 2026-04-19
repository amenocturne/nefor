//! Event bus implementation.
//!
//! See the module-level doc on [`crate::events`] for rationale. This file
//! carries the types.
//!
//! `dead_code` is allowed module-wide because `on`/`off`/`EventName::new` /
//! `EventName::as_str` / `SubscriptionId::as_u64` / the stored `Subscription`
//! fields / `EventPayload::Custom` have no Rust-side caller yet — the Lua
//! bindings (next commit) are their consumers. The TUI uses `emit` only.
//!
//! ## Concurrency model
//!
//! [`EventBus`] is `Send + Sync` and internally synchronized via a
//! [`std::sync::Mutex`] around the subscriber table. Critical sections are
//! deliberately tiny — push/remove for `on`/`off`, clone-the-handler-list
//! for `emit` — so contention stays cheap and we never hold the mutex across
//! an `.await`. `tokio::sync::Mutex` is intentionally avoided: the dispatch
//! path is synchronous and an async mutex would be strictly worse here.
//!
//! ## Reentrancy
//!
//! Handlers run while the subscriber snapshot is already cloned out of the
//! mutex, so a handler is free to call [`EventBus::emit`], [`EventBus::on`],
//! or [`EventBus::off`] on the same bus without deadlocking. Subscribers
//! registered during dispatch are *not* observed by the in-flight emit (the
//! snapshot is frozen at call start); they fire starting from the next emit.
//! Unsubscribing during dispatch, likewise, does not cancel an already-
//! scheduled handler within the same emit.

#![allow(dead_code)]

use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crossterm::event::KeyEvent;

// Namespace convention: core lifecycle events have bare names; plugin events
// should be namespaced `plugin:event`, e.g. `cc:message_update`,
// `dag:node_done`. Not enforced in MVP — the spec reserves validation for the
// Lua-side `nefor.events.on` loader, which lands with the Lua bindings.

/// Lifecycle event: binary finished startup, plugins loaded.
pub const STARTUP: &str = "startup";

/// Lifecycle event: binary is about to exit cleanly.
pub const SHUTDOWN: &str = "shutdown";

/// Lifecycle event: periodic redraw tick from the TUI.
pub const TICK: &str = "tick";

/// Lifecycle event: key pressed in the TUI.
pub const KEY: &str = "key";

/// Lifecycle event: terminal resized.
pub const RESIZE: &str = "resize";

/// Name of an event. Newtype over [`String`] so the bus API accepts both
/// borrowed str literals (for the core lifecycle constants) and owned strings
/// (for plugin-emitted names) without implicit conversions at the call site.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventName(String);

impl EventName {
    /// Construct from an owned [`String`].
    pub fn new(name: String) -> Self {
        Self(name)
    }

    /// Borrow the inner str.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for EventName {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for EventName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Payload delivered with an event.
///
/// Typed variants exist for the lifecycle events so core subscribers (the
/// renderer, future logging probes) avoid `Any`-downcasting. Plugin events
/// travel via [`EventPayload::Custom`] — an `Arc<dyn Any + Send + Sync>` so
/// the bus remains payload-agnostic and handler dispatch stays cheap (cloning
/// the `Arc`, not the payload).
///
/// The Lua bridge (future task) transcodes through serde at the boundary and
/// wraps the resulting Lua value into [`EventPayload::Custom`].
pub enum EventPayload {
    /// No payload. Used for events like `startup` / `shutdown` / `tick`.
    None,

    /// A key press.
    Key(KeyEvent),

    /// Terminal resized to `cols` × `rows`.
    Resize {
        /// New terminal column count.
        cols: u16,
        /// New terminal row count.
        rows: u16,
    },

    /// Redraw tick. Distinct from [`EventPayload::None`] so subscribers can
    /// match on intent even though no data rides along.
    Tick,

    /// Arbitrary payload for user / plugin events. Boxed trait object so the
    /// bus is payload-generic; downcasting is the subscriber's job.
    Custom(Arc<dyn Any + Send + Sync>),
}

impl fmt::Debug for EventPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("EventPayload::None"),
            Self::Key(k) => f.debug_tuple("EventPayload::Key").field(k).finish(),
            Self::Resize { cols, rows } => f
                .debug_struct("EventPayload::Resize")
                .field("cols", cols)
                .field("rows", rows)
                .finish(),
            Self::Tick => f.write_str("EventPayload::Tick"),
            // Avoid probing the inner Any — we don't know its Debug impl.
            Self::Custom(_) => f.write_str("EventPayload::Custom(<Any>)"),
        }
    }
}

/// Handler invoked on dispatch.
///
/// Synchronous: if a handler needs async work it spawns a task inside its
/// body. Dispatch order within a single name is registration order but
/// subscribers should not rely on it — future batching / parallel dispatch
/// may reorder. Cloning the payload, if needed, is the handler's concern.
pub type EventHandler = Box<dyn Fn(&EventPayload) + Send + Sync + 'static>;

/// Opaque monotonic id returned by [`EventBus::on`] for later [`EventBus::off`].
///
/// `Copy` so callers can store and pass it cheaply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

impl SubscriptionId {
    /// The raw monotonic id. Useful only for debug output.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// Reconstruct a `SubscriptionId` from its raw `u64`. Paired with
    /// [`SubscriptionId::as_u64`] for round-tripping through Lua, where ids
    /// cross the Rust→Lua boundary as plain integers and come back through
    /// `nefor.events.off`. Passing an id that was never issued (or one that
    /// already unsubscribed) is a no-op at [`EventBus::off`], so no
    /// correctness risk from this being public.
    pub fn from_u64(id: u64) -> Self {
        Self(id)
    }
}

struct Subscription {
    id: SubscriptionId,
    name: EventName,
    // Stored as Arc so `emit` can snapshot handlers out of the mutex without
    // cloning the underlying closure. Registration wraps the Box-typed public
    // `EventHandler` into an Arc internally.
    handler: Arc<EventHandler>,
}

/// Fan-out event bus.
///
/// Shared across tasks via `Arc<EventBus>`. See the module-level doc for
/// concurrency and reentrancy guarantees.
pub struct EventBus {
    // All subscriptions live in one vector; dispatch scans by name. The
    // expected subscriber count is O(10s), not O(millions) — a HashMap keyed
    // by name is a future optimization if profiling demands it.
    subs: Mutex<Vec<Subscription>>,
    next_id: AtomicU64,
}

impl EventBus {
    /// Create an empty bus.
    pub fn new() -> Self {
        Self {
            subs: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// Register `handler` for events named `name`. Returns a
    /// [`SubscriptionId`] the caller uses to unregister later.
    pub fn on(&self, name: EventName, handler: EventHandler) -> SubscriptionId {
        let id = SubscriptionId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let sub = Subscription {
            id,
            name,
            handler: Arc::new(handler),
        };
        // Poisoned mutex: the previous holder panicked. The bus's internal
        // invariants are unaffected (we only push/remove/iterate), so
        // recovering the guard with `into_inner` is sound.
        let mut guard = match self.subs.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push(sub);
        id
    }

    /// Remove the subscription identified by `sub`. No-op if already removed
    /// or never registered.
    pub fn off(&self, sub: SubscriptionId) {
        let mut guard = match self.subs.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.retain(|s| s.id != sub);
    }

    /// Emit `payload` for `name`. Every handler registered for this name is
    /// invoked synchronously, in registration order.
    ///
    /// Reentrancy: handlers may call `emit`/`on`/`off` on the same bus without
    /// deadlock. The snapshot of subscribers is captured under a brief lock
    /// and dispatch runs after the lock is released.
    pub fn emit(&self, name: &EventName, payload: EventPayload) {
        // Clone the Arcs of matching handlers out of the lock; drop the
        // guard before invoking anything. This keeps the critical section
        // tiny and makes reentrant emit safe.
        let handlers: Vec<Arc<EventHandler>> = {
            let guard = match self.subs.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard
                .iter()
                .filter(|s| s.name == *name)
                .map(|s| Arc::clone(&s.handler))
                .collect()
        };
        for h in handlers {
            (h)(&payload);
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for EventBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self
            .subs
            .lock()
            .map(|g| g.len())
            .unwrap_or_else(|p| p.into_inner().len());
        f.debug_struct("EventBus")
            .field("subscribers", &count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::thread;

    #[test]
    fn emit_with_no_subscribers_is_noop() {
        let bus = EventBus::new();
        // Must not panic, must not allocate a handler from nowhere.
        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        // And the subscriber table stays empty.
        assert_eq!(bus.subs.lock().unwrap().len(), 0);
    }

    #[test]
    fn on_then_emit_delivers_payload_once_per_subscriber() {
        let bus = EventBus::new();
        let counter = Arc::new(AtomicU64::new(0));

        let c = Arc::clone(&counter);
        bus.on(
            EventName::from(TICK),
            Box::new(move |payload| {
                assert!(matches!(payload, EventPayload::Tick));
                c.fetch_add(1, Ordering::Relaxed);
            }),
        );

        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn off_removes_subscriber() {
        let bus = EventBus::new();
        let counter = Arc::new(AtomicU64::new(0));

        let c = Arc::clone(&counter);
        let sub = bus.on(
            EventName::from(TICK),
            Box::new(move |_| {
                c.fetch_add(1, Ordering::Relaxed);
            }),
        );

        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        bus.off(sub);
        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn off_on_unknown_id_is_noop() {
        let bus = EventBus::new();
        bus.off(SubscriptionId(999));
    }

    #[test]
    fn multiple_subscribers_same_name_all_receive() {
        let bus = EventBus::new();
        let counter = Arc::new(AtomicU64::new(0));

        for _ in 0..3 {
            let c = Arc::clone(&counter);
            bus.on(
                EventName::from(TICK),
                Box::new(move |_| {
                    c.fetch_add(1, Ordering::Relaxed);
                }),
            );
        }

        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn subscribers_on_different_names_are_isolated() {
        let bus = EventBus::new();
        let tick_count = Arc::new(AtomicU64::new(0));
        let key_count = Arc::new(AtomicU64::new(0));

        let tc = Arc::clone(&tick_count);
        bus.on(
            EventName::from(TICK),
            Box::new(move |_| {
                tc.fetch_add(1, Ordering::Relaxed);
            }),
        );
        let kc = Arc::clone(&key_count);
        bus.on(
            EventName::from(KEY),
            Box::new(move |_| {
                kc.fetch_add(1, Ordering::Relaxed);
            }),
        );

        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        assert_eq!(tick_count.load(Ordering::Relaxed), 2);
        assert_eq!(key_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn concurrent_emit_from_multiple_threads() {
        let bus = Arc::new(EventBus::new());
        let counter = Arc::new(AtomicU64::new(0));

        let c = Arc::clone(&counter);
        bus.on(
            EventName::from(TICK),
            Box::new(move |_| {
                c.fetch_add(1, Ordering::Relaxed);
            }),
        );

        let iters_per_thread: u64 = 1_000;
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let b = Arc::clone(&bus);
                thread::spawn(move || {
                    for _ in 0..iters_per_thread {
                        b.emit(&EventName::from(TICK), EventPayload::Tick);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::Relaxed), iters_per_thread * 2);
    }

    #[test]
    fn subscription_id_is_copy() {
        let bus = EventBus::new();
        let id = bus.on(EventName::from(TICK), Box::new(|_| {}));
        // Copy-check: we pass by value twice without moving.
        let a = id;
        let b = id;
        assert_eq!(a, b);
    }

    #[test]
    fn reentrant_emit_is_safe() {
        // A handler that re-emits the same event must not deadlock.
        let bus = Arc::new(EventBus::new());
        let counter = Arc::new(AtomicU64::new(0));

        let b2 = Arc::clone(&bus);
        let c = Arc::clone(&counter);
        bus.on(
            EventName::from("ping"),
            Box::new(move |_| {
                let prev = c.fetch_add(1, Ordering::Relaxed);
                // Re-emit once, bounded, to exercise reentrancy without
                // infinite recursion.
                if prev == 0 {
                    b2.emit(&EventName::from("ping"), EventPayload::None);
                }
            }),
        );

        bus.emit(&EventName::from("ping"), EventPayload::None);
        // First emit increments once, handler re-emits → second increment.
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn event_name_from_str_and_string() {
        let a: EventName = "tick".into();
        let b: EventName = String::from("tick").into();
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "tick");
    }

    #[test]
    fn custom_payload_roundtrip() {
        // User-defined payload shape travels through Custom and the handler
        // downcasts it back.
        #[derive(Debug, PartialEq)]
        struct MyPayload {
            id: u64,
            label: &'static str,
        }

        let bus = EventBus::new();
        let received = Arc::new(Mutex::new(None));
        let r = Arc::clone(&received);

        bus.on(
            EventName::from("plugin:thing"),
            Box::new(move |payload| {
                if let EventPayload::Custom(arc) = payload {
                    if let Some(my) = arc.downcast_ref::<MyPayload>() {
                        *r.lock().unwrap() = Some((my.id, my.label));
                    }
                }
            }),
        );

        let payload = Arc::new(MyPayload { id: 7, label: "hi" });
        bus.emit(
            &EventName::from("plugin:thing"),
            EventPayload::Custom(payload),
        );

        let got = *received.lock().unwrap();
        assert_eq!(got, Some((7, "hi")));
    }

    #[test]
    fn event_bus_debug_shows_count() {
        let bus = EventBus::new();
        bus.on(EventName::from(TICK), Box::new(|_| {}));
        bus.on(EventName::from(KEY), Box::new(|_| {}));
        let s = format!("{:?}", bus);
        assert!(s.contains("subscribers"));
        assert!(s.contains('2'));
    }

    #[test]
    fn event_payload_debug_does_not_probe_custom_inner() {
        let payload = EventPayload::Custom(Arc::new(42u64));
        let s = format!("{:?}", payload);
        // We deliberately omit the inner Any contents.
        assert!(s.contains("Custom"));
        assert!(!s.contains("42"));
    }
}
