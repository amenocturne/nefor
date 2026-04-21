//! Engine-internal event bus.
//!
//! Per D-02 / D-09: the engine has an in-process bus for its own lifecycle
//! (startup / shutdown / tick). Plugins never observe it directly — they see
//! NCP messages brokered by [`crate::ncp`] instead. This bus is for engine
//! subsystems (Lua VM, scheduler hooks) that share a process.

#![allow(unused_imports)]

pub mod bus;

pub use bus::{
    EventBus, EventHandler, EventName, EventPayload, SubscriptionId, SHUTDOWN, STARTUP, TICK,
};
