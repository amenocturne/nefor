//! nefor engine — library surface.
//!
//! The `nefor` crate ships primarily as a binary (see `main.rs`), but also
//! exposes the NCP broker + transport types as a library so workspace
//! integration tests can drive a real [`ncp::Broker`] in-process against
//! subprocess-spawned plugins. Only modules needed by out-of-crate consumers
//! live here; the binary keeps its `cli`, `config`, `events`, `lua`, `log`,
//! and `paths` submodules private.

pub mod ncp;
