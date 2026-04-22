//! NCP — engine side of the Nefor Composition Protocol (spec v0.1).
//!
//! Post-Slice-2-I3 the engine is protocol-agnostic string routing:
//!
//! - **Runner** (`runner`) — resolves the plugin root directory, spawns
//!   the declared binary with `Command::new(binary).args(...)`, bridges
//!   stdio. No shell, no env map. Exposes a [`Transport`](transport::Transport)
//!   to the broker.
//! - **Broker** (`broker`) — stamps inbound lines, mirrors them to the
//!   session log, invokes the Lua `step` function, and routes step's
//!   outbound sends to connection writers. No envelope parsing, no
//!   system-message dispatch, no replay-on-attach. All NCP protocol
//!   handling lives in `starter/init.lua`.
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
#[allow(unused_imports)]
pub use runner::{resolve_plugin_root, spawn_plugin, PluginRoot};
pub use spawn::{PluginRegistry, PluginSpec, SharedPluginRegistry};
