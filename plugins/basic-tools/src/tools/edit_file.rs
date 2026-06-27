//! `edit_file` — edit an existing UTF-8 file by exact string replacement.
//!
//! This is a narrow mutation primitive: callers provide the exact text
//! to replace and the replacement text. Whole-file overwrites stay in
//! `write_file`; multi-file patches can grow into a separate patch tool
//! later.

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::error::ToolError;

pub const NAME: &str = "edit_file";
pub const DESCRIPTION: &str =
    "Modify an existing text file by replacing one exact string match with new text.";

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute or relative path to the existing file."
            },
            "old_string": {
                "type": "string",
                "description": "Exact text to replace. Must occur exactly once unless policy.require_unique_match is false."
            },
            "new_string": {
                "type": "string",
                "description": "Replacement text. Must differ from old_string."
            },
            "cwd": {
                "type": "string",
                "description": "Working directory. Relative paths are resolved against this."
            },
            "policy": {
                "type": "object",
                "description": "Runtime-supplied edit behavior. Models should not set this directly.",
                "properties": {
                    "require_unique_match": {
                        "type": "boolean",
                        "description": "Require old_string to occur exactly once. Defaults to true."
                    }
                }
            }
        },
        "required": ["path", "old_string", "new_string"]
    })
}

pub async fn run(args: &Value) -> Result<String, ToolError> {
    let parsed = parse_args(args)?;
    edit_text_file(parsed).await
}

#[derive(Debug)]
struct ParsedArgs {
    path: String,
    old_string: String,
    new_string: String,
    policy: EditPolicy,
}

#[derive(Debug)]
struct EditPolicy {
    require_unique_match: bool,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, ToolError> {
    let obj = args.as_object().ok_or_else(|| ToolError::BadArgs {
        tool: NAME.into(),
        message: "args must be a JSON object".into(),
    })?;
    let raw_path = required_string(obj, "path")?;
    if raw_path.is_empty() {
        return Err(bad_args("`path` must be non-empty"));
    }
    let old_string = required_string(obj, "old_string")?;
    if old_string.is_empty() {
        return Err(bad_args("`old_string` must be non-empty"));
    }
    let new_string = required_string(obj, "new_string")?;
    if old_string == new_string {
        return Err(bad_args("`old_string` and `new_string` must differ"));
    }
    let cwd = obj
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    Ok(ParsedArgs {
        path: resolve_path(raw_path, cwd),
        old_string: old_string.to_owned(),
        new_string: new_string.to_owned(),
        policy: parse_policy(obj.get("policy"))?,
    })
}

fn required_string<'a>(
    obj: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str, ToolError> {
    obj.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| bad_args(&format!("missing required string field `{field}`")))
}

fn parse_policy(value: Option<&Value>) -> Result<EditPolicy, ToolError> {
    let mut policy = EditPolicy {
        require_unique_match: true,
    };
    let Some(value) = value else {
        return Ok(policy);
    };
    let obj = value
        .as_object()
        .ok_or_else(|| bad_args("`policy` must be an object"))?;
    if let Some(v) = obj.get("require_unique_match") {
        policy.require_unique_match = v
            .as_bool()
            .ok_or_else(|| bad_args("`policy.require_unique_match` must be boolean"))?;
    }
    Ok(policy)
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

async fn edit_text_file(parsed: ParsedArgs) -> Result<String, ToolError> {
    let meta = tokio::fs::metadata(&parsed.path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ToolError::NotFound {
                path: parsed.path.clone(),
            }
        } else {
            ToolError::Io {
                path: parsed.path.clone(),
                message: e.to_string(),
            }
        }
    })?;
    if meta.is_dir() {
        return Err(ToolError::IsDirectory { path: parsed.path });
    }

    let bytes = tokio::fs::read(&parsed.path)
        .await
        .map_err(|e| ToolError::Io {
            path: parsed.path.clone(),
            message: e.to_string(),
        })?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Err(ToolError::BinaryContent { path: parsed.path });
    }
    let old_content = String::from_utf8(bytes).map_err(|_| ToolError::NotUtf8 {
        path: parsed.path.clone(),
    })?;

    let occurrences = old_content.matches(&parsed.old_string).count();
    if occurrences == 0 {
        return Err(bad_args("`old_string` was not found in the file"));
    }
    if parsed.policy.require_unique_match && occurrences > 1 {
        return Err(bad_args(
            "`old_string` matched multiple locations; provide more surrounding context",
        ));
    }

    let new_content = old_content.replacen(&parsed.old_string, &parsed.new_string, 1);

    let mut file = tokio::fs::File::create(&parsed.path)
        .await
        .map_err(|e| ToolError::Io {
            path: parsed.path.clone(),
            message: e.to_string(),
        })?;
    file.write_all(new_content.as_bytes())
        .await
        .map_err(|e| ToolError::Io {
            path: parsed.path.clone(),
            message: e.to_string(),
        })?;
    file.flush().await.map_err(|e| ToolError::Io {
        path: parsed.path.clone(),
        message: e.to_string(),
    })?;

    Ok(format!(
        "edited {}; changed {} line(s), byte delta {}",
        parsed.path,
        changed_lines(&old_content, &new_content),
        byte_delta(old_content.len(), new_content.len())
    ))
}

fn changed_lines(old_content: &str, new_content: &str) -> usize {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();
    let shared = old_lines
        .iter()
        .zip(new_lines.iter())
        .filter(|(a, b)| a == b)
        .count();
    (old_lines.len() - shared) + (new_lines.len() - shared)
}

fn byte_delta(old_len: usize, new_len: usize) -> usize {
    old_len.abs_diff(new_len)
}

fn bad_args(message: &str) -> ToolError {
    ToolError::BadArgs {
        tool: NAME.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn replaces_unique_exact_match() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
        let out = run(&json!({
            "path": path.to_str().unwrap(),
            "old_string": "beta",
            "new_string": "BETA"
        }))
        .await
        .unwrap();
        assert!(out.contains("edited"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn rejects_missing_match() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "alpha\n").unwrap();
        let err = run(&json!({
            "path": path.to_str().unwrap(),
            "old_string": "beta",
            "new_string": "BETA"
        }))
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }));
    }

    #[tokio::test]
    async fn rejects_ambiguous_match_by_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "x\nx\n").unwrap();
        let err = run(&json!({
            "path": path.to_str().unwrap(),
            "old_string": "x",
            "new_string": "y"
        }))
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }));
    }

    #[tokio::test]
    async fn permits_large_exact_replacements() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        run(&json!({
            "path": path.to_str().unwrap(),
            "old_string": "a\nb\nc",
            "new_string": "A\nB\nC"
        }))
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nB\nC\n");
    }
}
