//! Shared plugin state.
//!
//! Kept tiny and ADT-flavoured: readiness is an enum, not a bool pair.
//! Callbacks registered from Lua observe the state via the `nefor.state`
//! read-only accessor.

/// Lifecycle position of the plugin.
///
/// Encoded as an enum (not two booleans) so "ready but also shutting down"
/// is unrepresentable. The wire name is the snake_case variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    /// `ready` has been sent; `ready_ok` has not yet arrived. Events
    /// received in this window are buffered or dropped (see ncp.rs);
    /// `nefor.emit` is an error.
    AwaitingReadyOk,
    /// Handshake complete. `nefor.emit` is live and bus events are
    /// dispatched to Lua handlers.
    Ready,
    /// Engine has sent `shutdown`, or stdin has closed. The `on_shutdown`
    /// handler runs; then the process exits.
    ShuttingDown,
}

impl PluginState {
    /// Wire / Lua-facing name. Stable — tests and scripts may match on it.
    pub fn as_str(self) -> &'static str {
        match self {
            PluginState::AwaitingReadyOk => "awaiting_ready_ok",
            PluginState::Ready => "ready",
            PluginState::ShuttingDown => "shutting_down",
        }
    }
}
