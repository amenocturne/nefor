# Changelog

## v0.5.0

- Made the conversation manager the canonical owner of durable conversation facts, transcript projection, model context, and provider-scoped compaction checkpoints. `/compact` now works during live sessions, preserves the full transcript as fallback, carries the active model into native ChatGPT compaction, and reports structured provider failures intelligibly.
- Added a separately configurable shared session root, acknowledgement-first session replacement, and cooperative resume with progress. Semantic replay now reconstructs conversation state without re-running tools or workflows, rejects superseded replay traffic, preserves first-message listings, and no longer resurrects historical MAG notifications or approvals.
- Expanded MAG with typed products and sums, structured agent results, explicit Git worktree create/open nodes, detached execution, awaiting, run inspection, and scoped termination. New `process.exec` and POSIX `shell.script` primitives provide explicit timeouts plus structured stdout, stderr, exit, and signal results; `mag-eval` now resolves these libraries and preserves process results correctly.
- Reworked the TUI around run-scoped workflow ownership: concurrent runs have duration-first sidebar rows, bounded completed-run retention, foldable groups, chronological inspectors, and selected/all-run termination controls. Tool receipts, context usage, queued-input steering and recovery, safe Markdown links, initial CLI prompts, and path-aware compact rows are also reflected consistently.
- Extracted the chat reducer, lifecycle stages, transcript projection, queued-input ownership, run panels, and related rendering state into shared `lua/libs/chat` mechanisms with replaceable named handler groups. The bundled agent remains an explicit example composition, while downstream configs can replace individual stages without copying the whole chat surface.
- Added composition seams for providers, tools, read-only capabilities, validation policy, sessions, lead workflows, and CLI surfaces; clarified the engine/plugin/library/config ownership boundaries and hardened source, stable, and nightly distribution packaging with release-bundle validation.
- Hardened ChatGPT authentication and routing, model discovery, usage/quota refresh, compaction restore, structured output, tool calls, and image handling; fixed causal transcript and capability-preview duplication, queued-input reconciliation, overlapping MAG preview attribution, failed-run cleanup, and session-boundary UI reset.
- Approval mode now fails safe across startup, restart, and session resume: absent or stale durable authority resets to `safe`, replayed decisions do not reopen prompts, and explicit MAG human approvals remain separate from ordinary tool and write-review policy.
- **Breaking:** replaced MAG's ambiguous Bash helpers with `nefor.process.exec` and `nefor.shell.script`; removed the canonical `/mode` command; moved reusable Lua mechanisms under `lua/libs`, the bundled distribution under `examples/nefor-agent`, the MAG kernel under the MAG plugin, and installed runtime/plugin paths to generation-aware distribution roots. Pre-0.5 configs and sessions are not guaranteed to resume without migration.

## v0.4.0

- Replaced the reasoner-graph execution path with the run-scoped MAG actor kernel and made MAG the only workflow execution path.
- Made MAG a domain-neutral, typed, namespaced language that lowers library-defined programs to generic artifacts.
- Moved reusable Lua mechanisms out of starter composition into `lua/libs`, leaving starter-owned policy, prompts, and wiring behind explicit extension seams.
- Added MAG shell/tool/provider factories, lifecycle and run observability, output persistence, human injection, cancellation, and a standalone `mag` compiler CLI.
- Added mag-first lead tools and migrated the starter CLI/TUI workflows and deterministic mocks to the kernel contract.
- Added pre-public compatibility/versioning policy and automatic stable tagging from the committed workspace version.

## v0.3.0

- Introduced autonomous permission modes and buffered follow-up input while the chat workflow is busy.
- Added workflow cycles and decoupled bash command reasoning from the scheduler.
- Required builder commits before finalization and avoided creating empty session logs.
- Fixed mock-provider spawn graphs and preserved built-in combinators during the transition.

## v0.2.4

- Preserved system prompts when provider chats are created.

## v0.2.3

- Added native chat compaction, including pending-state UI and preservation of summary items and lead chat identity.

## v0.2.2

- Added reasoning-effort selection.

## v0.2.1

- Removed the fixed timeout from live SSE response bodies.

## v0.2.0

- Added image-aware tools and chat input, exact file edits, bounded/compact read-only tool output, and shared SSE parsing.
- Hardened session replay, event boundaries, interrupted workflows, provider tool-result history, and TUI replay/layout behavior.
- Split local test entry points, added pre-commit/pre-push hooks and the pre-public semantic-version policy, and made chat integration tests deterministic under shared environment state.
- Added Nix flake and Home Manager packaging and version-aware package-manager installation.

## v0.1.3

- **`openai-provider --auth-header NAME`**: send the API key under a non-standard header instead of `Authorization: Bearer …`. Useful for gateways that gate on a custom header (proxies, internal LLM routers). Defaults to `Authorization` so existing setups stay byte-identical.
- **CI automation**: release workflow now auto-bumps `amenocturne/homebrew-tap`'s `Formula/nefor.rb` after each tag push, and supports manual triggers via Actions UI (`workflow_dispatch` with a `tag` input).

## v0.1.2

UX fixes for the chat surface, surfaced during v0.1.1 bring-up.

- **No model configured**: openai-provider's hardcoded `qwen2.5-coder:7b` default is gone — `--model` is now optional, and `chat.create` without one fails with `NoModelConfigured` instead of silently dispatching against a model the user may not have. The error propagates through to the transcript as a clear sentence.
- **Provider error formatting**: HTTP errors from the upstream (Ollama, OpenAI) parse the response JSON's `error.message` field if present so the transcript shows e.g. `Error: HTTP 404: model 'X' not found` instead of the raw JSON envelope.
- **chat.error closes the in-flight node**: agentic_workflow now translates `<provider>.chat.error` to `chat.message.append` (system) and sends a node-result-err to reasoner-graph; the `[thinking…]` spinner stops on chat-create failures (previously hung forever). System messages also clear pending state.
- **Mid-conversation model switch retargets the active chat**: `/model` (or the picker) propagates the active orchestrator chat_id; openai-provider's `model.set` retargets that chat alongside the default. The new model sees the prior turns of the same conversation.
- **Tool-interrupt preserves chat history shape**: when the user interrupts during a tool call, the cancelled tool gets `(tool was interrupted by the user)` as its `tool_result`; any unstarted tools after it get `(tool not run; previous tool call in this turn was interrupted)`. The next turn has a valid OpenAI history shape and the model sees the cancellation context.
- **Session-resume suppresses the tool-permission popup**: replayed `chat.tool.permission_request` envelopes used to open a fresh approval popup even though the original session already had a recorded decision. chat.lua now tracks replay-mode (set by the new `from_resume` flag on `sessions.session_start`) and silently drops permission requests during replay.

## v0.1.1

Hotfix for brew-installed plugin discovery.

- Engine resolves `NEFOR_PLUGIN_DIR` via three new fallbacks before XDG: `<exe>/../share/nefor/plugins` (Homebrew layout), `<exe-dir>` if it bundles `nefor-tui` (in-tree dev), and only then `$XDG_DATA_HOME/nefor/plugins`. The resolved value is set as `NEFOR_PLUGIN_DIR` in the env so `init.lua`'s `bin()` helper sees it without configuration.
- `examples/nefor-agent/init.lua` `bin()` no longer falls back to `<config_parent>/target/debug/<name>`; the engine now propagates the resolved plugin dir.
- `examples/nefor-agent/init.lua` omits `--model` from the openai-provider spawn command when `PROVIDER_MODEL = nil` (instead of emitting a dangling `--model` flag with no value).

## v0.1.0 — initial public release

First public release. Everything in this version is plumbing toward a working agent harness.

- Pure string-bus engine with NCP v0.1 implemented in Lua.
- Declarative TUI plugin (`nefor-tui`) with 15 layout primitives, reconciler, line-diff renderer, and embedded Lua VM.
- OpenAI-compatible HTTP provider (`openai-provider`) targeting any OAI-shape endpoint (Ollama by default).
- Typed reasoner-graph scheduler (`reasoner-graph`) with cycles, per-firing lifecycle, fanout combinators.
- Tool-gate plugin with permission gating + `basic-tools` (`read_file` / `write_file` / `bash`).
- Generic provider/tool type-registry hubs for graph composition.
- Combinator algebra crate plus the corresponding NCP plugin.
- Lua starter config: chat surface composition, agentic workflow, session persistence, `agentic-cli` headless mode.
- XDG-style path resolution: `NEFOR_CONFIG_DIR`, `NEFOR_DATA_DIR`, `NEFOR_PLUGIN_DIR` (CLI flags `--config`, `--data-dir`, `--plugin-dir`).
- Homebrew install via `amenocturne/homebrew-tap`.
