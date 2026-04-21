//! NCP — engine side of the Nefor Composition Protocol (spec v0.1).
//!
//! The engine has two subsystems (see `docs/principles.md`, "Runner /
//! broker split"):
//!
//! - **Runner** (`runner`) — resolves the plugin root directory, spawns
//!   the declared binary with `Command::new(binary).args(...)`, bridges
//!   stdio. No shell, no env map. Exposes a [`Transport`](transport::Transport)
//!   to the broker.
//! - **Broker** (`broker`) — parses NCP envelopes, stamps `from` (from
//!   the runner-assigned name) and `ts`, validates system messages,
//!   broadcasts events, enforces bounded per-peer queues.
//!
//! Per D-10 the broker rejects malformed input loudly: every rejection is
//! a `SystemBody::Error` back to the offending connection with a specific
//! [`nefor_protocol::ErrorCode`], and close-triggering errors close the
//! connection after the error is emitted.
//!
//! # Submodules
//!
//! - [`broker`] — broker state, stamp-and-broadcast loop, public API.
//! - [`connection`] — a single plugin connection: send queue, read loop.
//! - [`transport`] — `AsyncRead` / `AsyncWrite` traits and the stdio
//!   implementation.
//! - [`runner`] — subprocess spawner (binary + args → `Transport`).
//! - [`spawn`] — [`PluginSpec`] + [`PluginRegistry`] (populated from Lua
//!   `nefor.plugins.spawn`).
//! - [`error`] — engine-side error enum.

pub mod broker;
pub mod connection;
pub mod error;
pub mod runner;
pub mod spawn;
pub mod transport;

pub use broker::Broker;
pub use error::BrokerError;
#[allow(unused_imports)]
pub use runner::{resolve_plugin_root, spawn_plugin, PluginRoot};
pub use spawn::{PluginRegistry, PluginSpec, SharedPluginRegistry};
