//! Lua VM embedding — mlua 0.10 with Lua 5.4.
//!
//! The engine keeps Lua embedded for `init.lua` composition (D-02): users
//! declare which plugins to spawn. Everything richer (UI, text, harnesses)
//! runs in separate plugin processes over NCP (D-03).

pub mod bindings;
pub mod error;
pub mod log;
pub mod mode;
pub mod vm;

pub use error::LuaError;
#[allow(unused_imports)]
pub use mode::EngineMode;
pub use vm::LuaHost;
