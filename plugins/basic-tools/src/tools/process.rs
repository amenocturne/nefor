// Canonical child-process execution for the structured and shell capabilities.

use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

use crate::error::ToolError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

/// Pipe read boundaries are unrelated to UTF-8 boundaries. Only incomplete
/// suffixes are deferred; invalid complete sequences retain the terminal
/// result's whole-buffer lossy replacement policy.
#[derive(Default)]
pub struct StreamDecoder {
    pending: Vec<u8>,
}

impl StreamDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut output = String::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(text) => {
                    output.push_str(text);
                    consumed = self.pending.len();
                }
                Err(error) => {
                    let valid_end = consumed + error.valid_up_to();
                    output.push_str(&String::from_utf8_lossy(&self.pending[consumed..valid_end]));
                    consumed = valid_end;
                    match error.error_len() {
                        Some(length) => {
                            output.push('�');
                            consumed += length;
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        output
    }

    pub fn finish(&mut self) -> String {
        let output = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        output
    }
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
    preview: Option<Arc<LivePreview>>,
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

    let stdout_task = tokio::spawn(read_pipe(stdout, preview.clone(), StreamKind::Stdout));
    let stderr_task = tokio::spawn(read_pipe(stderr, preview, StreamKind::Stderr));
    let stdin_task = tokio::spawn(feed_stdin(stdin, request.stdin));

    let cancelled = async move {
        match cancel {
            Some(receiver) => {
                let _ = receiver.await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(cancelled);
    let mut outcome = wait_for_outcome(&mut child, request.timeout, &mut cancelled).await;
    let reap = if !matches!(outcome, WaitOutcome::Exited(_)) {
        kill_process_group(process_group);
        child
            .wait()
            .await
            .map(|_| ())
            .map_err(|source| ToolError::ProcessIo {
                operation: "reaping process after termination".into(),
                message: source.to_string(),
            })
    } else {
        Ok(())
    };

    // A leader may exit while descendants still hold the pipes. Cancellation
    // must retain process-group authority until all pipe tasks are joined.
    let readers = async {
        let stdout = join_reader(stdout_task, "reading stdout").await;
        let stderr = join_reader(stderr_task, "reading stderr").await;
        let stdin = join_writer(stdin_task).await;
        Ok::<_, ToolError>((stdout?, stderr?, stdin?))
    };
    tokio::pin!(readers);
    let (stdout, stderr, ()) = if matches!(outcome, WaitOutcome::Exited(_)) {
        tokio::select! {
            result = &mut readers => result?,
            _ = &mut cancelled => {
                kill_process_group(process_group);
                outcome = WaitOutcome::Cancelled;
                readers.await?
            }
        }
    } else {
        readers.await?
    };

    reap?;
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
    cancelled: &mut (impl std::future::Future<Output = ()> + Unpin),
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
    preview: Option<Arc<LivePreview>>,
    kind: StreamKind,
) -> Result<Vec<u8>, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut retained = RetainedOutput::default();
    let mut chunk = vec![0; 8192];
    loop {
        let size = reader.read(&mut chunk).await?;
        if size == 0 {
            return Ok(retained.into_bytes());
        }
        let bytes = &chunk[..size];
        retained.push(bytes);
        if let Some(preview) = &preview {
            preview.push(kind, bytes);
        }
    }
}

/// Unforwarded live output kept per stream between preview drains; older
/// bytes are skipped and reported by count.
pub const PREVIEW_WINDOW_BYTES: usize = 16 * 1024;

/// Live process output that a consumer drains on its own schedule. The child
/// never waits on it, and its memory stays bounded however fast the child
/// writes.
#[derive(Default)]
pub struct LivePreview {
    stdout: Mutex<PreviewBuffer>,
    stderr: Mutex<PreviewBuffer>,
}

impl LivePreview {
    fn push(&self, kind: StreamKind, bytes: &[u8]) {
        lock(self.buffer(kind)).push(bytes);
    }

    /// Output written since the previous call, at most one chunk per stream.
    pub fn take(&self) -> Vec<StreamChunk> {
        let stdout = lock(&self.stdout).take().map(StreamChunk::Stdout);
        let stderr = lock(&self.stderr).take().map(StreamChunk::Stderr);
        stdout.into_iter().chain(stderr).collect()
    }

    fn buffer(&self, kind: StreamKind) -> &Mutex<PreviewBuffer> {
        match kind {
            StreamKind::Stdout => &self.stdout,
            StreamKind::Stderr => &self.stderr,
        }
    }
}

#[derive(Default)]
struct PreviewBuffer {
    pending: Vec<u8>,
    skipped: u64,
}

impl PreviewBuffer {
    fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        // Trim lazily so the buffer is compacted once per window of input.
        if self.pending.len() > 2 * PREVIEW_WINDOW_BYTES {
            self.trim();
        }
    }

    fn trim(&mut self) {
        if self.pending.len() > PREVIEW_WINDOW_BYTES {
            let excess = self.pending.len() - PREVIEW_WINDOW_BYTES;
            self.pending.drain(..excess);
            self.skipped += excess as u64;
        }
    }

    fn take(&mut self) -> Option<Vec<u8>> {
        self.trim();
        if self.pending.is_empty() && self.skipped == 0 {
            return None;
        }
        let mut bytes = if self.skipped > 0 {
            format!("[... {} bytes skipped in live output ...]\n", self.skipped).into_bytes()
        } else {
            Vec::new()
        };
        bytes.append(&mut self.pending);
        self.skipped = 0;
        Some(bytes)
    }
}

fn lock(buffer: &Mutex<PreviewBuffer>) -> MutexGuard<'_, PreviewBuffer> {
    buffer.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Bytes of each stream kept from the start and from the end of the output.
/// JSON escaping can grow a byte up to sixfold, so both streams at twice this
/// bound must stay below the engine's 16 MiB NCP line limit.
pub const RETAINED_EDGE_BYTES: usize = 512 * 1024;

/// Head and tail of one output stream; the middle of an oversized stream is
/// dropped so the tool result always fits in a single NCP frame.
#[derive(Default)]
struct RetainedOutput {
    head: Vec<u8>,
    tail: Vec<u8>,
    omitted: u64,
}

impl RetainedOutput {
    fn push(&mut self, bytes: &[u8]) {
        let head_room = RETAINED_EDGE_BYTES.saturating_sub(self.head.len());
        let (head, rest) = bytes.split_at(head_room.min(bytes.len()));
        self.head.extend_from_slice(head);
        self.tail.extend_from_slice(rest);
        // Trim lazily so the tail is compacted once per edge-size of input.
        if self.tail.len() > 2 * RETAINED_EDGE_BYTES {
            let excess = self.tail.len() - RETAINED_EDGE_BYTES;
            self.tail.drain(..excess);
            self.omitted += excess as u64;
        }
    }

    fn into_bytes(mut self) -> Vec<u8> {
        if self.tail.len() > RETAINED_EDGE_BYTES {
            let excess = self.tail.len() - RETAINED_EDGE_BYTES;
            self.tail.drain(..excess);
            self.omitted += excess as u64;
        }
        let mut bytes = self.head;
        if self.omitted > 0 {
            bytes.extend_from_slice(
                format!("\n[... {} bytes of output omitted ...]\n", self.omitted).as_bytes(),
            );
        }
        bytes.extend_from_slice(&self.tail);
        bytes
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

#[cfg(test)]
mod decoder_tests {
    use super::StreamDecoder;

    #[test]
    fn every_partition_matches_whole_buffer_lossy() {
        for bytes in [
            "Ж€😀".as_bytes(),
            b"a\xf0\x9f\xff\xd0",
            b"\xe2\x82x\xff\xc0\xaf",
        ] {
            for mask in 0..(1usize << (bytes.len() - 1)) {
                let mut decoder = StreamDecoder::default();
                let mut actual = String::new();
                let mut start = 0;
                for end in 1..=bytes.len() {
                    if end == bytes.len() || mask & (1 << (end - 1)) != 0 {
                        actual.push_str(&decoder.push(&bytes[start..end]));
                        start = end;
                    }
                }
                actual.push_str(&decoder.finish());
                assert_eq!(actual, String::from_utf8_lossy(bytes), "mask={mask}");
                assert_eq!(decoder.finish(), "");
            }
        }
    }

    #[test]
    fn interleaved_streams_keep_independent_suffixes() {
        let stdout = "Привет😀".as_bytes();
        let stderr = b"\xe2\x82\xac\xd0";
        for a in 0..=stdout.len() {
            for b in 0..=stderr.len() {
                let mut out = StreamDecoder::default();
                let mut err = StreamDecoder::default();
                let mut out_text = out.push(&stdout[..a]);
                let mut err_text = err.push(&stderr[..b]);
                out_text.push_str(&out.push(&stdout[a..]));
                err_text.push_str(&err.push(&stderr[b..]));
                out_text.push_str(&out.finish());
                err_text.push_str(&err.finish());
                assert_eq!(out_text, String::from_utf8_lossy(stdout));
                assert_eq!(err_text, String::from_utf8_lossy(stderr));
            }
        }
    }
}
