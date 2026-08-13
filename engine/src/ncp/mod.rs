//! NCP — engine side of the Nefor Composition Protocol (spec v0.1).
//!
//! Post-Slice-2-I3 the engine is protocol-agnostic string routing:
//!
//! - **Runner** (`runner`) — spawns the Lua-declared command with
//!   `Command::new(binary).args(...)` and bridges stdio. No shell, no env map,
//!   and no installation-layout discovery. Exposes a
//!   [`Transport`](transport::Transport) to the broker.
//! - **Broker** (`broker`) — stamps inbound lines, appends published entries to
//!   its in-memory dispatch log, invokes the Lua `dispatch` function, and routes
//!   outbound deliveries to connection writers. It owns no session ID or
//!   persistent session log and performs no envelope parsing, system-message
//!   dispatch, or replay-on-attach; those behaviors live in Lua composition.
//!
//! # Submodules
//!
//! - [`broker`] — broker state, step invocation loop, public API.
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

pub use broker::{Broker, BrokerOps, BrokerShared};
pub use error::BrokerError;
pub use runner::spawn_plugin;
pub use spawn::{PluginKind, PluginRegistry, PluginSpec, SharedPluginRegistry};
