//! Session log persistence.
//!
//! The engine stamps every message it sees — inbound (plugin stdout) and
//! outbound (emitted by the Lua `step` function) — and appends them to a
//! per-session JSONL file under the XDG data directory. The first line is a
//! [`SessionHeader`] (marked with `_session: true`); subsequent lines are
//! [`LogEntry`] records.
//!
//! This module provides the types, the [`SessionWriter`] that owns the open
//! file, and the [`load_session`] reader used to hydrate a parent session on
//! resume. Broker integration lives elsewhere; nothing here talks to the
//! broker or to plugins directly.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use nefor_protocol::{PluginName, Timestamp};

/// Opaque session identifier. Carries a UUID v4 string by construction; the
/// wrapped string is deliberately not exposed as a field so parse-time
/// invariants (non-empty, valid UUID shape) can't be bypassed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Mint a fresh UUID v4.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Parse an existing session id, validating it is a well-formed UUID.
    /// Accepts any UUID version because we only need structural validity
    /// here — the origin (v4 vs. externally-supplied) is not load-bearing
    /// on the read path.
    pub fn parse(s: &str) -> Result<Self, SessionError> {
        let parsed = uuid::Uuid::parse_str(s).map_err(|e| SessionError::InvalidSessionId {
            raw: s.to_string(),
            reason: e.to_string(),
        })?;
        Ok(Self(parsed.to_string()))
    }

    /// View the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SessionId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        SessionId::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Where a given line in the log came from.
///
/// `Origin::Plugin(name)` serializes as the plugin's name; `Origin::Step`
/// serializes as the literal `"step"`. `PluginName` construction reserves
/// `"step"` so the two wire forms can never collide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Message came from a plugin's stdout (inbound).
    Plugin(PluginName),
    /// Message came from the Lua `step` function (outbound).
    Step,
}

const STEP_ORIGIN: &str = "step";

impl Serialize for Origin {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Origin::Plugin(name) => s.serialize_str(name.as_str()),
            Origin::Step => s.serialize_str(STEP_ORIGIN),
        }
    }
}

impl<'de> Deserialize<'de> for Origin {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        if raw == STEP_ORIGIN {
            return Ok(Origin::Step);
        }
        // On-wire deserialization must accept any plugin identity the engine
        // actually stamped, including reserved names like "engine" (which
        // `PluginName::new` rejects at spawn time but the protocol-level
        // deserializer still admits). Using the serde path matches that
        // behavior.
        let pn: PluginName =
            serde_json::from_value(Value::String(raw)).map_err(serde::de::Error::custom)?;
        Ok(Origin::Plugin(pn))
    }
}

/// One inbound or outbound message, as persisted to the session log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Engine-stamped wall clock (ISO-8601 UTC, ms precision).
    pub ts: Timestamp,
    /// Who produced the line.
    pub origin: Origin,
    /// For `Origin::Step` emissions, the intended plugin target (if any).
    /// `None` means broadcast or untargeted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PluginName>,
    /// Raw line text — verbatim, with no trailing newline.
    pub payload: String,
}

/// First-line header for a session log file.
///
/// Distinguished from [`LogEntry`] on disk by the `_session: true` marker,
/// which keeps the reader's parsing single-pass and unambiguous even if a
/// plugin payload happened to contain a `ts` / `origin` / `payload` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// Always `true` for a valid header. Present to disambiguate from
    /// entries on disk; the struct wouldn't need this field in memory, but
    /// serializing it unconditionally keeps read and write symmetric.
    #[serde(rename = "_session")]
    pub marker: bool,
    /// Id of this session.
    pub session_id: SessionId,
    /// Parent session whose log was replayed into this one, if any.
    #[serde(default)]
    pub parent_session: Option<SessionId>,
    /// When the session was opened.
    pub started_at: Timestamp,
}

impl SessionHeader {
    /// Build a header with the `_session: true` marker set.
    pub fn new(
        session_id: SessionId,
        parent_session: Option<SessionId>,
        started_at: Timestamp,
    ) -> Self {
        Self {
            marker: true,
            session_id,
            parent_session,
            started_at,
        }
    }
}

/// A fully-loaded session read back from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSession {
    /// First-line header.
    pub header: SessionHeader,
    /// All entries following the header, in file order.
    pub entries: Vec<LogEntry>,
}

/// Failures that can arise writing or reading a session log.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The OS did not expose a usable XDG data directory (no `$HOME`, etc.).
    #[error("could not resolve data directory for session log")]
    NoDataDir,
    /// Session id was syntactically invalid.
    #[error("invalid session id {raw:?}: {reason}")]
    InvalidSessionId {
        /// The raw string we tried to parse.
        raw: String,
        /// Lower-level parse reason.
        reason: String,
    },
    /// Session file did not exist on disk.
    #[error("session not found: {0}")]
    NotFound(SessionId),
    /// Session file existed but could not be parsed per the format invariant
    /// (missing header, malformed JSON, entry before header, etc.).
    #[error("malformed session file at {path}: {reason}")]
    Malformed {
        /// File we were reading.
        path: PathBuf,
        /// Human-readable parse reason.
        reason: String,
    },
    /// Underlying I/O failure reading or writing the session file.
    #[error("I/O error on session log at {path}: {source}")]
    Io {
        /// File the I/O was directed at.
        path: PathBuf,
        /// OS-level error.
        #[source]
        source: std::io::Error,
    },
    /// Failed to serialize a value to JSON on the write path. Logically
    /// should not happen for our closed-set types, but kept as a typed
    /// variant rather than papered over with `unwrap`.
    #[error("serialization failed for session log: {0}")]
    Serialize(#[source] serde_json::Error),
}

/// Resolve the on-disk path for the session log with the given id.
///
/// macOS: `~/Library/Application Support/nefor/sessions/<id>.jsonl`.
/// Linux: `~/.local/share/nefor/sessions/<id>.jsonl`.
/// Windows: `%APPDATA%/nefor/sessions/<id>.jsonl`.
pub fn session_log_path(id: &SessionId) -> Result<PathBuf, SessionError> {
    let data_dir = dirs::data_dir().ok_or(SessionError::NoDataDir)?;
    Ok(session_log_path_in(&data_dir, id))
}

/// Path assembly rooted at an explicit base directory, used by tests that
/// need to avoid touching the real XDG tree.
fn session_log_path_in(base: &Path, id: &SessionId) -> PathBuf {
    base.join("nefor")
        .join("sessions")
        .join(format!("{}.jsonl", id.as_str()))
}

/// Owns the open session file and appends serialized entries line-by-line.
///
/// Construction writes the header as the first line; subsequent [`append`]
/// calls write one entry per line. The `Drop` impl flushes the buffer as a
/// best-effort guard against losing the tail on a graceful shutdown path
/// that forgot to call [`flush`] explicitly.
///
/// [`append`]: SessionWriter::append
/// [`flush`]: SessionWriter::flush
pub struct SessionWriter {
    path: PathBuf,
    file: BufWriter<File>,
    entries_written: usize,
}

impl SessionWriter {
    /// Create (or truncate) the session file, write the header, and return
    /// a writer positioned after the header. Creates parent directories as
    /// needed.
    pub fn create(header: SessionHeader) -> Result<Self, SessionError> {
        let path = session_log_path(&header.session_id)?;
        Self::create_at(path, header)
    }

    /// Like [`create`], but rooted at a caller-supplied path. Exposed to
    /// tests so they can write to a tempdir without touching the real XDG
    /// tree.
    ///
    /// [`create`]: SessionWriter::create
    #[doc(hidden)]
    pub fn create_at(path: PathBuf, header: SessionHeader) -> Result<Self, SessionError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| SessionError::Io {
                path: path.clone(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|source| SessionError::Io {
                path: path.clone(),
                source,
            })?;
        let mut writer = BufWriter::new(file);
        let line = serde_json::to_string(&header).map_err(SessionError::Serialize)?;
        write_line(&mut writer, &line, &path)?;
        Ok(Self {
            path,
            file: writer,
            entries_written: 0,
        })
    }

    /// Append one entry. Serializes to JSON and writes a trailing newline.
    pub fn append(&mut self, entry: &LogEntry) -> Result<(), SessionError> {
        let line = serde_json::to_string(entry).map_err(SessionError::Serialize)?;
        write_line(&mut self.file, &line, &self.path)?;
        self.entries_written += 1;
        Ok(())
    }

    /// Flush the buffered writer to disk. Call this on clean shutdown; the
    /// `Drop` impl also flushes but cannot propagate errors.
    pub fn flush(&mut self) -> Result<(), SessionError> {
        self.file.flush().map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source,
        })
    }

    /// Path this writer is appending to. Useful for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many entries have been appended so far (header excluded).
    pub fn entries_written(&self) -> usize {
        self.entries_written
    }
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        if let Err(e) = self.file.flush() {
            // Best-effort flush — we can't return an error from Drop and we
            // don't want to panic during unwind. Tracing a warning lets the
            // operator notice while keeping shutdown ordering intact.
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "session writer failed to flush on drop"
            );
        }
    }
}

fn write_line(writer: &mut BufWriter<File>, line: &str, path: &Path) -> Result<(), SessionError> {
    writer
        .write_all(line.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })
}

/// Load a full session log (header + all entries) by id.
///
/// Returns [`SessionError::NotFound`] if the file is absent, and
/// [`SessionError::Malformed`] for structural problems (empty file, entry
/// before header, invalid JSON, header marker not set).
pub fn load_session(id: &SessionId) -> Result<LoadedSession, SessionError> {
    let path = session_log_path(id)?;
    load_session_at(path, id)
}

/// Reader variant rooted at an explicit path. Exposed to tests.
#[doc(hidden)]
pub fn load_session_at(path: PathBuf, id: &SessionId) -> Result<LoadedSession, SessionError> {
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SessionError::NotFound(id.clone()));
        }
        Err(source) => {
            return Err(SessionError::Io {
                path: path.clone(),
                source,
            });
        }
    };
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    let header_line = lines
        .next()
        .ok_or_else(|| SessionError::Malformed {
            path: path.clone(),
            reason: "empty file (no header line)".into(),
        })?
        .map_err(|source| SessionError::Io {
            path: path.clone(),
            source,
        })?;

    let header: SessionHeader =
        serde_json::from_str(&header_line).map_err(|e| SessionError::Malformed {
            path: path.clone(),
            reason: format!("header line is not a valid SessionHeader: {e}"),
        })?;
    if !header.marker {
        return Err(SessionError::Malformed {
            path: path.clone(),
            reason: "header line is missing the `_session: true` marker".into(),
        });
    }

    let mut entries = Vec::new();
    for (idx, line) in lines.enumerate() {
        let line = line.map_err(|source| SessionError::Io {
            path: path.clone(),
            source,
        })?;
        if line.is_empty() {
            continue;
        }
        let entry: LogEntry = serde_json::from_str(&line).map_err(|e| SessionError::Malformed {
            path: path.clone(),
            // +2: human line numbering + one for the header.
            reason: format!("line {} is not a valid LogEntry: {e}", idx + 2),
        })?;
        entries.push(entry);
    }

    Ok(LoadedSession { header, entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::TempDir;

    fn mk_plugin(name: &str) -> PluginName {
        PluginName::new(name).expect("valid plugin name")
    }

    fn mk_ts(s: &str) -> Timestamp {
        Timestamp::parse(s).expect("valid ts")
    }

    fn mk_entry(origin: Origin, target: Option<PluginName>, payload: &str) -> LogEntry {
        LogEntry {
            ts: mk_ts("2026-04-23T12:00:00.000Z"),
            origin,
            target,
            payload: payload.into(),
        }
    }

    fn tmp_session_path(dir: &TempDir, id: &SessionId) -> PathBuf {
        session_log_path_in(dir.path(), id)
    }

    #[test]
    fn session_id_roundtrip() {
        let id = SessionId::new();
        let parsed = SessionId::parse(id.as_str()).expect("roundtrip");
        assert_eq!(id, parsed);
    }

    #[test]
    fn session_id_rejects_malformed() {
        let err = SessionId::parse("not-a-uuid").expect_err("should reject junk");
        assert!(matches!(err, SessionError::InvalidSessionId { .. }));
    }

    #[test]
    fn session_writer_writes_header_as_first_line() {
        let dir = TempDir::new().expect("tempdir");
        let id = SessionId::new();
        let path = tmp_session_path(&dir, &id);
        let header = SessionHeader::new(id.clone(), None, mk_ts("2026-04-23T12:00:00.000Z"));
        {
            let _writer =
                SessionWriter::create_at(path.clone(), header.clone()).expect("create writer");
            // Drop flushes.
        }

        let mut raw = String::new();
        File::open(&path)
            .expect("open")
            .read_to_string(&mut raw)
            .expect("read");
        let first_line = raw.lines().next().expect("at least one line");
        let parsed: SessionHeader = serde_json::from_str(first_line).expect("header parses");
        assert!(parsed.marker);
        assert_eq!(parsed.session_id, id);
        assert_eq!(parsed.parent_session, None);
        // Explicit check that the wire form contains the marker.
        assert!(first_line.contains(r#""_session":true"#));
    }

    #[test]
    fn session_writer_appends_entries_in_order() {
        let dir = TempDir::new().expect("tempdir");
        let id = SessionId::new();
        let path = tmp_session_path(&dir, &id);
        let header = SessionHeader::new(id.clone(), None, mk_ts("2026-04-23T12:00:00.000Z"));
        let mut writer = SessionWriter::create_at(path.clone(), header).expect("create writer");
        let e1 = mk_entry(Origin::Plugin(mk_plugin("mock-plugin")), None, "hello");
        let e2 = mk_entry(Origin::Step, Some(mk_plugin("nefor-tui")), "render");
        let e3 = mk_entry(Origin::Plugin(mk_plugin("nefor-tui")), None, "keypress");
        writer.append(&e1).expect("append 1");
        writer.append(&e2).expect("append 2");
        writer.append(&e3).expect("append 3");
        writer.flush().expect("flush");
        drop(writer);

        let mut raw = String::new();
        File::open(&path)
            .expect("open")
            .read_to_string(&mut raw)
            .expect("read");
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 4, "1 header + 3 entries");
        let p1: LogEntry = serde_json::from_str(lines[1]).expect("entry 1");
        let p2: LogEntry = serde_json::from_str(lines[2]).expect("entry 2");
        let p3: LogEntry = serde_json::from_str(lines[3]).expect("entry 3");
        assert_eq!(p1, e1);
        assert_eq!(p2, e2);
        assert_eq!(p3, e3);
    }

    #[test]
    fn session_writer_serializes_origin_plugin_and_step() {
        let dir = TempDir::new().expect("tempdir");
        let id = SessionId::new();
        let path = tmp_session_path(&dir, &id);
        let header = SessionHeader::new(id.clone(), None, mk_ts("2026-04-23T12:00:00.000Z"));
        let mut writer = SessionWriter::create_at(path.clone(), header).expect("create");
        writer
            .append(&mk_entry(
                Origin::Plugin(mk_plugin("mock-plugin")),
                None,
                "x",
            ))
            .expect("append plugin");
        writer
            .append(&mk_entry(Origin::Step, None, "y"))
            .expect("append step");
        drop(writer);

        let mut raw = String::new();
        File::open(&path)
            .expect("open")
            .read_to_string(&mut raw)
            .expect("read");
        let lines: Vec<&str> = raw.lines().collect();
        assert!(
            lines[1].contains(r#""origin":"mock-plugin""#),
            "plugin origin serialized as plain string: {}",
            lines[1]
        );
        assert!(
            lines[2].contains(r#""origin":"step""#),
            "step origin serialized as literal \"step\": {}",
            lines[2]
        );
    }

    #[test]
    fn session_writer_drop_flushes_buffer() {
        let dir = TempDir::new().expect("tempdir");
        let id = SessionId::new();
        let path = tmp_session_path(&dir, &id);
        let header = SessionHeader::new(id.clone(), None, mk_ts("2026-04-23T12:00:00.000Z"));
        {
            let mut writer = SessionWriter::create_at(path.clone(), header).expect("create");
            writer
                .append(&mk_entry(Origin::Step, None, "will-I-make-it"))
                .expect("append");
            // No explicit flush — rely on Drop.
        }

        let loaded = load_session_at(path.clone(), &id).expect("loaded");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].payload, "will-I-make-it");
    }

    #[test]
    fn load_session_returns_not_found_for_missing_id() {
        let dir = TempDir::new().expect("tempdir");
        let id = SessionId::new();
        let path = tmp_session_path(&dir, &id);
        let err = load_session_at(path, &id).expect_err("should be missing");
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[test]
    fn load_session_parses_header_and_entries() {
        let dir = TempDir::new().expect("tempdir");
        let id = SessionId::new();
        let parent = SessionId::new();
        let started_at = mk_ts("2026-04-23T12:00:00.000Z");
        let path = tmp_session_path(&dir, &id);
        let header = SessionHeader::new(id.clone(), Some(parent.clone()), started_at);
        let entries = vec![
            mk_entry(Origin::Plugin(mk_plugin("mock-plugin")), None, "first"),
            mk_entry(Origin::Step, Some(mk_plugin("mock-plugin")), "second"),
        ];
        {
            let mut writer =
                SessionWriter::create_at(path.clone(), header.clone()).expect("create");
            for e in &entries {
                writer.append(e).expect("append");
            }
        }

        let loaded = load_session_at(path, &id).expect("load");
        assert_eq!(loaded.header, header);
        assert_eq!(loaded.entries, entries);
    }

    #[test]
    fn load_session_rejects_malformed() {
        let dir = TempDir::new().expect("tempdir");
        let id = SessionId::new();
        let path = tmp_session_path(&dir, &id);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "this is not JSON\n").expect("write garbage");
        let err = load_session_at(path, &id).expect_err("should be malformed");
        assert!(matches!(err, SessionError::Malformed { .. }));
    }

    #[test]
    fn load_session_rejects_header_without_marker() {
        let dir = TempDir::new().expect("tempdir");
        let id = SessionId::new();
        let path = tmp_session_path(&dir, &id);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        // Valid JSON shape but `_session` flipped to false.
        let bogus = format!(
            r#"{{"_session":false,"session_id":"{}","parent_session":null,"started_at":"2026-04-23T12:00:00.000Z"}}"#,
            id.as_str()
        );
        std::fs::write(&path, format!("{bogus}\n")).expect("write");
        let err = load_session_at(path, &id).expect_err("missing marker");
        assert!(matches!(err, SessionError::Malformed { .. }));
    }

    #[test]
    fn plugin_name_reserves_step() {
        // Reserved-name mechanism lives in nefor-protocol; this test pins
        // the session layer's expectation that `"step"` cannot be spawned
        // as a plugin name and thus cannot collide with `Origin::Step`.
        assert!(PluginName::new("step").is_err());
    }

    #[test]
    fn session_log_path_uses_xdg_data_dir() {
        // Verify the concrete path assembly used at runtime (nefor/sessions/
        // <id>.jsonl under dirs::data_dir) without touching the real XDG
        // tree: we reuse the internal `_in` helper with a controlled base,
        // then sanity-check the public wrapper matches the expected suffix.
        let dir = TempDir::new().expect("tempdir");
        let id = SessionId::new();
        let assembled = session_log_path_in(dir.path(), &id);
        assert_eq!(
            assembled,
            dir.path()
                .join("nefor")
                .join("sessions")
                .join(format!("{}.jsonl", id.as_str()))
        );

        // Public wrapper should produce the same suffix under whichever
        // real data dir the OS exposes; we tolerate the test environment
        // having no data_dir (CI sandbox) by only asserting the suffix when
        // we can compute the path at all.
        if let Ok(real) = session_log_path(&id) {
            let expected_suffix = Path::new("nefor")
                .join("sessions")
                .join(format!("{}.jsonl", id.as_str()));
            assert!(
                real.ends_with(&expected_suffix),
                "real path {:?} should end with {:?}",
                real,
                expected_suffix
            );
        }
    }
}
