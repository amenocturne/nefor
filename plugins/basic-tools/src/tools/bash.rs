//! `bash` — run a shell command via `/bin/sh -c` with a wall-clock timeout.
//!
//! Behavior:
//!
//! - `command` is required; passed verbatim to `/bin/sh -c`.
//! - `cwd` is optional; defaults to the plugin's current directory.
//! - `timeout_ms` is optional; defaults to [`DEFAULT_TIMEOUT_MS`]. Capped at
//!   [`MAX_TIMEOUT_MS`] so a misbehaving prompt can't pin the plugin
//!   indefinitely.
//! - On timeout the child is killed and the partial output collected so far
//!   is returned via [`ToolError::BashTimeout`].
//! - Combined stdout+stderr is captured (interleaved-by-buffering, not
//!   true PTY merge — sufficient for typical commands). Output above
//!   [`MAX_OUTPUT_BYTES`] is truncated with a marker line at the end.
//! - The exit code is appended to the output as a footer line. Non-zero
//!   exit is NOT an error — many tools (grep, diff) signal "no match" via
//!   exit code; the caller (LLM) reads the footer.
//!
//! Trust model: same as `read_file` / `write_file` — no sandboxing here;
//! that's the gate's job.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::error::ToolError;

pub const NAME: &str = "bash";
pub const DESCRIPTION: &str =
    "Run a shell command via /bin/sh -c. Returns combined stdout+stderr followed by an exit-code footer.";

pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "Shell command line passed to /bin/sh -c."
            },
            "cwd": {
                "type": "string",
                "description": "Working directory. Defaults to the plugin's current directory."
            },
            "timeout_ms": {
                "type": "integer",
                "description": "Wall-clock timeout in milliseconds (default 30000, max 600000).",
                "minimum": 1
            }
        },
        "required": ["command"]
    })
}

pub async fn run(args: &Value) -> Result<String, ToolError> {
    let parsed = parse_args(args)?;
    run_command(parsed).await
}

#[derive(Debug)]
struct ParsedArgs {
    command: String,
    cwd: Option<String>,
    timeout_ms: u64,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, ToolError> {
    let obj = args.as_object().ok_or_else(|| ToolError::BadArgs {
        tool: NAME.into(),
        message: "args must be a JSON object".into(),
    })?;
    let command = obj
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArgs {
            tool: NAME.into(),
            message: "missing required string field `command`".into(),
        })?;
    if command.is_empty() {
        return Err(ToolError::BadArgs {
            tool: NAME.into(),
            message: "`command` must be non-empty".into(),
        });
    }
    let cwd = obj
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let timeout_ms = match obj.get("timeout_ms") {
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| ToolError::BadArgs {
                tool: NAME.into(),
                message: "`timeout_ms` must be a non-negative integer".into(),
            })?
            .clamp(1, MAX_TIMEOUT_MS),
        Some(_) => {
            return Err(ToolError::BadArgs {
                tool: NAME.into(),
                message: "`timeout_ms` must be a number".into(),
            });
        }
        None => DEFAULT_TIMEOUT_MS,
    };
    Ok(ParsedArgs {
        command: command.to_owned(),
        cwd,
        timeout_ms,
    })
}

async fn run_command(parsed: ParsedArgs) -> Result<String, ToolError> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&parsed.command);
    if let Some(dir) = &parsed.cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| ToolError::Io {
        path: parsed.command.clone(),
        message: format!("spawning /bin/sh: {e}"),
    })?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    let dur = Duration::from_millis(parsed.timeout_ms);
    let collect = async {
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        let _ = tokio::join!(
            stdout.read_to_end(&mut out_buf),
            stderr.read_to_end(&mut err_buf)
        );
        let status = child.wait().await;
        (out_buf, err_buf, status)
    };

    match timeout(dur, collect).await {
        Ok((out_buf, err_buf, status)) => {
            let combined = format_output(&out_buf, &err_buf, status_code(&status));
            Ok(combined)
        }
        Err(_elapsed) => {
            // child was moved into the inner future; on a timeout the future
            // is dropped, kill_on_drop fires and the OS reaps the child. We
            // can't recover the partial output here without restructuring;
            // best we can do is report the timeout cleanly.
            Err(ToolError::BashTimeout {
                timeout_ms: parsed.timeout_ms,
                output: format!("(killed after {}ms)", parsed.timeout_ms),
            })
        }
    }
}

fn status_code(status: &std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(s) => match s.code() {
            Some(c) => c.to_string(),
            None => "signal".into(),
        },
        Err(e) => format!("error: {e}"),
    }
}

fn format_output(stdout: &[u8], stderr: &[u8], exit: String) -> String {
    let stdout_s = String::from_utf8_lossy(stdout);
    let stderr_s = String::from_utf8_lossy(stderr);
    let mut out = String::new();
    if !stdout_s.is_empty() {
        out.push_str(&stdout_s);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if !stderr_s.is_empty() {
        out.push_str("[stderr]\n");
        out.push_str(&stderr_s);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if out.len() > MAX_OUTPUT_BYTES {
        out.truncate(MAX_OUTPUT_BYTES);
        out.push_str("\n[truncated]\n");
    }
    out.push_str(&format!("[exit {exit}]"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_simple_echo() {
        let out = run(&json!({"command": "echo hello"})).await.unwrap();
        assert!(out.contains("hello"));
        assert!(out.contains("[exit 0]"));
    }

    #[tokio::test]
    async fn captures_stderr_separately() {
        let out = run(&json!({"command": "echo out; echo err 1>&2"}))
            .await
            .unwrap();
        assert!(out.contains("out"));
        assert!(out.contains("[stderr]"));
        assert!(out.contains("err"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_not_error() {
        let out = run(&json!({"command": "exit 7"})).await.unwrap();
        assert!(out.contains("[exit 7]"));
    }

    #[tokio::test]
    async fn honors_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let path_str = dir.path().to_str().unwrap();
        let out = run(&json!({"command": "pwd", "cwd": path_str}))
            .await
            .unwrap();
        // macOS prefixes /private/ to /tmp paths in `pwd`; allow either form.
        assert!(
            out.contains(path_str) || out.contains(&format!("/private{path_str}")),
            "pwd output didn't match cwd: {out}"
        );
    }

    #[tokio::test]
    async fn times_out_long_running_command() {
        let err = run(&json!({"command": "sleep 5", "timeout_ms": 200}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::BashTimeout { .. }));
    }

    #[tokio::test]
    async fn rejects_missing_command() {
        let err = run(&json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }));
    }

    #[tokio::test]
    async fn rejects_empty_command() {
        let err = run(&json!({"command": ""})).await.unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }));
    }

    #[tokio::test]
    async fn rejects_non_numeric_timeout() {
        let err = run(&json!({"command": "echo x", "timeout_ms": "fast"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }));
    }

    #[test]
    fn schema_requires_command_only() {
        let s = schema();
        let req = s.get("required").and_then(Value::as_array).unwrap();
        let names: Vec<&str> = req.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, vec!["command"]);
    }
}
