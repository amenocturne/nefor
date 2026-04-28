//! `read_file` — read the textual contents of a file.
//!
//! Rejection policy (v1, no permission gating):
//!
//! - Path is missing → [`ToolError::NotFound`].
//! - Path is a directory → [`ToolError::IsDirectory`].
//! - File larger than 1 MiB → [`ToolError::TooLarge`].
//! - First 8 KiB contains a NUL byte (binary heuristic) → [`ToolError::BinaryContent`].
//! - Contents are not valid UTF-8 → [`ToolError::NotUtf8`].
//! - Any other IO error → [`ToolError::Io`].
//!
//! v1 deliberately does NOT validate path traversal or sandbox — the caller
//! passes whatever path they want, and basic-tools is trusted on the bus.
//! Sandboxing lands with the permission-gating story alongside `write_file`
//! and `bash`.

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::error::ToolError;

/// Wire name for this tool.
pub const NAME: &str = "read_file";

/// Human-readable description shipped to the LLM via the provider.
pub const DESCRIPTION: &str =
    "Read the contents of a file. Returns the file's text content or an error.";

/// 1 MiB cap on file size. Files larger than this are rejected — the LLM
/// can ask for a slice via a future `read_file_range` tool, or grep / head
/// via `bash` once that lands.
pub const MAX_BYTES: u64 = 1024 * 1024;

/// First N bytes inspected for a NUL byte to flag binary content.
pub const BINARY_PROBE_BYTES: usize = 8 * 1024;

/// JSON Schema (OpenAI tool-call format) for `read_file`'s parameters.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute or relative path to the file."
            }
        },
        "required": ["path"]
    })
}

/// Execute `read_file` with the given args. See module docs for rejection
/// rules.
pub async fn run(args: &Value) -> Result<String, ToolError> {
    let path = parse_path(args)?;
    read_text_file(&path).await
}

fn parse_path(args: &Value) -> Result<String, ToolError> {
    let obj = args.as_object().ok_or_else(|| ToolError::BadArgs {
        tool: NAME.into(),
        message: "args must be a JSON object".into(),
    })?;
    let raw = obj
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArgs {
            tool: NAME.into(),
            message: "missing required string field `path`".into(),
        })?;
    if raw.is_empty() {
        return Err(ToolError::BadArgs {
            tool: NAME.into(),
            message: "`path` must be non-empty".into(),
        });
    }
    Ok(raw.to_owned())
}

async fn read_text_file(path: &str) -> Result<String, ToolError> {
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolError::NotFound { path: path.into() });
        }
        Err(e) => {
            return Err(ToolError::Io {
                path: path.into(),
                message: e.to_string(),
            });
        }
    };

    if meta.is_dir() {
        return Err(ToolError::IsDirectory { path: path.into() });
    }

    let size = meta.len();
    if size > MAX_BYTES {
        return Err(ToolError::TooLarge {
            size,
            path: path.into(),
        });
    }

    let mut file = tokio::fs::File::open(path).await.map_err(|e| ToolError::Io {
        path: path.into(),
        message: e.to_string(),
    })?;

    // Probe the first BINARY_PROBE_BYTES for NUL bytes. If we find one,
    // bail early without slurping the whole file. This is the same
    // heuristic Git uses; it's cheap and catches the common case
    // (executables, images, archives) while letting unusual but legitimate
    // text files (UTF-16-with-BOM is a possible false positive — out of
    // scope for v1) through.
    let probe_cap = std::cmp::min(size as usize, BINARY_PROBE_BYTES);
    let mut probe = vec![0u8; probe_cap];
    let mut probe_read = 0usize;
    while probe_read < probe_cap {
        let n = file.read(&mut probe[probe_read..]).await.map_err(|e| ToolError::Io {
            path: path.into(),
            message: e.to_string(),
        })?;
        if n == 0 {
            break;
        }
        probe_read += n;
    }
    probe.truncate(probe_read);
    if probe.contains(&0u8) {
        return Err(ToolError::BinaryContent { path: path.into() });
    }

    // Read the remainder. We've already pulled `probe_read` bytes; concat
    // and slurp the rest. `read_to_end` would be simpler but would re-read
    // from the start; we use the existing handle to keep the probe data
    // and continue.
    let remaining_cap = (size as usize).saturating_sub(probe_read);
    let mut rest = Vec::with_capacity(remaining_cap);
    file.read_to_end(&mut rest).await.map_err(|e| ToolError::Io {
        path: path.into(),
        message: e.to_string(),
    })?;

    let mut all = probe;
    all.extend_from_slice(&rest);

    String::from_utf8(all).map_err(|_| ToolError::NotUtf8 { path: path.into() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn reads_utf8_contents() {
        let mut f = NamedTempFile::new().expect("tempfile");
        write!(f, "hello world").expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();
        let out = run(&json!({ "path": path })).await.expect("ok");
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn reads_empty_file() {
        let f = NamedTempFile::new().expect("tempfile");
        let path = f.path().to_str().expect("utf8 path").to_owned();
        let out = run(&json!({ "path": path })).await.expect("ok");
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn rejects_missing_path() {
        let err = run(&json!({ "path": "/definitely/does/not/exist/abcxyz" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf8 path").to_owned();
        let err = run(&json!({ "path": path })).await.unwrap_err();
        assert!(matches!(err, ToolError::IsDirectory { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_binary_content() {
        let mut f = NamedTempFile::new().expect("tempfile");
        // Write a NUL in the first 8 KiB.
        f.write_all(b"hello\0world").expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();
        let err = run(&json!({ "path": path })).await.unwrap_err();
        assert!(
            matches!(err, ToolError::BinaryContent { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_too_large() {
        let mut f = NamedTempFile::new().expect("tempfile");
        // 1 MiB + 1 byte of ASCII 'a'.
        let big = vec![b'a'; (MAX_BYTES as usize) + 1];
        f.write_all(&big).expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();
        let err = run(&json!({ "path": path })).await.unwrap_err();
        match err {
            ToolError::TooLarge { size, .. } => {
                assert_eq!(size, MAX_BYTES + 1);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn accepts_exactly_max_bytes() {
        let mut f = NamedTempFile::new().expect("tempfile");
        let buf = vec![b'a'; MAX_BYTES as usize];
        f.write_all(&buf).expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();
        let out = run(&json!({ "path": path })).await.expect("ok");
        assert_eq!(out.len(), MAX_BYTES as usize);
    }

    #[tokio::test]
    async fn rejects_invalid_utf8() {
        let mut f = NamedTempFile::new().expect("tempfile");
        // Valid-looking ASCII followed by a stray UTF-8 continuation byte.
        // No NUL so it doesn't trip the binary heuristic.
        f.write_all(&[b'h', b'i', 0xC3, 0x28]).expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();
        let err = run(&json!({ "path": path })).await.unwrap_err();
        assert!(matches!(err, ToolError::NotUtf8 { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_bad_args_no_path() {
        let err = run(&json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_bad_args_empty_path() {
        let err = run(&json!({ "path": "" })).await.unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_bad_args_non_object() {
        let err = run(&json!("just a string")).await.unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }), "got {err:?}");
    }

    #[test]
    fn schema_has_required_path() {
        let s = schema();
        assert_eq!(s.get("type").and_then(Value::as_str), Some("object"));
        let required = s.get("required").and_then(Value::as_array).expect("required");
        assert!(required.iter().any(|v| v.as_str() == Some("path")));
    }
}
