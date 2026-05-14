//! Stable installation id stamped on every Responses request.
//!
//! Codex generates a UUID once per machine and re-uses it across runs so
//! server-side analytics can attribute usage to a single seat. We follow
//! the same pattern. The file lives next to `chatgpt-auth.json` under
//! `$NEFOR_DATA_DIR/nefor/chatgpt-installation-id`; first run creates
//! it, subsequent runs read it.

use std::path::{Path, PathBuf};

use crate::error::ChatgptError;

/// Resolve the path the installation id is persisted to.
///
/// Priority: `$NEFOR_DATA_DIR/nefor/chatgpt-installation-id` →
/// `dirs::data_dir()/nefor/chatgpt-installation-id`. Same shape as
/// `auth::store::default_auth_path` so both files live together.
pub fn default_installation_path() -> Result<PathBuf, ChatgptError> {
    if let Ok(dir) = std::env::var("NEFOR_DATA_DIR") {
        return Ok(PathBuf::from(dir)
            .join("nefor")
            .join("chatgpt-installation-id"));
    }
    let base = dirs::data_dir().ok_or(ChatgptError::DataDirUnavailable)?;
    Ok(base.join("nefor").join("chatgpt-installation-id"))
}

/// Read the existing installation id, or generate-and-persist a fresh
/// one. Whitespace-only file contents are treated as missing so a
/// half-written file from a prior crash heals itself.
pub fn read_or_generate(path: &Path) -> Result<String, ChatgptError> {
    if let Ok(s) = std::fs::read_to_string(path) {
        let trimmed = s.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generates_and_persists_on_first_call() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("chatgpt-installation-id");
        let id = read_or_generate(&path).expect("generate");
        assert!(!id.is_empty());
        assert_eq!(std::fs::read_to_string(&path).expect("read").trim(), id);
    }

    #[test]
    fn second_call_returns_existing_id() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("chatgpt-installation-id");
        let a = read_or_generate(&path).expect("first");
        let b = read_or_generate(&path).expect("second");
        assert_eq!(a, b);
    }

    #[test]
    fn whitespace_only_file_is_treated_as_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("chatgpt-installation-id");
        std::fs::write(&path, "   \n").expect("write blank");
        let id = read_or_generate(&path).expect("regenerate");
        assert!(!id.trim().is_empty());
    }

    #[test]
    fn creates_parent_directory() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("dir").join("inst-id");
        let _ = read_or_generate(&path).expect("generate");
        assert!(path.exists());
    }
}
