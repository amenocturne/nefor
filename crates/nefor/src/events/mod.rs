//! Rust-side event bus.
//!
//! Per spec §`nefor` binary — "Event bus. Register subscribers, dispatch
//! events. The binary emits a small set of lifecycle events (`startup`,
//! `shutdown`, `tick`, `key`, `resize`); plugins emit their own."
//!
//! This module lands the Rust surface only. The Lua bindings
//! (`nefor.events.on` / `off` / `emit`) arrive in a subsequent commit; their
//! shape is already fixed by the spec and the types below map 1:1 so the
//! bridge is a thin transcoding layer.
//!
//! The bus is synchronous on dispatch: [`EventBus::emit`] invokes every
//! registered handler inline on the calling task. Async plugin work goes
//! through `tokio::spawn` inside the handler body (the spec's
//! `nefor.concurrency.spawn` will be the Lua-visible wrapper).

#![allow(unused_imports)]

pub mod bus;

pub use bus::{
    EventBus, EventHandler, EventName, EventPayload, SubscriptionId, KEY, RESIZE, SHUTDOWN,
    STARTUP, TICK,
};
