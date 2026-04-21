//! Event bus implementation.
//!
//! See the module-level doc on [`crate::events`] for rationale. This file
//! carries the types.
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

// Lifecycle events carried by the internal bus. Post-NCP the bus is
// engine-internal only (plugins never see it); KEY/RESIZE were TUI-specific
// and moved to the nefor-tui plugin where they originate from crossterm.

/// Lifecycle event: binary finished startup, plugins loaded.
pub const STARTUP: &str = "startup";

/// Lifecycle event: binary is about to exit cleanly.
pub const SHUTDOWN: &str = "shutdown";

/// Lifecycle event: periodic engine tick (1 Hz by default; concrete schedulers
/// set their own cadence).
pub const TICK: &str = "tick";

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
/// Typed variants exist for the lifecycle events so core subscribers avoid
/// `Any`-downcasting. Plugin events travel via [`EventPayload::Custom`] — an
/// `Arc<dyn Any + Send + Sync>` so the bus remains payload-agnostic and
/// handler dispatch stays cheap (cloning the `Arc`, not the payload).
pub enum EventPayload {
    /// No payload. Used for events like `startup` / `shutdown`.
    None,

    /// Redraw / scheduler tick. Distinct from [`EventPayload::None`] so
    /// subscribers can match on intent even though no data rides along.
    Tick,

    /// Arbitrary payload for user / plugin events. Boxed trait object so the
    /// bus is payload-generic; downcasting is the subscriber's job.
    Custom(Arc<dyn Any + Send + Sync>),
}

impl fmt::Debug for EventPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("EventPayload::None"),
            Self::Tick => f.write_str("EventPayload::Tick"),
            // Avoid probing the inner Any — we don't know its Debug impl.
            Self::Custom(_) => f.write_str("EventPayload::Custom(<Any>)"),
        }
    }
}

/// Handler invoked on dispatch.
pub type EventHandler = Box<dyn Fn(&EventPayload) + Send + Sync + 'static>;

/// Opaque monotonic id returned by [`EventBus::on`] for later [`EventBus::off`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

impl SubscriptionId {
    /// The raw monotonic id. Useful only for debug output.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// Reconstruct a `SubscriptionId` from its raw `u64`.
    pub fn from_u64(id: u64) -> Self {
        Self(id)
    }
}

struct Subscription {
    id: SubscriptionId,
    name: EventName,
    handler: Arc<EventHandler>,
}

/// Fan-out event bus.
pub struct EventBus {
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
    pub fn emit(&self, name: &EventName, payload: EventPayload) {
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
        bus.emit(&EventName::from(TICK), EventPayload::Tick);
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
        let other_count = Arc::new(AtomicU64::new(0));

        let tc = Arc::clone(&tick_count);
        bus.on(
            EventName::from(TICK),
            Box::new(move |_| {
                tc.fetch_add(1, Ordering::Relaxed);
            }),
        );
        let oc = Arc::clone(&other_count);
        bus.on(
            EventName::from("other"),
            Box::new(move |_| {
                oc.fetch_add(1, Ordering::Relaxed);
            }),
        );

        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        bus.emit(&EventName::from(TICK), EventPayload::Tick);
        assert_eq!(tick_count.load(Ordering::Relaxed), 2);
        assert_eq!(other_count.load(Ordering::Relaxed), 0);
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
    fn reentrant_emit_is_safe() {
        let bus = Arc::new(EventBus::new());
        let counter = Arc::new(AtomicU64::new(0));

        let b2 = Arc::clone(&bus);
        let c = Arc::clone(&counter);
        bus.on(
            EventName::from("ping"),
            Box::new(move |_| {
                let prev = c.fetch_add(1, Ordering::Relaxed);
                if prev == 0 {
                    b2.emit(&EventName::from("ping"), EventPayload::None);
                }
            }),
        );

        bus.emit(&EventName::from("ping"), EventPayload::None);
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
}
