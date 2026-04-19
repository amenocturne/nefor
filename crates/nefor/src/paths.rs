//! `PathBuf` newtypes — paths carrying semantics. Per spec §Code-Level
//! Conventions: "Paths carry semantics too."
//!
//! Only [`ConfigDir`] exists for MVP. `WorkDir`, `PluginDir`, `ScopedDir` land
//! when a concrete caller needs them.

use std::path::{Path, PathBuf};

/// The resolved nefor config directory (e.g., `~/.config/nefor/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDir(pub PathBuf);

impl ConfigDir {
    /// Borrow the underlying path. Public API for future callers (init.lua
    /// loader, plugin discovery) that don't exist yet in this commit.
    #[allow(dead_code)]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl std::fmt::Display for ConfigDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.display().fmt(f)
    }
}
