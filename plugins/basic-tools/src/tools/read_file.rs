// `read_file` — read the textual contents of a file.
//
// Rejection policy (v1, no permission gating):
//
// - Path is missing → [`ToolError::NotFound`].
// - Path is a directory → [`ToolError::IsDirectory`].
// - Path is not a regular file → [`ToolError::NotRegularFile`].
// - Unsliced file larger than the configured maximum → [`ToolError::TooLarge`].
// - First 8 KiB contains a NUL byte (binary heuristic) → [`ToolError::BinaryContent`].
// - Contents are not valid UTF-8 → [`ToolError::NotUtf8`].
// - Any other IO error → [`ToolError::Io`].
//
// v1 deliberately does NOT validate path traversal or sandbox — the caller
// passes whatever path they want, and basic-tools is trusted on the bus.
// Sandboxing lands with the permission-gating story alongside `write_file`
// and the process capabilities.

use std::sync::OnceLock;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::error::ToolError;

/// Wire name for this tool.
pub const NAME: &str = "read_file";

/// Human-readable description shipped to the LLM via the provider.
pub const DESCRIPTION: &str =
    "Read the UTF-8 contents of a regular file. Returns the file's text content or an error.";

/// The maximum is supplied by composition at process startup.
static CONFIGURED_MAX_BYTES: OnceLock<u64> = OnceLock::new();

pub fn configure_max_bytes(max_read_bytes: u64) -> Result<(), String> {
    if max_read_bytes == 0 {
        return Err("read_file maximum must be positive".into());
    }
    CONFIGURED_MAX_BYTES
        .set(max_read_bytes)
        .map_err(|_| "read_file maximum was configured more than once".into())
}

pub(crate) fn configured_max_bytes() -> Option<u64> {
    CONFIGURED_MAX_BYTES.get().copied()
}

/// First N bytes inspected for a NUL byte to flag binary content.
pub const BINARY_PROBE_BYTES: usize = 8 * 1024;

/// JSON Schema (OpenAI tool-call format) for `read_file`'s parameters.
pub fn schema() -> Value {
    schema_with_limit(configured_max_bytes())
}

pub fn schema_with_limit(max_read_bytes: Option<u64>) -> Value {
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
                "description": max_read_bytes
                    .map(|limit| format!("Maximum bytes to read. Optional; maximum {limit} bytes."))
                    .unwrap_or_else(|| "Maximum bytes to read. Optional; bounded by tool configuration.".into()),
                "maximum": max_read_bytes
            }
        },
        "required": ["path"]
    })
}

/// Declarative presentation metadata advertised with this tool.
pub fn display() -> Value {
    json!({
        "compact": { "label": "read file", "primary": { "label": "path", "select": { "source": "args", "path": "path" }, "kind": "path" } },
        "expanded": { "label": "read file", "fields": [
            { "label": "offset", "select": { "source": "args", "path": "offset" }, "kind": "scalar", "omit": "missing" },
            { "label": "max bytes", "select": { "source": "args", "path": "max_bytes" }, "kind": "bytes", "omit": "missing" }
        ] },
        "result": { "kind": "content", "fields": [
            { "label": "content", "select": { "source": "result", "path": "$" }, "kind": "text", "max_lines": 20, "max_bytes": 1600 }
        ] }
    })
}

/// Execute `read_file` with the given args. See module docs for rejection
/// rules.
pub async fn run(args: &Value, max_read_bytes: Option<u64>) -> Result<String, ToolError> {
    let max_read_bytes = max_read_bytes.ok_or_else(|| ToolError::BadArgs {
        tool: NAME.into(),
        message: "read_file maximum is not configured".into(),
    })?;
    let request = parse_args(args, max_read_bytes)?;
    read_text_file(request, max_read_bytes).await
}

#[derive(Debug)]
struct ReadRequest {
    path: String,
    offset: u64,
    max_bytes: u64,
    sliced: bool,
}

fn parse_args(args: &Value, max_read_bytes: u64) -> Result<ReadRequest, ToolError> {
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
        Some(Value::Number(n)) => {
            let requested = n.as_u64().ok_or_else(|| ToolError::BadArgs {
                tool: NAME.into(),
                message: "`max_bytes` must be a positive integer".into(),
            })?;
            if requested == 0 {
                return Err(ToolError::BadArgs {
                    tool: NAME.into(),
                    message: "`max_bytes` must be a positive integer".into(),
                });
            }
            if requested > max_read_bytes {
                return Err(ToolError::BadArgs {
                    tool: NAME.into(),
                    message: format!(
                        "`max_bytes` must not exceed the configured maximum of {max_read_bytes} bytes (requested {requested})"
                    ),
                });
            }
            requested
        }
        Some(_) => {
            return Err(ToolError::BadArgs {
                tool: NAME.into(),
                message: "`max_bytes` must be an integer".into(),
            });
        }
        None => max_read_bytes,
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

async fn read_text_file(request: ReadRequest, max_read_bytes: u64) -> Result<String, ToolError> {
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
    if !meta.is_file() {
        return Err(ToolError::NotRegularFile { path });
    }

    let size = meta.len();
    if !request.sliced && size > max_read_bytes {
        return Err(ToolError::TooLarge {
            size,
            cap: max_read_bytes,
            path,
        });
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
            max_read_bytes
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
