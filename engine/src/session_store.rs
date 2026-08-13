use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("invalid session id {0:?}")]
    InvalidSessionId(String),
    #[error("session '{0}' does not exist")]
    Missing(String),
    #[error("session '{0}' already exists")]
    Collision(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub struct SessionHandle {
    events_path: PathBuf,
}

impl SessionHandle {
    pub fn events_path(&self) -> &Path {
        &self.events_path
    }
}

fn validate_component(value: &str) -> Result<(), SessionStoreError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(SessionStoreError::InvalidSessionId(value.to_owned()));
    }
    Ok(())
}

fn events_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{id}.jsonl"))
}

pub fn create_session(root: &Path, id: &str) -> Result<SessionHandle, SessionStoreError> {
    validate_component(id)?;
    let events = events_path(root, id);
    if events.exists() || root.join(id).exists() {
        return Err(SessionStoreError::Collision(id.to_owned()));
    }
    fs::create_dir_all(root)?;
    Ok(SessionHandle {
        events_path: events,
    })
}

pub fn resume_session(root: &Path, id: &str) -> Result<SessionHandle, SessionStoreError> {
    validate_component(id)?;
    let events = events_path(root, id);
    if !events.is_file() {
        return Err(SessionStoreError::Missing(id.to_owned()));
    }
    Ok(SessionHandle {
        events_path: events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_session_create_needs_no_distribution_identity() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let handle = create_session(&sessions, "s1").unwrap();
        assert_eq!(handle.events_path(), sessions.join("s1.jsonl"));
        assert!(!sessions.join("s1").exists());
    }

    #[test]
    fn existing_event_log_resumes_without_metadata() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("s1.jsonl"), b"events\n").unwrap();
        let handle = resume_session(&sessions, "s1").unwrap();
        assert_eq!(handle.events_path(), sessions.join("s1.jsonl"));
    }
}
