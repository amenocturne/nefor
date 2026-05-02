//! nefor-tui — declarative TUI plugin for nefor.
//!
//! Phase 1 of the rewrite. The crate exposes a small library surface so
//! the engine can be driven from in-process integration tests, and a thin
//! binary wrapper (`main.rs`) for the NCP plugin role. Higher-level
//! widgets land in phases 2–5.

pub mod animation;
pub mod ansi;
pub mod desc;
pub mod engine;
pub mod error;
pub mod input;
pub mod input_router;
pub mod instance;
pub mod layout;
pub mod lua_host;
pub mod markdown;
pub mod mouse;
pub mod ncp;
pub mod reconciler;
pub mod render;
pub mod scrollable;
pub mod text_input;
pub mod tty;
