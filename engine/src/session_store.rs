use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
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
    #[error("session '{0}' already exists at the destination")]
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

pub struct SessionLease {
    lock: File,
    events_path: PathBuf,
}

impl SessionLease {
    pub fn events_path(&self) -> &Path {
        &self.events_path
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock);
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
    root.join("sessions")
}
fn session_dir(root: &Path, id: &str) -> PathBuf {
    sessions_dir(root).join(id)
}
fn events_path(root: &Path, id: &str) -> PathBuf {
    sessions_dir(root).join(format!("{id}.jsonl"))
}
fn lock_path(root: &Path, id: &str) -> PathBuf {
    sessions_dir(root).join(".locks").join(format!("{id}.lock"))
}

fn acquire_lock(root: &Path, id: &str) -> Result<File, SessionStoreError> {
    let path = lock_path(root, id);
    fs::create_dir_all(
        path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "lock path has no parent")
        })?,
    )?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock_exclusive()?;
    Ok(file)
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
) -> Result<SessionLease, SessionStoreError> {
    validate_component(id)?;
    validate_installation_id(installation_id)?;
    let lock = acquire_lock(root, id)?;
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
    Ok(SessionLease {
        lock,
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

pub fn record_resume(
    root: &Path,
    id: &str,
    installation_id: &str,
) -> Result<PathBuf, SessionStoreError> {
    validate_component(id)?;
    validate_installation_id(installation_id)?;
    append_installation(root, id, installation_id)
}

pub fn resume_session(
    root: &Path,
    id: &str,
    installation_id: &str,
) -> Result<SessionLease, SessionStoreError> {
    validate_component(id)?;
    validate_installation_id(installation_id)?;
    let lock = acquire_lock(root, id)?;
    let events = append_installation(root, id, installation_id)?;
    Ok(SessionLease {
        lock,
        events_path: events,
    })
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), SessionStoreError> {
    if !destination.exists() {
        fs::create_dir(destination)?;
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "session contains unsupported filesystem entry: {}",
                    source_path.display()
                ),
            )
            .into());
        }
    }
    Ok(())
}

pub fn list_session_ids(root: &Path) -> Result<Vec<String>, SessionStoreError> {
    let dir = sessions_dir(root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(id) = name.strip_suffix(".jsonl") {
            if session_dir(root, id).join(METADATA_FILE).is_file() {
                ids.push(id.to_owned());
            }
        }
    }
    ids.sort();
    Ok(ids)
}

pub fn copy_session(
    source_root: &Path,
    destination_root: &Path,
    id: &str,
    destination_installation_id: &str,
) -> Result<(), SessionStoreError> {
    validate_component(id)?;
    validate_installation_id(destination_installation_id)?;
    let _source_lock = acquire_lock(source_root, id)?;
    let _destination_lock = acquire_lock(destination_root, id)?;
    let source_dir = session_dir(source_root, id);
    let source_events = events_path(source_root, id);
    let destination_dir = session_dir(destination_root, id);
    let destination_events = events_path(destination_root, id);
    if !source_dir.is_dir() || !source_events.is_file() {
        return Err(SessionStoreError::Missing(id.to_owned()));
    }
    if destination_dir.exists() || destination_events.exists() {
        return Err(SessionStoreError::Collision(id.to_owned()));
    }
    let mut metadata = read_metadata(&source_dir, id)?;
    metadata
        .installation_history
        .push(destination_installation_id.to_owned());

    fs::create_dir_all(sessions_dir(destination_root))?;
    let staging_dir = tempfile::Builder::new()
        .prefix(&format!(".{id}.copying-"))
        .tempdir_in(sessions_dir(destination_root))?;
    copy_tree(&source_dir, staging_dir.path())?;
    atomic_write_json(&staging_dir.path().join(METADATA_FILE), &metadata)?;
    let mut staging_events = tempfile::NamedTempFile::new_in(sessions_dir(destination_root))?;
    io::copy(&mut File::open(source_events)?, &mut staging_events)?;
    staging_events.as_file().sync_all()?;
    staging_events
        .persist(&destination_events)
        .map_err(|error| error.error)?;
    if let Err(error) = fs::rename(staging_dir.keep(), &destination_dir) {
        let _ = fs::remove_file(&destination_events);
        return Err(error.into());
    }
    File::open(sessions_dir(destination_root))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_preserves_event_bytes_and_extends_provenance() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let lease = create_session(source.path(), "s1", "generation-a").unwrap();
        fs::write(lease.events_path(), b"opaque\0events\n").unwrap();
        drop(lease);
        copy_session(source.path(), destination.path(), "s1", "generation-b").unwrap();
        assert_eq!(
            fs::read(events_path(destination.path(), "s1")).unwrap(),
            b"opaque\0events\n"
        );
        assert_eq!(
            read_metadata(&session_dir(destination.path(), "s1"), "s1").unwrap(),
            SessionMetadata {
                created_with: "generation-a".into(),
                installation_history: vec!["generation-a".into(), "generation-b".into()],
            }
        );
        assert!(matches!(
            copy_session(source.path(), destination.path(), "s1", "generation-b"),
            Err(SessionStoreError::Collision(_))
        ));
    }

    #[test]
    fn successful_resume_appends_exactly_one_installation_id() {
        let root = tempfile::tempdir().unwrap();
        let lease = create_session(root.path(), "s1", "generation-a").unwrap();
        fs::write(lease.events_path(), b"events\n").unwrap();
        drop(lease);
        drop(resume_session(root.path(), "s1", "generation-b").unwrap());
        assert_eq!(
            read_metadata(&session_dir(root.path(), "s1"), "s1")
                .unwrap()
                .installation_history,
            vec!["generation-a", "generation-b"]
        );
    }

    #[test]
    fn failed_resume_does_not_create_or_change_metadata() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            resume_session(root.path(), "missing", "generation-a"),
            Err(SessionStoreError::Missing(_))
        ));
        assert!(!session_dir(root.path(), "missing").exists());
    }
}
