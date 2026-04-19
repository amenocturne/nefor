//! TUI rendering — ratatui + crossterm glue.
//!
//! Per spec §`nefor` binary: the binary is opinionated about *being a good
//! TUI*. This module owns the frame loop, the region-layout protocol, and the
//! widget registry. It stays voiceless about *content* — which widgets render,
//! what the statusline says, and so on come from plugins.
//!
//! This commit lands the Rust-side scaffolding only. The Lua bindings for
//! `nefor.ui.register_widget` / `subscribe_key` / `subscribe_resize` arrive
//! once the Lua VM and event bus are wired.

pub mod app;
pub mod error;
pub mod placeholder;
pub mod region;
pub mod widget;

pub use error::UiError;
pub use placeholder::NoConfigWidget;
pub use region::Region;
pub use widget::WidgetRegistry;
