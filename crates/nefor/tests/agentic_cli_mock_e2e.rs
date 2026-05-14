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
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// Hard wall-clock cap per scenario. Generous — the spawn pipeline is
/// 7 plugin processes + a Lua VM. Real cost is ~300-700ms on a warm
/// build; 10s leaves headroom for slow CI without masking flakiness.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(10);

const POLL_INTERVAL: Duration = Duration::from_millis(50);

// --------------------------------------------------------------------
// path resolution
// --------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("repo root is two levels above crates/nefor")
}

fn target_debug(bin: &str) -> PathBuf {
    repo_root().join("target").join("debug").join(bin)
}

/// Build the engine + every plugin the cli-config spawns. No-op on a
/// warm cache. The `Once` guard ensures concurrent test runs (cargo
/// runs #[test]s in parallel by default) don't all queue on the cargo
/// artifact lock — only one build call goes out per process.
fn ensure_built() {
    static BUILT: Once = Once::new();
    BUILT.call_once(|| {
        let pkgs = [
            "nefor",
            "mock-plugin",
            "reasoner-graph",
            "tool-gate-plugin",
            "nefor-combinators-plugin",
            "generic-provider",
            "generic-tool",
            "basic-tools-plugin",
        ];
        let mut cmd = Command::new(env!("CARGO"));
        cmd.arg("build").current_dir(repo_root());
        for p in pkgs {
            cmd.arg("-p").arg(p);
        }
        let status = cmd.status().expect("spawn cargo build");
        assert!(status.success(), "cargo build failed for required packages");
    });
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
        .env("NEFOR_PLUGIN_DIR", repo_root().join("plugins"))
        .env("XDG_DATA_HOME", xdg);
    cmd
}

/// Spawn the engine, wait up to `SCENARIO_TIMEOUT`, return captured
/// stdout/stderr + exit status. Drains both pipes on background threads
/// to avoid the classic pipe-buffer deadlock — stream-json mode emits
/// hundreds of envelope lines and would otherwise block the engine on
/// write before we had a chance to call `wait_with_output`. Kills the
/// process on timeout (test fails the assertion afterwards).
fn run_scenario(extra_argv: &[&str], stdin_payload: StdinPayload) -> ProcessOutput {
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

    let timed_out = match wait_with_deadline(&mut child, SCENARIO_TIMEOUT) {
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
        "scenario exceeded {SCENARIO_TIMEOUT:?}; stdout (first 4KB): {}\nstderr (first 4KB): {}",
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
/// spawn_graph path: orchestrator-turn matches "octopus" + "lighthouse"
/// + "parallel"/"combine", returning the canned 4-node sub-graph.
const SPAWN_GRAPH_PROMPT: &str =
    "summarise octopuses and lighthouses in parallel and combine into one paragraph";

/// Two short non-spawn-graph prompts. The mock has no canned match for
/// either, so it returns the deterministic
/// "[mock provider: no canned match for: <prompt>]" fallback per turn.
/// That's enough for REPL multi-turn — we just need each turn to
/// produce a distinct, recognisable line.
const SIMPLE_PROMPT_1: &str = "hello";
const SIMPLE_PROMPT_2: &str = "world";

// --------------------------------------------------------------------
// scenario 1 — single-shot text format
// --------------------------------------------------------------------

#[test]
fn scenario_1_single_shot_text_canonical() {
    ensure_built();
    let out = run_scenario(&[SPAWN_GRAPH_PROMPT], None);
    assert_success(&out);

    // The mock's combine-step canned text contains "octopus", "lighthouse",
    // and "sentinels". Loose substring match on those three keeps the
    // assertion robust against minor mock_provider tweaks while still
    // catching regressions in the spawn_graph round-trip.
    let lc = out.stdout.to_lowercase();
    for needle in ["octopus", "lighthouse", "sentinel"] {
        assert!(
            lc.contains(needle),
            "expected stdout to contain {needle:?}; got: {:?}",
            truncate(&out.stdout, 2048)
        );
    }

    // text mode prints a trailing newline on completion; the answer is
    // the only payload on stdout (tool one-liners go to stderr).
    assert!(
        out.stdout.ends_with('\n'),
        "expected trailing newline on text-format stdout"
    );

    // Sanity: the spawn_graph one-liner appeared on stderr.
    assert!(
        out.stderr.contains("[tool: spawn_graph"),
        "expected spawn_graph tool one-liner on stderr; got: {:?}",
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
        &["--format", OutputFormat::Json.as_arg(), SPAWN_GRAPH_PROMPT],
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
    assert!(
        !answer.is_empty(),
        "expected non-empty `answer` field; got: {v:?}"
    );
    let answer_lc = answer.to_lowercase();
    for needle in ["octopus", "lighthouse"] {
        assert!(
            answer_lc.contains(needle),
            "expected answer to contain {needle:?}; got: {answer:?}"
        );
    }

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
            SPAWN_GRAPH_PROMPT,
        ],
        None,
    );
    assert_success(&out);

    // Every non-empty stdout line must be a valid JSON envelope. Parse
    // each and bucket by `body.kind`.
    //
    // Run-close on the canonical tool contract is `tool.result { id=run_id,
    // result: { status, results } }` — the prior `graph.run_complete`
    // wire shape is gone. We accept either `graph.run_started` (paired
    // observer that always lands for our single run) or any
    // `tool.result` body that contains a `result.status` field as the
    // run-close marker; either is sufficient evidence the run reached
    // termination.
    let mut run_close_count = 0usize;
    let mut run_started_count = 0usize;
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
        if kind == "graph.run_started" {
            run_started_count += 1;
        }
        if kind == "tool.result"
            && body
                .and_then(|b| b.get("result"))
                .and_then(|r| r.get("status"))
                .is_some()
        {
            run_close_count += 1;
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
        "expected at least one graph.run_started envelope; saw \
         {run_started_count} across {total_lines} lines"
    );
    assert!(
        run_close_count >= 1,
        "expected at least one run-close tool.result (id=run_id, \
         result.status set) envelope; saw {run_close_count} across \
         {total_lines} lines"
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

    // The mock returns "[mock provider: no canned match for: <prompt>]"
    // for unknown prompts. Both should appear on stdout (text format),
    // each on its own turn.
    let stdout = &out.stdout;
    let needle1 = format!("no canned match for: {SIMPLE_PROMPT_1}");
    let needle2 = format!("no canned match for: {SIMPLE_PROMPT_2}");
    assert!(
        stdout.contains(&needle1),
        "expected first turn's mock fallback for {SIMPLE_PROMPT_1:?}; \
         stdout: {:?}",
        truncate(stdout, 2048)
    );
    assert!(
        stdout.contains(&needle2),
        "expected second turn's mock fallback for {SIMPLE_PROMPT_2:?}; \
         stdout: {:?}",
        truncate(stdout, 2048)
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
    let out = run_scenario(&["--yolo", SPAWN_GRAPH_PROMPT], None);
    assert_success(&out);

    // --yolo is a placeholder per agentic_workflow.set_yolo. Behaviour
    // should be identical to the non-yolo run — assert the canonical
    // answer still flows. We don't assert on tool-gate behaviour: the
    // gate hookup is explicitly deferred (Phase 1B).
    let lc = out.stdout.to_lowercase();
    for needle in ["octopus", "lighthouse"] {
        assert!(
            lc.contains(needle),
            "expected --yolo run to still produce canonical answer; \
             missing {needle:?}; stdout: {:?}",
            truncate(&out.stdout, 2048)
        );
    }
}

// --------------------------------------------------------------------
// Bug 1 regression — long-running streams complete without a watchdog.
// --------------------------------------------------------------------
//
// `reasoner-graph` lost its ack-timeout watchdog in commit 0941531.
// This scenario drives a turn where the mock provider deliberately
// blocks past any short-window ack budget; the assertion is only that
// the run finishes successfully and emits its answer. With the
// watchdog removed the test is trivially green; if anything ever
// reintroduces a watchdog at the protocol level, the slow turn would
// fail with a deadline error here.
//
// The slow path is gated on the literal substring
// "SLOW_STREAM_REGRESSION_" in the user prompt — see
// `starter/mock-provider/init.lua`.

#[test]
fn long_stream_completes_without_timeout() {
    ensure_built();
    let prompt = "SLOW_STREAM_REGRESSION_marker please respond";
    let out = run_scenario(&[prompt], None);
    assert_success(&out);

    // The slow-path canned text — distinguishes a real completion from
    // an early-exit / "no canned match" fallback that might otherwise
    // satisfy `assert_success` while skipping the slow handler.
    assert!(
        out.stdout.contains("slow regression payload acknowledged"),
        "slow path must complete with its canned answer; got: {:?}",
        truncate(&out.stdout, 2048)
    );
    // No deadline-shaped error envelope should leak onto stderr —
    // historical AckTimeout payloads carried this exact substring.
    assert!(
        !out.stderr.contains("AckTimeout"),
        "stderr must not surface the legacy AckTimeout error code; \
         a watchdog regression would emit it on a slow turn: {:?}",
        truncate(&out.stderr, 2048)
    );
}
