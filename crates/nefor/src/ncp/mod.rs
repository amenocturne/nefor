//! NCP broker — engine side of the Nefor Composition Protocol (spec v0.1).
//!
//! The broker is the engine's entire user-facing surface at the wire level:
//! it spawns plugin processes, receives their JSON-Lines envelopes over stdio
//! (§2), enforces the 4-field envelope (§3) and the attach handshake
//! (§5.1-§5.2), broadcasts event messages to every other attached plugin
//! (§6), and delivers the lifecycle system messages (`plugin_joined`,
//! `plugin_left`, `shutdown`) as point-to-point sends per §9 clarification.
//!
//! Per D-10 the broker rejects malformed input loudly: every rejection is a
//! `SystemBody::Error` back to the offending connection with a specific
//! [`nefor_protocol::ErrorCode`], and close-triggering errors close the
//! connection after the error is emitted.
//!
//! # Submodules
//!
//! - [`broker`] — broker state, spawn-stamp-broadcast loop, public API.
//! - [`connection`] — a single plugin connection: attach handshake, send
//!   queue, read loop, dispatch.
//! - [`transport`] — `AsyncRead` / `AsyncWrite` traits and the stdio
//!   implementation.
//! - [`spawn`] — [`PluginSpec`] + [`PluginRegistry`] (populated from Lua
//!   `nefor.plugins.spawn`).
//! - [`error`] — broker-internal error enum.

pub mod broker;
pub mod connection;
pub mod error;
pub mod spawn;
pub mod transport;

pub use broker::Broker;
pub use error::BrokerError;
pub use spawn::{PluginRegistry, PluginSpec, SharedPluginRegistry};
