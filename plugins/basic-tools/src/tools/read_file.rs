//! `read_file` — read the textual contents of a file.
//!
//! Rejection policy (v1, no permission gating):
//!
//! - Path is missing → [`ToolError::NotFound`].
//! - Path is a directory → [`ToolError::IsDirectory`].
//! - Unsliced file larger than 1 MiB → [`ToolError::TooLarge`].
//! - First 8 KiB contains a NUL byte (binary heuristic) → [`ToolError::BinaryContent`].
//! - Contents are not valid UTF-8 → [`ToolError::NotUtf8`].
//! - Any other IO error → [`ToolError::Io`].
//!
//! v1 deliberately does NOT validate path traversal or sandbox — the caller
//! passes whatever path they want, and basic-tools is trusted on the bus.
//! Sandboxing lands with the permission-gating story alongside `write_file`
//! and `bash`.

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::error::ToolError;

/// Wire name for this tool.
pub const NAME: &str = "read_file";

/// Human-readable description shipped to the LLM via the provider.
pub const DESCRIPTION: &str =
    "Read the contents of a file. Returns the file's text content or an error.";

/// 1 MiB cap on a single read. Unsliced files larger than this are
/// rejected; sliced reads may target larger files but never return more
/// than this many bytes at once.
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
            },
            "cwd": {
                "type": "string",
                "description": "Working directory. Relative paths are resolved against this."
            },
            "offset": {
                "type": "integer",
                "description": "Byte offset to start reading from. Optional; defaults to 0."
            },
            "max_bytes": {
                "type": "integer",
                "description": "Maximum bytes to read. Optional; capped at 1 MiB."
            }
        },
        "required": ["path"]
    })
}

/// Execute `read_file` with the given args. See module docs for rejection
/// rules.
pub async fn run(args: &Value) -> Result<String, ToolError> {
    let request = parse_args(args)?;
    read_text_file(request).await
}

#[derive(Debug)]
struct ReadRequest {
    path: String,
    offset: u64,
    max_bytes: u64,
    sliced: bool,
}

fn parse_args(args: &Value) -> Result<ReadRequest, ToolError> {
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
    let cwd = obj
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    let offset = match obj.get("offset") {
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| ToolError::BadArgs {
            tool: NAME.into(),
            message: "`offset` must be a non-negative integer".into(),
        })?,
        Some(_) => {
            return Err(ToolError::BadArgs {
                tool: NAME.into(),
                message: "`offset` must be an integer".into(),
            });
        }
        None => 0,
    };

    let max_bytes = match obj.get("max_bytes") {
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| ToolError::BadArgs {
                tool: NAME.into(),
                message: "`max_bytes` must be a positive integer".into(),
            })?
            .clamp(4, MAX_BYTES),
        Some(_) => {
            return Err(ToolError::BadArgs {
                tool: NAME.into(),
                message: "`max_bytes` must be an integer".into(),
            });
        }
        None => MAX_BYTES,
    };

    Ok(ReadRequest {
        path: resolve_path(raw, cwd),
        offset,
        max_bytes,
        sliced: offset > 0 || obj.get("max_bytes").is_some(),
    })
}

fn resolve_path(path: &str, cwd: Option<&str>) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return path.to_owned();
    }
    match cwd {
        Some(dir) => std::path::Path::new(dir)
            .join(p)
            .to_string_lossy()
            .into_owned(),
        None => path.to_owned(),
    }
}

async fn read_text_file(request: ReadRequest) -> Result<String, ToolError> {
    let path = request.path;
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolError::NotFound { path });
        }
        Err(e) => {
            return Err(ToolError::Io {
                path,
                message: e.to_string(),
            });
        }
    };

    if meta.is_dir() {
        return Err(ToolError::IsDirectory { path });
    }

    let size = meta.len();
    if !request.sliced && size > MAX_BYTES {
        return Err(ToolError::TooLarge { size, path });
    }

    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| ToolError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;

    if request.offset >= size {
        if !request.sliced {
            return Ok(String::new());
        }
        return Ok(format!(
            "[read_file slice: bytes {size}..{size} of {size}]\n"
        ));
    }

    let start = seek_to_next_utf8_boundary(&mut file, request.offset, size, &path).await?;
    let read_limit = request.max_bytes.min(size.saturating_sub(start));

    // Probe the first BINARY_PROBE_BYTES for NUL bytes. If we find one,
    // bail early without slurping the whole file. This is the same
    // heuristic Git uses; it's cheap and catches the common case
    // (executables, images, archives) while letting unusual but legitimate
    // text files (UTF-16-with-BOM is a possible false positive — out of
    // scope for v1) through.
    let probe_cap = std::cmp::min(read_limit as usize, BINARY_PROBE_BYTES);
    let mut probe = vec![0u8; probe_cap];
    let mut probe_read = 0usize;
    while probe_read < probe_cap {
        let n = file
            .read(&mut probe[probe_read..])
            .await
            .map_err(|e| ToolError::Io {
                path: path.clone(),
                message: e.to_string(),
            })?;
        if n == 0 {
            break;
        }
        probe_read += n;
    }
    probe.truncate(probe_read);
    if probe.contains(&0u8) {
        return Err(ToolError::BinaryContent { path });
    }

    // Read the remainder of the requested slice. We've already pulled
    // `probe_read` bytes; keep the same handle and continue from there.
    let remaining_cap = (read_limit as usize).saturating_sub(probe_read);
    let mut rest = Vec::with_capacity(remaining_cap);
    file.take(remaining_cap as u64)
        .read_to_end(&mut rest)
        .await
        .map_err(|e| ToolError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;

    let mut all = probe;
    all.extend_from_slice(&rest);
    let valid_len = valid_utf8_prefix_len(&all, &path)?;
    all.truncate(valid_len);

    let text = String::from_utf8(all).map_err(|_| ToolError::NotUtf8 { path: path.clone() })?;
    if !request.sliced {
        return Ok(text);
    }

    let end = start + valid_len as u64;
    let mut out = format!("[read_file slice: bytes {start}..{end} of {size}]\n{text}");
    if end < size {
        out.push_str(&format!(
            "\n[... file continues; next offset: {end}; max_bytes cap: {}]",
            MAX_BYTES
        ));
    }
    Ok(out)
}

async fn seek_to_next_utf8_boundary(
    file: &mut tokio::fs::File,
    mut offset: u64,
    size: u64,
    path: &str,
) -> Result<u64, ToolError> {
    while offset < size {
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| ToolError::Io {
                path: path.into(),
                message: e.to_string(),
            })?;
        let mut one = [0u8; 1];
        let n = file.read(&mut one).await.map_err(|e| ToolError::Io {
            path: path.into(),
            message: e.to_string(),
        })?;
        if n == 0 || !is_utf8_continuation(one[0]) {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| ToolError::Io {
                    path: path.into(),
                    message: e.to_string(),
                })?;
            return Ok(offset);
        }
        offset += 1;
    }

    file.seek(std::io::SeekFrom::Start(size))
        .await
        .map_err(|e| ToolError::Io {
            path: path.into(),
            message: e.to_string(),
        })?;
    Ok(size)
}

fn is_utf8_continuation(b: u8) -> bool {
    (0x80..=0xBF).contains(&b)
}

fn valid_utf8_prefix_len(bytes: &[u8], path: &str) -> Result<usize, ToolError> {
    match std::str::from_utf8(bytes) {
        Ok(_) => Ok(bytes.len()),
        Err(e) if e.error_len().is_none() => Ok(e.valid_up_to()),
        Err(_) => Err(ToolError::NotUtf8 { path: path.into() }),
    }
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
    async fn sliced_read_allows_large_file_in_bounded_chunks() {
        let mut f = NamedTempFile::new().expect("tempfile");
        let big = format!("{}END", "a".repeat((MAX_BYTES as usize) + 10));
        f.write_all(big.as_bytes()).expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();

        let out = run(&json!({
            "path": path,
            "offset": MAX_BYTES + 5,
            "max_bytes": 16
        }))
        .await
        .expect("slice ok");

        assert!(out.contains("[read_file slice: bytes "));
        assert!(out.contains("aaaaaEND"));
        assert!(!out.contains("file too large"));
    }

    #[tokio::test]
    async fn sliced_read_reports_next_offset_when_file_continues() {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(b"0123456789abcdef").expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();

        let out = run(&json!({
            "path": path,
            "offset": 2,
            "max_bytes": 5
        }))
        .await
        .expect("slice ok");

        assert!(out.contains("[read_file slice: bytes 2..7 of 16]"));
        assert!(out.contains("23456"));
        assert!(out.contains("next offset: 7"));
    }

    #[tokio::test]
    async fn sliced_read_does_not_split_utf8_at_boundaries() {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all("aa€b".as_bytes()).expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();

        let first = run(&json!({
            "path": path,
            "offset": 0,
            "max_bytes": 4
        }))
        .await
        .expect("first slice ok");
        assert!(first.contains("\naa"));
        assert!(first.contains("next offset: 2"));

        let second = run(&json!({
            "path": path,
            "offset": 3,
            "max_bytes": 4
        }))
        .await
        .expect("second slice ok");
        assert!(second.contains("[read_file slice: bytes 5..6 of 6]"));
        assert!(second.contains("\nb"));
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
        let required = s
            .get("required")
            .and_then(Value::as_array)
            .expect("required");
        assert!(required.iter().any(|v| v.as_str() == Some("path")));
    }
}
