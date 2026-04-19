//! Typed UI errors.
//!
//! Scoped to the `ui` module's surface: registration conflicts and terminal
//! I/O failures. Per spec §Code-Level Conventions, every module owns a typed
//! error enum; [`crate::error::NeforError`] aggregates them at the top.

use crate::ui::region::Region;

/// Errors produced by the TUI layer.
#[derive(Debug, thiserror::Error)]
pub enum UiError {
    /// Two widgets tried to claim the same [`Region`]. Fails eagerly at
    /// [`crate::ui::widget::WidgetRegistry::register`] — spec §Rust-caliber
    /// errors at the Lua boundary: "widget region conflicts fail at
    /// `register_widget`, not at first render."
    #[error("widget region conflict: {region:?} is already claimed")]
    WidgetRegionConflict {
        /// The region both widgets asked for.
        region: Region,
    },

    /// Any terminal / crossterm I/O failure — raw-mode toggling, alternate
    /// screen enter/leave, event read.
    #[error(transparent)]
    Terminal(#[from] std::io::Error),
}
