//! openai-provider library surface.
//!
//! Most of the plugin lives in `main.rs`. The library exposes the
//! request shape, the SSE parser, and the session state types so the
//! integration tests can exercise them without driving the binary
//! end-to-end.

pub mod auth;
pub mod broker;
pub mod catalog;
pub mod config;
pub mod openai;
pub mod state;
pub mod stream;
