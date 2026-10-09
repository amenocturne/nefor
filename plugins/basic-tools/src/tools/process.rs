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
pub struct PreviewChunk {
    pub bytes: Vec<u8>,
    /// Raw source bytes lost immediately before `bytes`, since the last drain.
    /// Zero means this span is contiguous with the preceding drain.
    pub skipped_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamChunk {
    Stdout(PreviewChunk),
    Stderr(PreviewChunk),
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

    /// End the contiguous segment before rendering a gap. Never feed omission
    /// markers into this decoder: a pending suffix belongs to the old segment.
    pub fn discontinuity(&mut self) -> String {
        self.finish()
    }

    /// Flush a partial EOF suffix lossily once; subsequent calls are empty.
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
    let deadline = async {
        match request.timeout {
            Timeout::Unbounded => std::future::pending::<Duration>().await,
            Timeout::Bounded(duration) => {
                tokio::time::sleep(duration).await;
                duration
            }
        }
    };
    tokio::pin!(deadline);
    let mut outcome = wait_for_outcome(&mut child, &mut deadline, &mut cancelled).await;
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
                drain_terminated_group(process_group, &mut readers).await?
            }
            duration = &mut deadline => {
                kill_process_group(process_group);
                outcome = WaitOutcome::TimedOut(duration);
                drain_terminated_group(process_group, &mut readers).await?
            }
        }
    } else {
        drain_terminated_group(process_group, &mut readers).await?
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

// A shell can fork concurrently with killpg: a descendant in the still-owned
// group can survive the first signal and retain the pipes after leader reap.
// Keep terminating that group until its pipe/stdin tasks finish; do not abort
// the readers and discard their partial captures or detach surviving work.
async fn drain_terminated_group<T>(
    process_group: Option<u32>,
    readers: &mut (impl std::future::Future<Output = T> + Unpin),
) -> T {
    let mut retry = tokio::time::interval(Duration::from_millis(10));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            result = &mut *readers => return result,
            _ = retry.tick() => kill_process_group(process_group),
        }
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
    deadline: &mut (impl std::future::Future<Output = Duration> + Unpin),
    cancelled: &mut (impl std::future::Future<Output = ()> + Unpin),
) -> WaitOutcome {
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
/// writes. With 8192-byte pipe reads, pending logical length is at most 32KiB
/// per stream after push (40KiB transiently); a drain keeps at most 16KiB.
/// Vec capacities, decoding/serialization temporaries, RSS and concurrency are
/// separate from these per-stream logical bounds.
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

    fn take(&mut self) -> Option<PreviewChunk> {
        self.trim();
        if self.pending.is_empty() && self.skipped == 0 {
            return None;
        }
        Some(PreviewChunk {
            bytes: std::mem::take(&mut self.pending),
            skipped_bytes: std::mem::take(&mut self.skipped),
        })
    }
}

fn lock(buffer: &Mutex<PreviewBuffer>) -> MutexGuard<'_, PreviewBuffer> {
    buffer.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Bytes of each stream kept from the start and from the end of the output.
/// This is a raw payload bound, not full-frame admission: IDs, escaping and
/// the outer envelope remain the checked publisher's responsibility.
pub const RETAINED_EDGE_BYTES: usize = 512 * 1024;

/// Head and tail of one output stream; the middle of an oversized stream is
/// dropped with an exact raw-byte count. Cuts may split UTF-8 characters.
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

#[cfg(test)]
mod buffer_contract_tests {
    use super::*;

    #[test]
    fn every_read_and_timer_drain_partition_preserves_contiguous_utf8() {
        let bytes = "Ж€😀".as_bytes();
        for reads in 0..(1usize << (bytes.len() - 1)) {
            for drains in 0..(1usize << bytes.len()) {
                let mut buffer = PreviewBuffer::default();
                let mut decoder = StreamDecoder::default();
                let mut text = String::new();
                let mut start = 0;
                for end in 1..=bytes.len() {
                    if end == bytes.len() || reads & (1 << (end - 1)) != 0 {
                        buffer.push(&bytes[start..end]);
                        start = end;
                    }
                    if drains & (1 << (end - 1)) != 0 {
                        if let Some(chunk) = buffer.take() {
                            assert_eq!(chunk.skipped_bytes, 0);
                            text.push_str(&decoder.push(&chunk.bytes));
                        }
                    }
                }
                if let Some(chunk) = buffer.take() {
                    text.push_str(&decoder.push(&chunk.bytes));
                }
                text.push_str(&decoder.finish());
                assert_eq!(text, String::from_utf8_lossy(bytes));
            }
        }
    }

    #[test]
    fn gap_ends_pending_suffix_before_new_segment() {
        for suffix in [b"\xe2".as_slice(), b"\xe2\x82", b"\xf0\x9f\x98"] {
            let mut decoder = StreamDecoder::default();
            assert_eq!(decoder.push(suffix), "");
            let mut buffer = PreviewBuffer::default();
            buffer.push(&vec![b'x'; PREVIEW_WINDOW_BYTES + 1]);
            buffer.push(b"\xac");
            let chunk = buffer.take().unwrap();
            assert_eq!(chunk.skipped_bytes, 2);
            assert_eq!(decoder.discontinuity(), String::from_utf8_lossy(suffix));
            let text = decoder.push(&chunk.bytes) + &decoder.finish();
            assert_eq!(text, String::from_utf8_lossy(&chunk.bytes));
            assert!(!text.contains('€'));
            assert_eq!(decoder.finish(), "");
        }
    }

    #[test]
    fn invalid_and_suffix_only_eof_flush_exactly_once() {
        let mut decoder = StreamDecoder::default();
        assert_eq!(decoder.push(b"a\xff\xe2\x82"), "a�");
        assert_eq!(decoder.push(b""), "");
        assert_eq!(decoder.finish(), "�");
        assert_eq!(decoder.finish(), "");
    }

    #[test]
    fn literal_markers_are_data_and_stream_gaps_are_independent() {
        let preview = LivePreview::default();
        let literal = b"[... 42 bytes skipped in live output ...]\n\xe2";
        preview.push(StreamKind::Stdout, literal);
        preview.push(StreamKind::Stderr, &vec![b'e'; PREVIEW_WINDOW_BYTES + 7]);
        let chunks = preview.take();
        assert_eq!(
            chunks[0],
            StreamChunk::Stdout(PreviewChunk {
                bytes: literal.to_vec(),
                skipped_bytes: 0,
            })
        );
        assert_eq!(
            chunks[1],
            StreamChunk::Stderr(PreviewChunk {
                bytes: vec![b'e'; PREVIEW_WINDOW_BYTES],
                skipped_bytes: 7,
            })
        );
        preview.push(StreamKind::Stdout, b"\x82\xac");
        assert_eq!(
            preview.take(),
            vec![StreamChunk::Stdout(PreviewChunk {
                bytes: b"\x82\xac".to_vec(),
                skipped_bytes: 0,
            })]
        );
        assert!(preview.take().is_empty());
        let mut gap_only = PreviewBuffer {
            pending: Vec::new(),
            skipped: 11,
        };
        assert_eq!(
            gap_only.take(),
            Some(PreviewChunk {
                bytes: vec![],
                skipped_bytes: 11
            })
        );
        assert_eq!(gap_only.take(), None);
    }

    #[test]
    fn preview_thresholds_and_repeated_drains_account_for_every_raw_byte() {
        for size in [
            0,
            1,
            PREVIEW_WINDOW_BYTES - 1,
            PREVIEW_WINDOW_BYTES,
            PREVIEW_WINDOW_BYTES + 1,
            2 * PREVIEW_WINDOW_BYTES,
            2 * PREVIEW_WINDOW_BYTES + 1,
            7 * PREVIEW_WINDOW_BYTES + 19,
        ] {
            for stride in [1, 127, 8192] {
                let source: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
                let mut buffer = PreviewBuffer::default();
                for chunk in source.chunks(stride) {
                    buffer.push(chunk);
                    assert!(buffer.pending.len() <= 2 * PREVIEW_WINDOW_BYTES);
                }
                if let Some(chunk) = buffer.take() {
                    let kept = size.min(PREVIEW_WINDOW_BYTES);
                    assert_eq!(chunk.bytes, source[size - kept..]);
                    assert_eq!(chunk.skipped_bytes as usize + kept, size);
                } else {
                    assert_eq!(size, 0);
                }
                assert!(buffer.take().is_none());
                buffer.push(b"next");
                assert_eq!(buffer.take().unwrap().skipped_bytes, 0);
            }
        }
    }

    #[test]
    fn retained_thresholds_counts_and_raw_utf8_cuts_match_reference() {
        let edge = RETAINED_EDGE_BYTES;
        for size in [
            edge - 1,
            edge,
            edge + 1,
            2 * edge - 1,
            2 * edge,
            2 * edge + 1,
            3 * edge + 17,
            5 * edge + 3,
        ] {
            let source: Vec<u8> = "€😀Ж"
                .as_bytes()
                .iter()
                .copied()
                .cycle()
                .take(size)
                .collect();
            for stride in [127, 8192] {
                let mut retained = RetainedOutput::default();
                let mut total = 0;
                for chunk in source.chunks(stride) {
                    total += chunk.len();
                    retained.push(chunk);
                    assert!(retained.head.len() <= edge);
                    assert!(retained.tail.len() <= 2 * edge);
                    assert_eq!(
                        retained.omitted as usize + retained.head.len() + retained.tail.len(),
                        total
                    );
                }
                let omitted = size.saturating_sub(2 * edge);
                let expected = if omitted == 0 {
                    source.clone()
                } else {
                    let mut bytes = source[..edge].to_vec();
                    bytes.extend_from_slice(
                        format!("\n[... {omitted} bytes of output omitted ...]\n").as_bytes(),
                    );
                    bytes.extend_from_slice(&source[size - edge..]);
                    bytes
                };
                let actual = retained.into_bytes();
                assert_eq!(actual, expected);
                if omitted > 0 {
                    let displayed = format!(
                        "{}\n[... {omitted} bytes of output omitted ...]\n{}",
                        String::from_utf8_lossy(&source[..edge]),
                        String::from_utf8_lossy(&source[size - edge..])
                    );
                    assert_eq!(String::from_utf8_lossy(&actual), displayed);
                }
            }
        }
    }
}
