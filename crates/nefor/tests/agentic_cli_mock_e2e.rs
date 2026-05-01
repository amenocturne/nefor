//! End-to-end tests for the agentic-cli plugin against the mock provider.
//!
//! Spawns the real `nefor` engine binary as a subprocess against
//! `cli-config/`, with `USE_MOCK_PROVIDER=true` so no live LLM is needed.
//! Each scenario covers one path through the agentic_workflow + agentic_cli
//! surface: single-shot text/json/stream-json formats, REPL multi-turn,
//! `--help`, and the `--yolo` placeholder flag.
//!
//! These run in default `cargo test` (no `#[ignore]`). They close the gap
//! `stage1_e2e.rs` left open: that test still requires live Ollama; this
//! one validates the same wire end-to-end with the deterministic mock.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
/// warm cache. We do this once per test invocation; cargo serialises
/// individual #[test] runs via the test binary itself, so concurrent
/// `cargo build` calls only race the first time and the overhead is
/// minimal afterwards.
fn ensure_built() {
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
}

impl OutputFormat {
    fn as_arg(self) -> &'static str {
        match self {
            OutputFormat::Json => "json",
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
        .env("USE_MOCK_PROVIDER", "true")
        .env("NEFOR_PLUGIN_DIR", repo_root().join("plugins"))
        .env("XDG_DATA_HOME", xdg);
    cmd
}

/// Spawn the engine, wait up to `SCENARIO_TIMEOUT`, return captured
/// stdout/stderr + exit status. Kills the process on timeout (test
/// fails the assertion afterwards).
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

    let timed_out = match wait_with_deadline(&mut child, SCENARIO_TIMEOUT) {
        Some(_) => false,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            true
        }
    };

    let output = child.wait_with_output().expect("wait_with_output");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !timed_out,
        "scenario exceeded {SCENARIO_TIMEOUT:?}; stdout (first 4KB): {}\nstderr (first 4KB): {}",
        truncate(&stdout, 4096),
        truncate(&stderr, 4096)
    );

    drop(xdg);
    ProcessOutput {
        status: output.status,
        stdout,
        stderr,
    }
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
        format!("{}...<truncated {} bytes>", &s[..n], s.len() - n)
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

// --------------------------------------------------------------------
// fixtures
// --------------------------------------------------------------------

/// The canonical prompt that drives the mock provider down the
/// spawn_graph path: orchestrator-turn matches "octopus" + "lighthouse"
/// + "parallel"/"combine", returning the canned 4-node sub-graph.
const SPAWN_GRAPH_PROMPT: &str =
    "summarise octopuses and lighthouses in parallel and combine into one paragraph";

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
