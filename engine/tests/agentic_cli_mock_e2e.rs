//! End-to-end tests for the agentic-cli plugin against the mock provider.
//!
//! Spawns the real `nefor` engine binary as a subprocess against
//! `cli-config/`, with `NEFOR_CONFIG=test` so no live LLM is needed.
//! Each scenario covers one path through the agentic_workflow + agentic_cli
//! surface: single-shot text/json/stream-json formats, REPL multi-turn,
//! `--help`, and the `--yolo` placeholder flag.
//!
//! These run in default `cargo test` (no `#[ignore]`). They close the gap
//! `stage1_e2e.rs` left open: that test still requires live Ollama; this
//! one validates the same wire end-to-end with the deterministic mock.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// Wall-clock cap per scenario. Each scenario spawns the engine + 7
/// plugin subprocesses; when all 8 run in parallel the combined I/O
/// load on a busy machine needs headroom beyond the ~1s per-scenario
/// cost in isolation. Override via `NEFOR_E2E_TIMEOUT_SECS` for CI
/// runners or loaded dev machines.
fn scenario_timeout() -> Duration {
    let secs: u64 = std::env::var("NEFOR_E2E_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    Duration::from_secs(secs)
}

const POLL_INTERVAL: Duration = Duration::from_millis(50);

// --------------------------------------------------------------------
// path resolution
// --------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .expect("repo root is one level above engine")
}

fn target_debug(bin: &str) -> PathBuf {
    repo_root().join("target").join("debug").join(bin)
}

/// Required binaries for the cli-config spawn pipeline.
const REQUIRED_BINS: &[&str] = &[
    "nefor",
    "mock-plugin",
    "mag-plugin",
    "tool-gate",
    "generic-provider",
    "generic-tool",
    "basic-tools",
];

/// Assert that every binary the cli-config spawns exists in
/// `target/debug/`. Under `cargo test --workspace` they're built
/// automatically; under `cargo test -p nefor` the user must have
/// run `cargo build --workspace` first.
///
/// We deliberately do NOT spawn a nested `cargo build` here —
/// cargo holds a target-directory lock while running test binaries,
/// so a child `cargo build` from inside a test deadlocks.
/// The missing-list is computed once and asserted per scenario (not
/// inside a `Once::call_once`): a panic inside call_once poisons the
/// Once, so every OTHER scenario would fail with an opaque
/// "poisoned Once" instead of the actual missing-binaries message.
fn ensure_built() {
    static MISSING: OnceLock<Vec<&'static str>> = OnceLock::new();
    let missing = MISSING.get_or_init(|| {
        REQUIRED_BINS
            .iter()
            .filter(|bin| !target_debug(bin).exists())
            .copied()
            .collect()
    });
    assert!(
        missing.is_empty(),
        "e2e tests require pre-built binaries. Missing: {:?}. \
         Run `cargo build --workspace` first, or use `cargo test --workspace`.",
        missing,
    );
}

// --------------------------------------------------------------------
// child helpers
// --------------------------------------------------------------------

/// `--format` values that scenarios assert on. `text` is the default and
/// has its own scenario; non-default formats are passed via `--format`.
/// Enum rather than stringly-typed to keep the call sites readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Json,
    StreamJson,
}

impl OutputFormat {
    fn as_arg(self) -> &'static str {
        match self {
            OutputFormat::Json => "json",
            OutputFormat::StreamJson => "stream-json",
        }
    }
}

/// Output of a finished engine subprocess.
struct ProcessOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// What a scenario does with stdin. `None` closes it immediately
/// (single-shot mode); `Some(bytes)` pipes the bytes and then closes,
/// triggering REPL EOF.
type StdinPayload<'a> = Option<&'a [u8]>;

/// Build a `Command` configured for the agentic-cli plugin against
/// `cli-config/`. Caller appends extra argv (flags, prompt) and runs.
fn base_command(xdg: &Path) -> Command {
    let mut cmd = Command::new(target_debug("nefor"));
    cmd.arg("--config")
        .arg(repo_root().join("cli-config"))
        .arg("plugin")
        .arg("agentic-cli")
        .env("NEFOR_CONFIG", "test")
        .env("NEFOR_INSTALLATION_ID", "test:agentic-cli-mock-e2e")
        .env("NEFOR_PLUGIN_DIR", repo_root().join("plugins"))
        // Disable the mock provider's 80 tok/s pacing under tests so
        // the 10s scenario_timeout() stays comfortable. Interactive
        // launches (the user driving `nefor` directly) get pacing
        // because the env var isn't set.
        .env("NEFOR_TEST_FAST_MOCK", "1")
        .env("XDG_DATA_HOME", xdg);
    cmd
}

/// Serialise scenario execution. Each scenario spawns the engine + 7
/// plugin subprocesses (56+ processes total across 8 tests). Running
/// all 8 in parallel causes the engine subprocesses to hang on startup
/// with empty stdout/stderr — likely OS-level resource contention from
/// 56 simultaneous process spawns. Serialised execution is fast (~8s
/// total on warm cache) and reliable.
static SCENARIO_LOCK: Mutex<()> = Mutex::new(());

/// Spawn the engine, wait up to `scenario_timeout()`, return captured
/// stdout/stderr + exit status. Drains both pipes on background threads
/// to avoid the classic pipe-buffer deadlock — stream-json mode emits
/// hundreds of envelope lines and would otherwise block the engine on
/// write before we had a chance to call `wait_with_output`. Kills the
/// process on timeout (test fails the assertion afterwards).
fn run_scenario(extra_argv: &[&str], stdin_payload: StdinPayload) -> ProcessOutput {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let xdg = TempDir::new().expect("xdg tempdir");
    let mut cmd = base_command(xdg.path());
    for a in extra_argv {
        cmd.arg(a);
    }
    cmd.stdin(if stdin_payload.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn nefor engine");

    if let (Some(payload), Some(mut stdin)) = (stdin_payload, child.stdin.take()) {
        stdin.write_all(payload).expect("write stdin payload");
        // Closing on drop sends EOF; for REPL that triggers clean exit.
        drop(stdin);
    }

    let stdout_buf = drain_pipe(child.stdout.take().expect("stdout piped"));
    let stderr_buf = drain_pipe(child.stderr.take().expect("stderr piped"));

    let timeout = scenario_timeout();
    let timed_out = match wait_with_deadline(&mut child, timeout) {
        Some(_) => false,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            true
        }
    };
    let status = child.wait().expect("wait child");

    let stdout = take_buf(&stdout_buf);
    let stderr = take_buf(&stderr_buf);

    assert!(
        !timed_out,
        "scenario exceeded {timeout:?}; stdout (first 4KB): {}\nstderr (first 4KB): {}",
        truncate(&stdout, 4096),
        truncate(&stderr, 4096)
    );

    drop(xdg);
    ProcessOutput {
        status,
        stdout,
        stderr,
    }
}

/// Spawn a thread that reads `pipe` to EOF into a shared `Vec<u8>`. The
/// returned `Arc<Mutex<Vec<u8>>>` collects bytes as they arrive; reading
/// it after the child exits gives the full output. Reading it earlier
/// gives a partial snapshot (used in timeout-diagnostic paths).
fn drain_pipe<R: Read + Send + 'static>(mut pipe: R) -> Arc<Mutex<Vec<u8>>> {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let buf_clone = Arc::clone(&buf);
    thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    if let Ok(mut g) = buf_clone.lock() {
                        g.extend_from_slice(&chunk[..n]);
                    }
                }
                Err(_) => return,
            }
        }
    });
    buf
}

fn take_buf(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let bytes = buf.lock().map(|g| g.clone()).unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn wait_with_deadline(child: &mut Child, deadline: Duration) -> Option<std::process::ExitStatus> {
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(_) => return None,
        }
    }
    None
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_owned()
    } else {
        // Floor to the nearest char boundary at-or-before `n`; raw
        // `&s[..n]` panics when `n` lands inside a multi-byte UTF-8
        // char (scenario_4 flake under heavy parallel load — help
        // banners with CJK / cyrillic byte runs cross the cap).
        let cut = s.floor_char_boundary(n);
        format!("{}...<truncated {} bytes>", &s[..cut], s.len() - cut)
    }
}

fn assert_success(out: &ProcessOutput) {
    assert!(
        out.status.success(),
        "engine exited with {:?}; stderr (first 4KB): {}",
        out.status,
        truncate(&out.stderr, 4096)
    );
}

#[test]
fn truncate_does_not_panic_on_multibyte_boundary() {
    // Pre-fix `truncate` did `&s[..n]` which panics whenever `n` lands
    // inside a multi-byte UTF-8 char — the scenario_4 flake under heavy
    // parallel load when a help-banner CJK / cyrillic byte run crossed
    // the 2048-byte cap. Each n in 0..=s.len() must produce valid UTF-8.
    let s = "привет world hello мир — еще немного текста";
    for n in 0..=s.len() {
        let out = truncate(s, n);
        // String already enforces UTF-8 validity, so reaching this line
        // is the no-panic assertion. Belt-and-braces: the prefix must
        // be a prefix of `s` up to some char boundary at-or-before n.
        let head_end = out.find("...<truncated ").unwrap_or(out.len());
        let head = &out[..head_end];
        assert!(s.starts_with(head), "head must be a prefix of input");
        assert!(
            head.len() <= n,
            "head bytes ({}) must not exceed cap ({}) for n={}",
            head.len(),
            n,
            n
        );
    }
}

// --------------------------------------------------------------------
// fixtures
// --------------------------------------------------------------------

/// The canonical prompt that drives the mock provider down the
/// kernel-dispatch path: the orchestrator turn matches "octopus" +
/// "lighthouse" and writes + executes the canned MAG program via the
/// lead's `mag` tool (write → execute → deferred relay turn).
const MAG_DISPATCH_PROMPT: &str =
    "summarise octopuses and lighthouses in parallel and combine into one paragraph";

/// Two short non-dispatching prompts. The mock has no canned trigger
/// for either, so each turn falls through to the help-banner path —
/// fine for REPL multi-turn since we only need two recognisable turns.
const SIMPLE_PROMPT_1: &str = "Summarise octopuses in one sentence.";
const SIMPLE_PROMPT_2: &str = "Summarise lighthouses in one sentence.";
const EXPECTED_MAG_FINAL: &str = "Octopuses, with their remarkable intelligence and adaptive camouflage, share an unlikely kinship with the steadfast lighthouse — both serve as vigilant sentinels of their respective worlds, the cephalopod beneath the waves and the beacon above them, each watchful in its solitary post.";
const EXPECTED_OCTOPUS_FINAL: &str = "Octopuses are highly intelligent invertebrate cephalopods known for problem-solving, dynamic camouflage, and eight prehensile arms lined with chemosensitive suckers.";
const EXPECTED_LIGHTHOUSE_FINAL: &str = "Lighthouses are tall coastal towers crowned with bright rotating beams that guide ships safely past hazards and into harbours, dating back to the Pharos of Alexandria.";
const EXPECTED_SLOW_FINAL: &str = "slow regression payload acknowledged";

// --------------------------------------------------------------------
// scenario 1 — single-shot text format
// --------------------------------------------------------------------

#[test]
fn scenario_1_single_shot_text_canonical() {
    ensure_built();
    let out = run_scenario(&[MAG_DISPATCH_PROMPT], None);
    assert_success(&out);

    assert_eq!(out.stdout, format!("{EXPECTED_MAG_FINAL}\n"));

    // Sanity: the mag tool one-liners (write + execute) appeared on
    // stderr — the kernel-dispatch pipeline actually ran.
    assert!(
        out.stderr.contains("[tool: mag"),
        "expected mag tool one-liner on stderr; got: {:?}",
        truncate(&out.stderr, 2048)
    );
}

// --------------------------------------------------------------------
// scenario 2 — single-shot json format
// --------------------------------------------------------------------

#[test]
fn scenario_2_single_shot_json() {
    ensure_built();
    let out = run_scenario(
        &["--format", OutputFormat::Json.as_arg(), MAG_DISPATCH_PROMPT],
        None,
    );
    assert_success(&out);

    // json mode prints exactly one JSON line on stdout.
    let trimmed = out.stdout.trim_end_matches('\n');
    assert!(
        !trimmed.is_empty(),
        "expected one JSON line on stdout; got empty"
    );
    assert!(
        !trimmed.contains('\n'),
        "expected exactly one JSON line on stdout; got multiple: {:?}",
        truncate(&out.stdout, 2048)
    );

    let v: Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("stdout is not valid JSON: {e}; line: {trimmed:?}"));

    let answer = v
        .get("answer")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing `answer` field: {v:?}"));
    assert_eq!(answer, EXPECTED_MAG_FINAL);

    let status = v
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing `status` field: {v:?}"));
    assert_eq!(
        status, "success",
        "expected status=success; full payload: {v:?}"
    );
}

// --------------------------------------------------------------------
// scenario 3 — single-shot stream-json format
// --------------------------------------------------------------------

#[test]
fn scenario_3_single_shot_stream_json() {
    ensure_built();
    let out = run_scenario(
        &[
            "--format",
            OutputFormat::StreamJson.as_arg(),
            MAG_DISPATCH_PROMPT,
        ],
        None,
    );
    assert_success(&out);

    // Every non-empty stdout line must be a valid JSON envelope. Parse
    // each and bucket by `body.kind`.
    //
    // Kernel-run lifecycle rides `mag.*`: `mag.run_started` fires for
    // every run (the lead's turn-programs AND the dispatched canned
    // program), and the terminal `mag.run_result` closes a run. The
    // dispatch ack itself is a canonical `tool.result` carrying
    // `output.status` ("written" for the workspace write, "executing"
    // for the submit) — also asserted, as the seam between the lead's
    // tool surface and the kernel.
    let mut run_started_count = 0usize;
    let mut run_result_count = 0usize;
    let mut dispatch_ack_count = 0usize;
    let mut total_lines = 0usize;
    for (idx, line) in out.stdout.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        total_lines += 1;
        let v: Value = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!(
                "stream-json line {idx} is not valid JSON: {e}; line: {:?}",
                truncate(line, 512)
            )
        });
        let body = v.get("body");
        let kind = body
            .and_then(|b| b.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if kind == "mag.run_started" {
            run_started_count += 1;
        }
        if kind == "mag.run_result" {
            run_result_count += 1;
        }
        if kind == "tool.result"
            && body
                .and_then(|b| b.get("output"))
                .and_then(|o| o.get("status"))
                .is_some()
        {
            dispatch_ack_count += 1;
        }
    }

    assert!(
        total_lines > 0,
        "expected at least one envelope on stdout in stream-json mode"
    );
    // Bus fan-out delivers one log entry per subscriber, so the wildcard
    // handler in install_stream_json_format sees the same kind multiple
    // times. Don't lock to an exact count — assert ≥1 to keep the test
    // robust against bus-fan-out tuning.
    assert!(
        run_started_count >= 1,
        "expected at least one mag.run_started envelope; saw \
         {run_started_count} across {total_lines} lines"
    );
    assert!(
        run_result_count >= 1,
        "expected at least one mag.run_result run-close envelope; saw \
         {run_result_count} across {total_lines} lines"
    );
    assert!(
        dispatch_ack_count >= 1,
        "expected at least one tool.result with output.status (the mag \
         write/execute acks); saw {dispatch_ack_count} across \
         {total_lines} lines"
    );

    // The surface consumes the universal conversation projection, not the
    // old TUI-specific `chat.graph_result.append` presentation event. Assert
    // both a canonical terminal turn and the deferred relay content.
    let mut terminal_turn_count = 0usize;
    let mut relay_content_count = 0usize;
    for line in out.stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let body = match v.get("body") {
            Some(b) => b,
            None => continue,
        };
        let kind = body.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind == "conversation.projection.delta"
            && body
                .get("change")
                .and_then(|change| change.get("kind"))
                .and_then(Value::as_str)
                == Some("turn_completed")
        {
            terminal_turn_count += 1;
        }
        if kind.starts_with("conversation.") && line.contains("sentinels") {
            relay_content_count += 1;
        }
    }
    assert!(
        terminal_turn_count >= 1,
        "expected a canonical terminal conversation turn on the bus; saw \
         {terminal_turn_count} across {total_lines} lines"
    );
    assert!(
        relay_content_count >= 1,
        "expected the combine step's text (\"sentinels\") to reach the \
         wire in a conversation.* envelope from the deferred relay turn; saw \
         {relay_content_count} across {total_lines} lines"
    );
}

// --------------------------------------------------------------------
// scenario 4 — REPL multi-turn (2 prompts + EOF)
// --------------------------------------------------------------------

#[test]
fn scenario_4_repl_multi_turn() {
    ensure_built();
    let payload = format!("{SIMPLE_PROMPT_1}\n{SIMPLE_PROMPT_2}\n");
    let out = run_scenario(&[], Some(payload.as_bytes()));
    assert_success(&out);

    assert_eq!(
        out.stdout,
        format!("{EXPECTED_OCTOPUS_FINAL}\n{EXPECTED_LIGHTHOUSE_FINAL}\n")
    );

    // Sanity: the REPL emitted at least two prompts on stderr.
    let prompt_count = out.stderr.matches("> ").count();
    assert!(
        prompt_count >= 2,
        "expected at least two REPL prompts on stderr; saw {prompt_count}; \
         stderr: {:?}",
        truncate(&out.stderr, 1024)
    );
}

// --------------------------------------------------------------------
// scenario 5 — `-- --help` documented usage workaround
// --------------------------------------------------------------------

#[test]
fn scenario_5_help_via_double_dash() {
    ensure_built();
    let out = run_scenario(&["--", "--help"], None);
    assert_success(&out);

    // The USAGE banner from agentic_cli.lua starts with "Usage:" and
    // documents the format flag. Both substrings keep the assertion
    // anchored without locking to the full text.
    assert!(
        out.stdout.starts_with("Usage:"),
        "expected stdout to start with `Usage:`; got: {:?}",
        truncate(&out.stdout, 512)
    );
    assert!(
        out.stdout.contains("--format"),
        "expected USAGE banner to mention --format; got: {:?}",
        truncate(&out.stdout, 1024)
    );
}

// --------------------------------------------------------------------
// scenario 6 — --yolo accepted (placeholder, no behavioural assertion)
// --------------------------------------------------------------------

#[test]
fn scenario_6_yolo_flag_accepted() {
    ensure_built();
    let out = run_scenario(&["--yolo", MAG_DISPATCH_PROMPT], None);
    assert_success(&out);

    assert_eq!(out.stdout, format!("{EXPECTED_MAG_FINAL}\n"));
}

// --------------------------------------------------------------------
// Bug 1 regression — long-running streams complete without a watchdog.
// --------------------------------------------------------------------
//
// This scenario drives a turn where the mock provider deliberately
// blocks past any short-window ack budget; the assertion is only that
// the run finishes successfully and emits its answer. If anything ever
// reintroduces an ack-timeout watchdog at the protocol level, the slow
// turn would fail with a deadline error here.
//
// The slow path is gated on the literal substring
// "SLOW_STREAM_REGRESSION_" in the user prompt — see
// `examples/nefor-agent/mock-provider/init.lua`.

#[test]
fn long_stream_completes_without_timeout() {
    ensure_built();
    let prompt = "SLOW_STREAM_REGRESSION_marker please respond";
    let out = run_scenario(&[prompt], None);
    assert_success(&out);

    // The slow-path canned text — distinguishes a real completion from
    // an early-exit / "no canned match" fallback that might otherwise
    // satisfy `assert_success` while skipping the slow handler.
    assert_eq!(out.stdout, format!("{EXPECTED_SLOW_FINAL}\n"));
    // No deadline-shaped error envelope should leak onto stderr —
    // historical AckTimeout payloads carried this exact substring.
    assert!(
        !out.stderr.contains("AckTimeout"),
        "stderr must not surface the legacy AckTimeout error code; \
         a watchdog regression would emit it on a slow turn: {:?}",
        truncate(&out.stderr, 2048)
    );
}
