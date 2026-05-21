//! nefor engine — library surface.
//!
//! The `nefor` crate ships primarily as a binary (see `main.rs`), but also
//! exposes the NCP broker + transport types as a library so workspace
//! integration tests can drive a real [`ncp::Broker`] in-process against
//! subprocess-spawned plugins. Only modules needed by out-of-crate consumers
//! live here; the binary keeps its `cli`, `config`, and `log` submodules
//! private. `paths` is exposed so test harnesses can build the
//! `DataDir` newtype that `LuaHost::new` requires.

pub mod events;
pub mod lua;
pub mod ncp;
pub mod paths;
pub mod session;
