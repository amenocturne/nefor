//! `read_image` — read image bytes for vision-capable providers.
//!
//! The tool only loads and classifies bytes. It does not OCR, caption, or
//! downsample; interpretation belongs to the model layer. Providers that
//! cannot send image parts must turn the structured media result into an
//! explicit user-visible error before the next model turn.

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::error::ToolError;

/// Wire name for this tool.
pub const NAME: &str = "read_image";

/// Human-readable description shipped to the LLM via the provider.
pub const DESCRIPTION: &str =
    "Read an image file for visual inspection. Returns image bytes and metadata; only vision-capable models can use the result.";

/// 5 MiB cap on a single image read.
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;

/// JSON Schema (OpenAI tool-call format) for `read_image`'s parameters.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute or relative path to the image file."
            },
            "cwd": {
                "type": "string",
                "description": "Working directory. Relative paths are resolved against this."
            }
        },
        "required": ["path"]
    })
}

/// Execute `read_image` with the given args.
pub async fn run(args: &Value) -> Result<Value, ToolError> {
    let request = parse_args(args)?;
    read_image_file(request).await
}

#[derive(Debug)]
struct ReadImageRequest {
    path: String,
}

fn parse_args(args: &Value) -> Result<ReadImageRequest, ToolError> {
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
    Ok(ReadImageRequest {
        path: resolve_path(raw, cwd),
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

async fn read_image_file(request: ReadImageRequest) -> Result<Value, ToolError> {
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
    if size > MAX_BYTES {
        return Err(ToolError::ImageTooLarge {
            size,
            cap: MAX_BYTES,
            path,
        });
    }

    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| ToolError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
    let mut bytes = Vec::with_capacity(size as usize);
    file.read_to_end(&mut bytes)
        .await
        .map_err(|e| ToolError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(ToolError::ImageTooLarge {
            size: bytes.len() as u64,
            cap: MAX_BYTES,
            path,
        });
    }

    let media_type = detect_image_mime(&bytes)
        .ok_or_else(|| ToolError::UnsupportedImage { path: path.clone() })?;
    let filename = std::path::Path::new(&path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&path)
        .to_owned();

    Ok(json!({
        "type": "media",
        "media_type": media_type,
        "filename": filename,
        "data": encode_base64(&bytes),
    }))
}

fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b2 & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn reads_png_as_media_object() {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(b"\x89PNG\r\n\x1a\nabc").expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();
        let out = run(&json!({ "path": path })).await.expect("ok");
        assert_eq!(out.get("type").and_then(Value::as_str), Some("media"));
        assert_eq!(
            out.get("media_type").and_then(Value::as_str),
            Some("image/png")
        );
        assert_eq!(
            out.get("data").and_then(Value::as_str),
            Some("iVBORw0KGgphYmM=")
        );
    }

    #[tokio::test]
    async fn rejects_unsupported_image_format() {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(b"not an image").expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();
        let err = run(&json!({ "path": path })).await.unwrap_err();
        assert!(
            matches!(err, ToolError::UnsupportedImage { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn resolves_relative_path_against_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("image.gif");
        std::fs::write(&path, b"GIF89a").expect("write");
        let out = run(&json!({
            "path": "image.gif",
            "cwd": dir.path().to_str().expect("utf8 cwd")
        }))
        .await
        .expect("ok");
        assert_eq!(
            out.get("media_type").and_then(Value::as_str),
            Some("image/gif")
        );
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
