use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const METADATA_FILE: &str = "metadata.json";

#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("invalid session id {0:?}")]
    InvalidSessionId(String),
    #[error("installation id must be non-empty and single-line")]
    InvalidInstallationId,
    #[error("session '{0}' does not exist")]
    Missing(String),
    #[error("session '{0}' already exists")]
    Collision(String),
    #[error("session '{session_id}' has invalid metadata: {reason}")]
    InvalidMetadata { session_id: String, reason: String },
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMetadata {
    pub created_with: String,
    pub installation_history: Vec<String>,
}

pub struct SessionHandle {
    root: PathBuf,
    session_id: String,
    events_path: PathBuf,
}

impl SessionHandle {
    pub fn events_path(&self) -> &Path {
        &self.events_path
    }

    pub fn commit_resume(&mut self, installation_id: &str) -> Result<(), SessionStoreError> {
        validate_installation_id(installation_id)?;
        append_installation(&self.root, &self.session_id, installation_id)?;
        Ok(())
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

fn validate_installation_id(value: &str) -> Result<(), SessionStoreError> {
    if value.trim().is_empty() || value.contains('\n') || value.contains('\r') {
        return Err(SessionStoreError::InvalidInstallationId);
    }
    Ok(())
}

fn sessions_dir(root: &Path) -> PathBuf {
    root.to_path_buf()
}
fn session_dir(root: &Path, id: &str) -> PathBuf {
    sessions_dir(root).join(id)
}
fn events_path(root: &Path, id: &str) -> PathBuf {
    sessions_dir(root).join(format!("{id}.jsonl"))
}
fn read_metadata(dir: &Path, id: &str) -> Result<SessionMetadata, SessionStoreError> {
    let bytes = fs::read(dir.join(METADATA_FILE))?;
    let metadata: SessionMetadata =
        serde_json::from_slice(&bytes).map_err(|error| SessionStoreError::InvalidMetadata {
            session_id: id.to_owned(),
            reason: error.to_string(),
        })?;
    if metadata.created_with.is_empty()
        || metadata.installation_history.is_empty()
        || metadata.installation_history[0] != metadata.created_with
        || metadata
            .installation_history
            .iter()
            .any(|entry| entry.is_empty())
    {
        return Err(SessionStoreError::InvalidMetadata {
            session_id: id.to_owned(),
            reason: "created_with must equal the first non-empty installation_history entry"
                .to_owned(),
        });
    }
    Ok(metadata)
}

fn atomic_write_json(path: &Path, value: &SessionMetadata) -> Result<(), SessionStoreError> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "metadata path has no parent")
    })?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, value).map_err(io::Error::other)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn create_session(
    root: &Path,
    id: &str,
    installation_id: &str,
) -> Result<SessionHandle, SessionStoreError> {
    validate_component(id)?;
    validate_installation_id(installation_id)?;
    let dir = session_dir(root, id);
    let events = events_path(root, id);
    if dir.exists() || events.exists() {
        return Err(SessionStoreError::Collision(id.to_owned()));
    }

    fs::create_dir_all(sessions_dir(root))?;
    let staging = tempfile::Builder::new()
        .prefix(&format!(".{id}.creating-"))
        .tempdir_in(sessions_dir(root))?;
    let metadata = SessionMetadata {
        created_with: installation_id.to_owned(),
        installation_history: vec![installation_id.to_owned()],
    };
    atomic_write_json(&staging.path().join(METADATA_FILE), &metadata)?;
    fs::rename(staging.keep(), &dir)?;
    File::open(sessions_dir(root))?.sync_all()?;
    Ok(SessionHandle {
        root: root.to_path_buf(),
        session_id: id.to_owned(),
        events_path: events,
    })
}

fn append_installation(
    root: &Path,
    id: &str,
    installation_id: &str,
) -> Result<PathBuf, SessionStoreError> {
    let dir = session_dir(root, id);
    let events = events_path(root, id);
    if !dir.is_dir() || !events.is_file() {
        return Err(SessionStoreError::Missing(id.to_owned()));
    }
    let mut metadata = read_metadata(&dir, id)?;
    metadata
        .installation_history
        .push(installation_id.to_owned());
    atomic_write_json(&dir.join(METADATA_FILE), &metadata)?;
    Ok(events)
}

pub fn resume_session(
    root: &Path,
    id: &str,
    installation_id: &str,
) -> Result<SessionHandle, SessionStoreError> {
    validate_component(id)?;
    validate_installation_id(installation_id)?;
    let dir = session_dir(root, id);
    let events = events_path(root, id);
    if !dir.is_dir() || !events.is_file() {
        return Err(SessionStoreError::Missing(id.to_owned()));
    }
    read_metadata(&dir, id)?;
    Ok(SessionHandle {
        root: root.to_path_buf(),
        session_id: id.to_owned(),
        events_path: events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_resume_appends_exactly_one_installation_id() {
        let root = tempfile::tempdir().unwrap();
        let lease = create_session(&root.path().join("sessions"), "s1", "generation-a").unwrap();
        fs::write(lease.events_path(), b"events\n").unwrap();
        drop(lease);
        let mut lease =
            resume_session(&root.path().join("sessions"), "s1", "generation-b").unwrap();
        lease.commit_resume("generation-b").unwrap();
        drop(lease);
        assert_eq!(
            read_metadata(&session_dir(&root.path().join("sessions"), "s1"), "s1")
                .unwrap()
                .installation_history,
            vec!["generation-a", "generation-b"]
        );
    }

    #[test]
    fn failed_resume_does_not_create_or_change_metadata() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            resume_session(&root.path().join("sessions"), "missing", "generation-a"),
            Err(SessionStoreError::Missing(_))
        ));
        assert!(!session_dir(&root.path().join("sessions"), "missing").exists());
    }

    #[test]
    fn resume_provenance_is_committed_only_after_activation() {
        let root = tempfile::tempdir().unwrap();
        let lease = create_session(&root.path().join("sessions"), "s1", "generation-a").unwrap();
        fs::write(lease.events_path(), b"events\n").unwrap();
        drop(lease);
        let mut lease =
            resume_session(&root.path().join("sessions"), "s1", "generation-b").unwrap();
        assert_eq!(
            read_metadata(&session_dir(&root.path().join("sessions"), "s1"), "s1")
                .unwrap()
                .installation_history,
            vec!["generation-a"]
        );
        lease.commit_resume("generation-b").unwrap();
        assert_eq!(
            read_metadata(&session_dir(&root.path().join("sessions"), "s1"), "s1")
                .unwrap()
                .installation_history,
            vec!["generation-a", "generation-b"]
        );
    }
}
