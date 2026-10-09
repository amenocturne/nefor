//
// With no `old_string`, the complete file is written from `new_string`,
// creating parents as needed. With `old_string`, one exact unique match in an
// existing UTF-8 file is replaced. An empty `new_string` is valid in both
// forms (truncate the file, or delete the matched text).
//
// Trust model matches `read_file` v1: basic-tools is trusted on the bus.
// Path-traversal / sandboxing decisions live in the gate, not here.

use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::error::ToolError;

pub const NAME: &str = "write_file";
pub async fn run(args: &Value) -> Result<String, ToolError> {
    let parsed = parse_args(args)?;
    match parsed.old_string {
        Some(old_string) => edit_text_file(&parsed.path, &old_string, &parsed.new_string).await,
        None => write_text_file(&parsed.path, &parsed.new_string).await,
    }
}

struct ParsedArgs {
    path: String,
    new_string: String,
    old_string: Option<String>,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, ToolError> {
    let obj = args.as_object().ok_or_else(|| ToolError::BadArgs {
        tool: NAME.into(),
        message: "args must be a JSON object".into(),
    })?;
    let raw_path = obj
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArgs {
            tool: NAME.into(),
            message: "missing required string field `path`".into(),
        })?;
    if raw_path.is_empty() {
        return Err(ToolError::BadArgs {
            tool: NAME.into(),
            message: "`path` must be non-empty".into(),
        });
    }
    let new_string = obj
        .get("new_string")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArgs {
            tool: NAME.into(),
            message: "missing required string field `new_string`".into(),
        })?;
    let old_string = match obj.get("old_string") {
        None => None,
        Some(Value::String(value)) if value.is_empty() => {
            return Err(bad_args("`old_string` must be non-empty when present"));
        }
        Some(Value::String(value)) => Some(value.to_owned()),
        Some(_) => return Err(bad_args("`old_string` must be a string when present")),
    };
    if old_string.as_deref() == Some(new_string) {
        return Err(bad_args("`old_string` and `new_string` must differ"));
    }
    Ok(ParsedArgs {
        path: raw_path.to_owned(),
        new_string: new_string.to_owned(),
        old_string,
    })
}

fn bad_args(message: &str) -> ToolError {
    ToolError::BadArgs {
        tool: NAME.into(),
        message: message.into(),
    }
}

async fn write_text_file(path: &str, content: &str) -> Result<String, ToolError> {
    // If the path already exists as a directory, refuse — overwriting a
    // directory with file bytes is never the intent and produces a
    // confusing IO error.
    if let Ok(meta) = tokio::fs::metadata(path).await {
        if meta.is_dir() {
            return Err(ToolError::IsDirectory { path: path.into() });
        }
    }

    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Io {
                    path: path.into(),
                    message: format!("creating parent directory: {e}"),
                })?;
        }
    }

    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| ToolError::Io {
            path: path.into(),
            message: e.to_string(),
        })?;
    file.write_all(content.as_bytes())
        .await
        .map_err(|e| ToolError::Io {
            path: path.into(),
            message: e.to_string(),
        })?;
    file.flush().await.map_err(|e| ToolError::Io {
        path: path.into(),
        message: e.to_string(),
    })?;

    Ok(format!("wrote {} bytes to {}", content.len(), path))
}

async fn edit_text_file(
    path: &str,
    old_string: &str,
    new_string: &str,
) -> Result<String, ToolError> {
    let meta = tokio::fs::metadata(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ToolError::NotFound { path: path.into() }
        } else {
            ToolError::Io {
                path: path.into(),
                message: e.to_string(),
            }
        }
    })?;
    if meta.is_dir() {
        return Err(ToolError::IsDirectory { path: path.into() });
    }
    let bytes = tokio::fs::read(path).await.map_err(|e| ToolError::Io {
        path: path.into(),
        message: e.to_string(),
    })?;
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Err(ToolError::BinaryContent { path: path.into() });
    }
    let old_content =
        String::from_utf8(bytes).map_err(|_| ToolError::NotUtf8 { path: path.into() })?;
    match old_content.matches(old_string).count() {
        0 => return Err(bad_args("`old_string` was not found in the file")),
        1 => {}
        _ => {
            return Err(bad_args(
                "`old_string` matched multiple locations; provide more surrounding context",
            ))
        }
    }
    let new_content = old_content.replacen(old_string, new_string, 1);
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| ToolError::Io {
            path: path.into(),
            message: e.to_string(),
        })?;
    file.write_all(new_content.as_bytes())
        .await
        .map_err(|e| ToolError::Io {
            path: path.into(),
            message: e.to_string(),
        })?;
    file.flush().await.map_err(|e| ToolError::Io {
        path: path.into(),
        message: e.to_string(),
    })?;
    Ok(format!(
        "edited {}; changed {} line(s), byte delta {}",
        path,
        changed_lines(&old_content, &new_content),
        byte_delta(old_content.len(), new_content.len())
    ))
}

fn changed_lines(old_content: &str, new_content: &str) -> usize {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();
    let shared = old_lines.len().min(new_lines.len());
    old_lines
        .iter()
        .zip(new_lines.iter())
        .take(shared)
        .filter(|(old, new)| old != new)
        .count()
        + old_lines.len().max(new_lines.len())
        - shared
}

fn byte_delta(old_len: usize, new_len: usize) -> isize {
    new_len as isize - old_len as isize
}
