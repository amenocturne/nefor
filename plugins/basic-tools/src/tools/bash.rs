//! `bash` — run a shell command via `/bin/sh -c`, optionally time-bounded.
//!
//! Behavior:
//!
//! - `command` is required; passed verbatim to `/bin/sh -c`.
//! - `stdin` is optional; when present it is written to the command's
//!   standard input (then the pipe closes). Absent, stdin is null — the
//!   pre-existing behavior.
//! - `cwd` is optional; defaults to the plugin's current directory.
//! - `timeout_ms` is optional; ABSENT means the command runs until it exits.
//!   Long-running commands (review UIs, dev servers, watch loops) are
//!   first-class: runs are observable and interruptible through the kernel,
//!   so an unbounded default costs nothing — the caller opts INTO a bound
//!   when a hang would otherwise go unnoticed.
//! - On timeout the child is killed and the call returns
//!   [`ToolError::BashTimeout`].
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
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::error::ToolError;

pub const NAME: &str = "bash";
pub const DESCRIPTION: &str =
    "Run a shell command via /bin/sh -c. Returns combined stdout+stderr followed by an exit-code footer.";

pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "Shell command line passed to /bin/sh -c."
            },
            "stdin": {
                "type": "string",
                "description": "Text piped to the command's standard input. Omit for no stdin."
            },
            "cwd": {
                "type": "string",
                "description": "Working directory. Defaults to the plugin's current directory."
            },
            "timeout_ms": {
                "type": "integer",
                "description": "Optional wall-clock timeout in milliseconds. Omit to let the command run until it exits.",
                "minimum": 1
            }
        },
        "required": ["command"]
    })
}

/// Declarative presentation metadata advertised with this tool.
pub fn display() -> Value {
    json!({
        "label": "Run command",
        "primary": { "arg": "command" },
        "arguments": [
            { "label": "in", "arg": "cwd" },
            { "label": "timeout ms", "arg": "timeout_ms" }
        ],
        "result": { "kind": "content" }
    })
}

pub async fn run(args: &Value) -> Result<String, ToolError> {
    run_cancellable(args, None).await
}

/// Run `bash`, but abortable: when `cancel` fires (the double-Esc interrupt
/// path — main.rs forwards `basic-tools.tool.cancel` for the in-flight invoke
/// id), the child's whole process group is killed so grandchildren (the
/// `sleep` inside `sleep 10 && echo`) die too, and the call returns
/// [`ToolError::Cancelled`]. A `None` receiver is the ordinary, un-cancellable
/// invocation.
pub async fn run_cancellable(
    args: &Value,
    cancel: Option<oneshot::Receiver<()>>,
) -> Result<String, ToolError> {
    let parsed = parse_args(args)?;
    run_command(parsed, cancel).await
}

#[derive(Debug)]
struct ParsedArgs {
    command: String,
    stdin: Option<String>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
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
    let stdin = match obj.get("stdin") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            return Err(ToolError::BadArgs {
                tool: NAME.into(),
                message: "`stdin` must be a string".into(),
            });
        }
    };
    let cwd = obj
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let timeout_ms = match obj.get("timeout_ms") {
        Some(Value::Number(n)) => {
            Some(
                n.as_u64()
                    .filter(|ms| *ms >= 1)
                    .ok_or_else(|| ToolError::BadArgs {
                        tool: NAME.into(),
                        message: "`timeout_ms` must be a positive integer".into(),
                    })?,
            )
        }
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(ToolError::BadArgs {
                tool: NAME.into(),
                message: "`timeout_ms` must be a number".into(),
            });
        }
    };
    Ok(ParsedArgs {
        command: command.to_owned(),
        stdin,
        cwd,
        timeout_ms,
    })
}

async fn run_command(
    parsed: ParsedArgs,
    cancel: Option<oneshot::Receiver<()>>,
) -> Result<String, ToolError> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&parsed.command);
    if let Some(dir) = &parsed.cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(if parsed.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    // Put the child in its OWN process group (leader, so pgid == child pid).
    // A bare `kill_on_drop` only SIGKILLs the direct `/bin/sh`; the shell's
    // own children (`sleep` in `sleep 10 && echo`) would survive as orphans —
    // exactly the incident. A group of its own lets the cancel path `killpg`
    // the whole subtree at once.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().map_err(|e| ToolError::Io {
        path: parsed.command.clone(),
        message: format!("spawning /bin/sh: {e}"),
    })?;
    // Capture the pid (== pgid, group leader) before the child is moved into
    // the collect future, so the cancel arm can signal the group.
    let child_pid = child.id();
    let stdin_pipe = child.stdin.take();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    let stdin_text = parsed.stdin.clone();
    let collect = async move {
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        // Write stdin concurrently with the output reads (a large input
        // against a full pipe buffer must not deadlock), then drop the handle
        // so the child sees EOF.
        let feed = async {
            if let (Some(mut pipe), Some(text)) = (stdin_pipe, stdin_text) {
                use tokio::io::AsyncWriteExt;
                let _ = pipe.write_all(text.as_bytes()).await;
                let _ = pipe.shutdown().await;
            }
        };
        let _ = tokio::join!(
            feed,
            stdout.read_to_end(&mut out_buf),
            stderr.read_to_end(&mut err_buf)
        );
        let status = child.wait().await;
        (out_buf, err_buf, status)
    };

    // Pends forever when there is no cancel channel, so the select collapses
    // to the plain timeout path for an ordinary invocation.
    let cancelled = async move {
        match cancel {
            Some(rx) => {
                let _ = rx.await;
            }
            None => std::future::pending::<()>().await,
        }
    };

    // No timeout requested → the command runs until it exits (the collect
    // future is awaited unbounded; cancellation stays available).
    let bounded = async {
        match parsed.timeout_ms {
            Some(ms) => timeout(Duration::from_millis(ms), collect)
                .await
                .map_err(|_elapsed| ms),
            None => Ok(collect.await),
        }
    };

    tokio::select! {
        result = bounded => match result {
            Ok((out_buf, err_buf, status)) => {
                Ok(format_output(&out_buf, &err_buf, status_code(&status)))
            }
            Err(ms) => {
                // child was moved into the inner future; on a timeout the
                // future is dropped, kill_on_drop fires and the OS reaps the
                // child. We can't recover the partial output here without
                // restructuring; best we can do is report the timeout cleanly.
                Err(ToolError::BashTimeout {
                    timeout_ms: ms,
                    output: format!("(killed after {ms}ms)"),
                })
            }
        },
        _ = cancelled => {
            // Kill the whole process group (the shell AND its children). The
            // collect future is dropped when this arm wins, so kill_on_drop
            // also SIGKILLs the shell; killpg is what reaches the grandchildren.
            #[cfg(unix)]
            if let Some(pid) = child_pid {
                // Safe: a bare kill(2)-family syscall on an integer pid.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            let _ = child_pid; // referenced on non-unix to avoid a warning
            Err(ToolError::Cancelled)
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
    async fn feeds_stdin_to_the_command() {
        let out = run(&json!({"command": "sort", "stdin": "b\na\n"}))
            .await
            .unwrap();
        assert!(out.starts_with("a\nb\n"), "sorted stdin: {out}");
        assert!(out.contains("[exit 0]"));
    }

    #[tokio::test]
    async fn rejects_non_string_stdin() {
        let err = run(&json!({"command": "cat", "stdin": 42}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }));
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
    async fn rejects_zero_timeout() {
        let err = run(&json!({"command": "echo x", "timeout_ms": 0}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }));
    }

    #[tokio::test]
    async fn rejects_non_numeric_timeout() {
        let err = run(&json!({"command": "echo x", "timeout_ms": "fast"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }));
    }

    #[tokio::test]
    async fn cancel_kills_the_process_group_before_the_child_completes() {
        use tokio::sync::oneshot;

        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let marker_str = marker.to_str().unwrap().to_owned();
        // The shell sleeps, THEN touches the marker. If cancel only killed the
        // `/bin/sh` and left `sleep` orphaned, the marker would appear after
        // the sleep elapses. A process-group kill takes the `sleep` with it,
        // so the marker never lands.
        let cmd = format!("sleep 2 && touch {marker_str}");

        let (tx, rx) = oneshot::channel();
        let handle =
            tokio::spawn(
                async move { run_cancellable(&json!({ "command": cmd }), Some(rx)).await },
            );

        // Let the child spawn, then cancel well before the sleep would finish.
        tokio::time::sleep(Duration::from_millis(300)).await;
        tx.send(()).unwrap();

        let result = handle.await.unwrap();
        assert!(
            matches!(result, Err(ToolError::Cancelled)),
            "cancel must return ToolError::Cancelled, got: {result:?}"
        );

        // Wait past the original sleep: a surviving orphan would touch it here.
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(
            !marker.exists(),
            "the child process group must be dead — the sleep's touch must never land"
        );
    }

    #[tokio::test]
    async fn none_cancel_receiver_runs_normally() {
        let out = run_cancellable(&json!({ "command": "echo hi" }), None)
            .await
            .unwrap();
        assert!(out.contains("hi"));
        assert!(out.contains("[exit 0]"));
    }

    #[test]
    fn schema_requires_command_only() {
        let s = schema();
        let req = s.get("required").and_then(Value::as_array).unwrap();
        let names: Vec<&str> = req.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, vec!["command"]);
    }
}
