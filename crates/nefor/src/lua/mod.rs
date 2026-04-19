//! Lua VM embedding — mlua 0.10 with Lua 5.4.
//!
//! Per spec §`nefor` binary: the binary wraps the combinators library in a
//! real TUI, a plugin host (mlua + wasmtime), and a tokio event loop. This
//! module owns the Lua half: bootstrap a VM, load the user's `init.lua`,
//! install the `nefor.*` API surface, and bridge between Rust-side primitives
//! (event bus, tracing) and Lua callers.
//!
//! ## Threading model
//!
//! mlua's `send` feature makes [`mlua::Lua`] itself `Send + Sync` (internally
//! synchronized via a reentrant mutex). The VM state is still conceptually
//! single-threaded — only one Lua call can be in-flight at a time — but any
//! thread can *initiate* a call. That's what the event bus integration relies
//! on: a Lua-registered handler is wrapped in a Rust closure that clones the
//! [`mlua::Lua`] handle and invokes the stashed Lua function from whichever
//! task emitted the event. mlua serializes the actual bytecode execution.
//!
//! Lua function references (`mlua::Function`) are also `Send + Sync` under
//! `send`; they're pointer-sized handles into the Lua registry. A handler
//! closure captures the handle by clone and calls it on dispatch.
//!
//! ## Boundary error handling
//!
//! Per spec §Rust-caliber errors at the Lua boundary: every binding validates
//! eagerly. The Rust-side bindings raise `mlua::Error::runtime` with a
//! variant-prefixed message (`nefor.events.on: name must be a non-empty
//! string`); the Rust-side caller of `load_init` / future `call_into_lua`
//! paths pattern-matches the Lua error and re-wraps it as a typed
//! [`error::LuaError`].
//!
//! ## What this commit lands
//!
//! - The VM bootstrap, [`vm::LuaHost`].
//! - `nefor.events` — `on` / `off` / `emit` with typed validation.
//! - `nefor.log` — `debug` / `info` / `warn` / `error` routed to `tracing`.
//!
//! `nefor.concurrency`, `nefor.ui`, `nefor.process` land in the follow-up
//! commit once the shape of widget-in-ratatui-frame and process-stdio dispatch
//! are both in place.

pub mod bindings;
pub mod error;
pub mod vm;

pub use error::LuaError;
pub use vm::LuaHost;
