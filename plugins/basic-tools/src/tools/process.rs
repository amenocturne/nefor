//! Canonical child-process execution for the structured and shell capabilities.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use crate::error::ToolError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    Unbounded,
    Bounded(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub argv: Vec<String>,
    pub cwd: String,
    pub timeout: Timeout,
    pub stdin: Option<String>,
}

pub fn parse_timeout(tool: &str, value: Option<&Value>) -> Result<Timeout, ToolError> {
    let value = value.ok_or_else(|| bad_args(tool, "missing required object field `timeout`"))?;
    let object = value
        .as_object()
        .ok_or_else(|| bad_args(tool, "`timeout` must be an object"))?;
    let present = object
        .get("present")
        .and_then(Value::as_bool)
        .ok_or_else(|| bad_args(tool, "`timeout.present` must be a boolean"))?;
    let milliseconds = object
        .get("milliseconds")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            bad_args(
                tool,
                "`timeout.milliseconds` must be a non-negative integer",
            )
        })?;
    match (present, milliseconds) {
        (false, 0) => Ok(Timeout::Unbounded),
        (false, _) => Err(bad_args(
            tool,
            "`timeout.milliseconds` must be 0 when `timeout.present` is false",
        )),
        (true, 0) => Err(bad_args(
            tool,
            "`timeout.milliseconds` must be positive when `timeout.present` is true",
        )),
        (true, milliseconds) => Ok(Timeout::Bounded(Duration::from_millis(milliseconds))),
    }
}

pub fn parse_common(
    tool: &str,
    object: &serde_json::Map<String, Value>,
) -> Result<(String, Timeout, Option<String>), ToolError> {
    let cwd = object
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .ok_or_else(|| bad_args(tool, "missing required non-empty string field `cwd`"))?
        .to_owned();
    let timeout = parse_timeout(tool, object.get("timeout"))?;
    let stdin = match object.get("stdin") {
        None | Some(Value::Null) => None,
        Some(Value::String(stdin)) => Some(stdin.clone()),
        Some(_) => return Err(bad_args(tool, "`stdin` must be a string")),
    };
    Ok((cwd, timeout, stdin))
}

pub async fn execute(
    request: Request,
    cancel: Option<oneshot::Receiver<()>>,
    stream: Option<mpsc::UnboundedSender<StreamChunk>>,
) -> Result<Value, ToolError> {
    let executable = request
        .argv
        .first()
        .cloned()
        .ok_or_else(|| bad_args("process.exec", "`argv` must be non-empty"))?;
    let mut command = Command::new(&executable);
    command.args(&request.argv[1..]);
    command.current_dir(&request.cwd);
    command.stdin(if request.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|source| ToolError::ProcessSpawn {
        executable: executable.clone(),
        message: source.to_string(),
    })?;
    let process_group = child.id();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().ok_or_else(|| ToolError::ProcessIo {
        operation: "opening stdout".into(),
        message: "spawned process did not provide its configured stdout pipe".into(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::ProcessIo {
        operation: "opening stderr".into(),
        message: "spawned process did not provide its configured stderr pipe".into(),
    })?;

    let stdout_task = tokio::spawn(read_pipe(stdout, stream.clone(), StreamKind::Stdout));
    let stderr_task = tokio::spawn(read_pipe(stderr, stream, StreamKind::Stderr));
    let stdin_task = tokio::spawn(feed_stdin(stdin, request.stdin));

    let outcome = wait_for_outcome(&mut child, request.timeout, cancel).await;
    if matches!(
        outcome,
        WaitOutcome::WaitFailed(_) | WaitOutcome::TimedOut(_) | WaitOutcome::Cancelled
    ) {
        kill_process_group(process_group);
        child.wait().await.map_err(|source| ToolError::ProcessIo {
            operation: "reaping process after termination".into(),
            message: source.to_string(),
        })?;
    }

    let stdout = join_reader(stdout_task, "reading stdout").await?;
    let stderr = join_reader(stderr_task, "reading stderr").await?;
    join_writer(stdin_task).await?;

    match outcome {
        WaitOutcome::Exited(status) => Ok(result_value(stdout, stderr, termination(&status))),
        WaitOutcome::WaitFailed(source) => Err(ToolError::ProcessIo {
            operation: "waiting for process".into(),
            message: source.to_string(),
        }),
        WaitOutcome::TimedOut(duration) => Err(ToolError::ProcessTimeout {
            timeout_ms: duration.as_millis() as u64,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }),
        WaitOutcome::Cancelled => Err(ToolError::ProcessCancelled {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }),
    }
}

enum WaitOutcome {
    Exited(ExitStatus),
    WaitFailed(std::io::Error),
    TimedOut(Duration),
    Cancelled,
}

async fn wait_for_outcome(
    child: &mut Child,
    timeout: Timeout,
    cancel: Option<oneshot::Receiver<()>>,
) -> WaitOutcome {
    let deadline = async move {
        match timeout {
            Timeout::Unbounded => std::future::pending::<Duration>().await,
            Timeout::Bounded(duration) => {
                tokio::time::sleep(duration).await;
                duration
            }
        }
    };
    let cancelled = async move {
        match cancel {
            Some(receiver) => {
                let _ = receiver.await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        status = child.wait() => match status {
            Ok(status) => WaitOutcome::Exited(status),
            Err(source) => WaitOutcome::WaitFailed(source),
        },
        duration = deadline => WaitOutcome::TimedOut(duration),
        _ = cancelled => WaitOutcome::Cancelled,
    }
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

async fn read_pipe<R>(
    mut reader: R,
    stream: Option<mpsc::UnboundedSender<StreamChunk>>,
    kind: StreamKind,
) -> Result<Vec<u8>, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut retained = Vec::new();
    let mut chunk = vec![0; 8192];
    loop {
        let size = reader.read(&mut chunk).await?;
        if size == 0 {
            return Ok(retained);
        }
        let bytes = chunk[..size].to_vec();
        retained.extend_from_slice(&bytes);
        if let Some(sender) = &stream {
            let event = match kind {
                StreamKind::Stdout => StreamChunk::Stdout(bytes),
                StreamKind::Stderr => StreamChunk::Stderr(bytes),
            };
            let _ = sender.send(event);
        }
    }
}

async fn feed_stdin(
    pipe: Option<tokio::process::ChildStdin>,
    input: Option<String>,
) -> Result<(), std::io::Error> {
    if let (Some(mut pipe), Some(input)) = (pipe, input) {
        pipe.write_all(input.as_bytes()).await?;
        pipe.shutdown().await?;
    }
    Ok(())
}

async fn join_reader(
    task: tokio::task::JoinHandle<Result<Vec<u8>, std::io::Error>>,
    operation: &str,
) -> Result<Vec<u8>, ToolError> {
    task.await
        .map_err(|source| ToolError::ProcessIo {
            operation: operation.into(),
            message: source.to_string(),
        })?
        .map_err(|source| ToolError::ProcessIo {
            operation: operation.into(),
            message: source.to_string(),
        })
}

async fn join_writer(
    task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
) -> Result<(), ToolError> {
    task.await
        .map_err(|source| ToolError::ProcessIo {
            operation: "writing stdin".into(),
            message: source.to_string(),
        })?
        .map_err(|source| ToolError::ProcessIo {
            operation: "writing stdin".into(),
            message: source.to_string(),
        })
}

fn kill_process_group(process_group: Option<u32>) {
    #[cfg(unix)]
    if let Some(process_group) = process_group {
        // The child was spawned as the leader of a dedicated process group.
        unsafe {
            libc::killpg(process_group as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = process_group;
}

fn result_value(stdout: Vec<u8>, stderr: Vec<u8>, termination: Value) -> Value {
    json!({
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "termination": termination,
    })
}

fn termination(status: &ExitStatus) -> Value {
    if let Some(code) = status.code() {
        return json!({ "kind": "code", "code": code });
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        json!({ "kind": "signal", "signal": status.signal() })
    }
    #[cfg(not(unix))]
    json!({ "kind": "unknown" })
}

pub fn bad_args(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::BadArgs {
        tool: tool.into(),
        message: message.into(),
    }
}
