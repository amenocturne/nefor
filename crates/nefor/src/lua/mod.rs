//! Lua VM embedding — mlua 0.10 with Lua 5.4.
//!
//! The engine keeps Lua embedded for `init.lua` composition (D-02): users
//! declare which plugins to spawn. Everything richer (UI, text, harnesses)
//! runs in separate plugin processes over NCP (D-03).

pub mod bindings;
pub mod error;
pub mod log;
pub mod vm;

pub use error::LuaError;
// I3 will wire the step log types into main/broker; until then the re-exports
// are consumed only by tests inside the `log` module.
#[allow(unused_imports)]
pub use log::{log_entry_to_lua_table, log_to_lua_table, LogEntry};
pub use vm::LuaHost;
