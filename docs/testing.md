# Testing nefor

How to verify a working build, run the test suite, and exercise each surface (CLI, TUI, mock e2e). Written against the post-CLI-plugin-epic main (May 2026 onward); the workspace ships ~1600 tests and the mock-driven CLI e2e suite as the default health check.

## TL;DR — 30-second smoke

```bash
cargo build --workspace                              # builds clean
cargo test --workspace 2>&1 | grep "test result"     # all pass
cargo test --test agentic_cli_mock_e2e               # 6 scenarios, ~2.5s wall
```

If those three pass, the substrate is healthy.

## Test suite layout

| Suite                           | Where                                           | What it covers                                                                                             | Cost                           |
| ------------------------------- | ----------------------------------------------- | ---------------------------------------------------------------------------------------------------------- | ------------------------------ |
| Workspace unit tests            | `cargo test --workspace`                        | Per-crate unit tests across engine + all plugins                                                           | Few seconds, mostly in-process |
| `agentic_cli_mock_e2e`          | `engine/tests/agentic_cli_mock_e2e.rs`          | Full chain (engine binary as subprocess + Lua + mock provider). 6 scenarios. **Default e2e health check.** | ~2.5s wall                     |
| `starter_ncp_test`              | `engine/tests/starter_ncp_test.rs`              | NCP behavior in Lua                                                                                   | <1s                            |
| `starter_sessions_test`         | `engine/tests/starter_sessions_test.rs`         | Session persistence + resume                                                                               | <1s                            |
| `starter_agentic_workflow_test` | `engine/tests/starter_agentic_workflow_test.rs` | Agentic orchestration Lua tests                                                                            | <1s                            |
| Plugin unit tests               | `cargo test -p <plugin>`                        | Each plugin's own tests (nefor-tui has chat + layout + reconciler tests, openai-provider 137, etc.)        | Sub-second per plugin          |

The `agentic_cli_mock_e2e` suite is the one to add scenarios to when you ship new agentic-cli flow behavior. It's deterministic, non-ignored, and fast enough to live in CI.

## Running the workspace

```bash
# Full workspace
cargo test --workspace

# A single test file
cargo test --test agentic_cli_mock_e2e

# A single test function
cargo test --test agentic_cli_mock_e2e -- single_shot_text_canonical_happy_path

# A specific plugin
cargo test -p nefor-tui
cargo test -p openai-provider

# With output (for debugging a failing test)
cargo test --test agentic_cli_mock_e2e -- --nocapture

# Including #[ignore]-gated tests (opt-in; may need external services)
cargo test -- --ignored
```

Conventional pre-commit checks: `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings`. Both expected to pass on main.

## Manual verification — agentic-cli surface

The CLI is a pure-Lua plugin declared in `cli-config/init.lua`. Run via `nefor plugin agentic-cli ...`. Use the mock provider for deterministic testing without needing a live LLM.

### Setup (run once per shell)

```bash
cd /path/to/nefor
cargo build --workspace
export NEFOR_PLUGIN_DIR=$PWD/target/debug
export NEFOR_CONFIG=test
```

`NEFOR_CONFIG=test` makes `cli-config/config.lua` set `spawn_mock = true`, which spawns `mock-plugin` running `starter/mock-provider/init.lua` (`--script`) instead of a live provider. Drop the env var to use the configured real provider. (`USE_MOCK_PROVIDER=true` is deprecated — it now maps to the `test` config with a warning.)

### Single-shot mode

```bash
./target/debug/nefor --config cli-config plugin agentic-cli \
  "summarise octopuses and lighthouses in parallel and combine into one paragraph"
```

**Expect:** stdout contains the canonical mock answer (mentions "octopus" + "lighthouse" + "sentinel"-ish framing). Stderr lines `[tool: mag(...)]` for the write + execute calls. Exit 0.

This is the canonical e2e flow: `chat.input.submit` → lead turn-program on the
MAG kernel → `mag` tool (write, then execute) → kernel run (chained summaries →
combine → structural result boundary) → deferred relay turn → final answer.

### Output format modes

Default is `text`. Two other modes for scripting:

```bash
# Single-line JSON envelope on success
./target/debug/nefor --config cli-config plugin agentic-cli \
  --format json "summarise octopuses..."
# stdout: {"answer": "...", "status": "success", "duration_ms": ..., "tool_calls": {}}

# Raw NCP envelopes streamed live, one per line
./target/debug/nefor --config cli-config plugin agentic-cli \
  --format stream-json "summarise octopuses..."
# stdout: many lines of {"type":"event","from":"...","ts":"...","body":{"kind":"...","..."}}
```

The `stream-json` mode is the same wire format the engine logs to disk — a `tail -f` of NCP events. Use `jq` to filter:

```bash
./target/debug/nefor --config cli-config plugin agentic-cli --format stream-json "..." \
  | jq -c 'select(.body.kind == "mag.run_complete")'
```

**Known wart**: broadcast events appear N times (one per recipient peer) due to the engine's broadcast-as-N-targeted-sends pattern. Filter on a primary key like `run_id` if you need uniqueness.

### REPL mode

```bash
./target/debug/nefor --config cli-config plugin agentic-cli
> hello
[mock provider: no canned match for: hello]
> and goodbye
[mock provider: no canned match for: and goodbye]
> ^D
```

Pipe stdin for scripted multi-turn:

```bash
printf "hello\nand goodbye\n" | ./target/debug/nefor --config cli-config plugin agentic-cli
```

EOF (Ctrl+D or end-of-pipe) exits cleanly with code 0.

### Other flags

Starter-owned interactive flags are parsed by the composition, not Rust clap:

- `nefor run --session <id>` opens the TUI on a specific saved session.
- `nefor run --prompt <text>` submits a prompt after startup.
- `nefor run --mode safe|auto|yolo` selects the initial tool permission mode.
- `nefor run --yolo` is shorthand for `--mode yolo`.

Arguments may appear in any order. Repeated mode controls are applied left-to-right, so the last occurrence wins. A resumed session remains safe after process restart unless this invocation explicitly supplies a mode.

| Flag                            | What it does                                                                                                                          |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `-m <model>`, `--model <model>` | Sets the model for new chats; calls `agentic_workflow.set_model`                                                                      |
| `--yolo`                        | Calls `agentic_workflow.set_yolo(true)`; requests `tool-gate.set_mode { mode = "yolo" }`                                              |
| `-f <path>`, `--file <path>`    | Reads file, prepends to prompt as `### File: <path>\n\`\`\`\n<contents>\n\`\`\`\n\n`                                                  |
| `--debug`                       | No-op for v1                                                                                                                          |
| `--help`, `-h`                  | Reachable as `nefor --config cli-config plugin agentic-cli -- --help` (the `--` is needed because outer clap eats `--help` otherwise) |

### Deferred until further capability ships

- `-c, --continue` — most-recent session resume; needs multi-session resume
- `-s, --session <id>` inside `agentic-cli` — specific session resume for the CLI plugin
- `--fork` — fork on resume; same dependency
- `--allowed-tools / --disallowed-tools` — needs tool-gate policy API

## Manual verification — TUI regression

The agentic-cli and TUI share the same `agentic-loop` module (`starter/agentic-loop/init.lua`; the CLI binds it to a local `agentic_workflow` alias). After any change to that module, smoke the TUI to confirm chat still works.

```bash
unset NEFOR_CONFIG
./target/debug/nefor --config starter
```

(Assuming `starter/init.lua` is configured for your real provider. If not, edit it to point at Ollama / OpenAI / whatever.)

Things to verify:

- Submit a prompt → tokens stream into the transcript
- Reasoning collapses to `▸ reasoning (Ns)` if your model emits `delta.reasoning`
- Tool calls render as collapsible rows (Ctrl+O toggles expansion of all tool/reasoning rows globally)
- `/model <model-id>` switches model at runtime
- `/safe`, `/auto`, and `/yolo` toggle policy; see `docs/approval-model.md`
- `/new` clears transcript and starts a fresh chat
- Single Esc cancels in-flight turn (chat unblocks for next submit)
- Double Esc within 600ms → fan-cancel (lead run + queued inputs) → system message reports counts
- A dispatched kernel run (lead `mag` tool, action=execute) appears in the sidebar run panel; actor groups go running → done

## Verifying known concerns

These are documented quirks of the current codebase. Confirming they exist is part of due diligence — they're surfaced for fixing in future sessions, not bugs to be hit by surprise.

### Stream-json fan-out duplication

```bash
./target/debug/nefor --config cli-config plugin agentic-cli --format stream-json \
  "summarise octopuses..." \
  | jq -c 'select(.body.kind == "mag.run_complete")' | wc -l
```

Expect `~15` (not 1). Each broadcast is logged N times for N recipient peers; `nefor.bus.on_event` fires once per log entry.

Fix candidates (future work): dedupe inside `nefor.bus.on_event` by `(origin, ts)`; or dedupe in the CLI's `install_stream_json_format` by `run_id × kind`.

### `--help` consumption by outer clap

```bash
./target/debug/nefor --config cli-config plugin agentic-cli --help
# Shows engine help, NOT agentic-cli help
```

Documented workaround: `--` separator.

```bash
./target/debug/nefor --config cli-config plugin agentic-cli -- --help
# Now shows agentic-cli help
```

Fix (future work): `disable_help_flag = true` on the `Plugin` subcommand in `engine/src/cli.rs`.

### Permission modes

`agentic_workflow.set_mode` emits `tool-gate.set_mode`, and the starter mirrors
the selected mode through `tool-validator`, `lead-workflow`, and chat status.
The current mode contract is documented in `docs/approval-model.md`.

## Adding a test scenario to the mock e2e suite

```rust
// In engine/tests/agentic_cli_mock_e2e.rs

#[test]
fn my_new_scenario() {
    let env = TestEnv::setup();
    let output = env.run_cli(&["--format", "text", "your prompt"]).expect("ran");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("expected substring"));
}
```

`TestEnv` helper handles:

- TempDir for `XDG_DATA_HOME` (each test gets its own; no pollution)
- Spawning the engine binary with `NEFOR_CONFIG=test` (mock provider), `NEFOR_TEST_FAST_MOCK=1` (drops the mock's token pacing), and `NEFOR_PLUGIN_DIR=$repo/plugins`
- `--config cli-config` argv prefix
- Wall-clock timeout (10s default per scenario)
- Thread-per-pipe drain (avoids the ~64KB stdout pipe-buffer deadlock that otherwise stalls `wait_with_output` in stream-json mode)
- One-time `cargo build` via `Once` guard (cargo's artifact lock serializes parallel builds)

If the prompt doesn't have a canned response in `starter/mock-provider/init.lua`, the mock falls through to `[mock provider: no canned match for: <prompt>]`. Use this for scenarios that don't care about response content (e.g. testing flag plumbing or REPL multi-turn). Add a new scripted prompt to `mock-provider/init.lua` only if a deterministic content match is required.

## Adding scenarios to mock-provider/init.lua

The mock matches on Lua patterns. `CANNED_TEXT` is a list of `{ pattern, text }` entries — the first pattern that matches the (lowercased) prompt wins and its `text` is streamed back:

```lua
-- starter/mock-provider/init.lua

local CANNED_TEXT = {
  -- Existing canonical: the octopus+lighthouse "sentinel" combiner answer
  { pattern = "octopus.*lighthouse", text = "Octopuses ... sentinels ..." },
  { pattern = "[Ss]ummarise octopuses", text = "..." },
  -- Add yours here
  { pattern = "your test prompt", text = "..." },
}
```

The multi-agent kernel flow uses a separate pair — `CANNED_MAG_FILE` and `CANNED_MAG_PROGRAM` — the `.mag` source the mock emits for the `mag` tool's write/execute calls. Document at the top of `mock-provider/init.lua` what prompts trigger what responses. Don't change existing entries (other tests depend on them); only add new ones.

## Troubleshooting

**`cargo build` fails.** Likely a missing rebase from main. `git pull --rebase origin main` and retry. Check `cargo --version` (current toolchain expected).

**`--config cli-config` errors with "config not found".** You ran from outside the repo root; the path is relative. Either `cd` to the nefor repo or use an absolute path: `--config /path/to/nefor/cli-config`.

**Mock e2e test times out.** Default 10s per scenario. On a slow machine, bump in `TestEnv::run_cli`. If a real bug, run with `--nocapture` to see what the CLI is producing.

**Engine binary not found.** Tests use `env!("CARGO_BIN_EXE_nefor")` which requires `cargo build` first. The `Once`-guarded build runs lazily on first test, but if you're invoking via something other than `cargo test`, build manually first.

**TUI doesn't render.** Wrong `TERM`, missing `tput`, or running over a terminal that doesn't support raw mode. Confirm `tput cols` and `tput lines` work.

**TUI shows blank transcript on submit.** Likely a regression in the `agentic-loop` module (`starter/agentic-loop/init.lua`) — the chat plugin still emits chat-contract events but no one's translating provider events anymore. Re-run `cargo test -p nefor-tui` and `cargo test --test agentic_cli_mock_e2e`; if those pass but TUI is broken with a real provider, suspect provider-side translation in `agentic-loop`.

**`NEFOR_CONFIG=test cmd` doesn't propagate env.** Some shells lose env vars across `bash -c '...'` subshells. Export the var first (`export NEFOR_CONFIG=test; cmd`) or set explicitly per-command (`NEFOR_CONFIG=test exec cmd`). For Rust tests use `Command::env()`, not relying on the parent process.

## What good looks like

- **Workspace test count**: ~1600 passing / 0 failed on a clean main
- **Mock e2e suite**: 6 scenarios, all green, ~2.5s wall
- **Clippy**: clean across `cargo clippy --workspace --all-targets -- -D warnings`
- **Fmt**: clean
- **TUI smoke**: renders, streams, slash commands, double-Esc fan-cancel all work against a real provider
- **CLI smoke**: single-shot canonical prompt produces canonical answer; REPL EOFs cleanly; all three formats work

If those are all true, you have a healthy nefor.
