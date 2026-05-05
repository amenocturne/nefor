# Changelog

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
- `starter/init.lua` `bin()` no longer falls back to `<config_parent>/target/debug/<name>`; the engine now propagates the resolved plugin dir.
- `starter/init.lua` omits `--model` from the openai-provider spawn command when `PROVIDER_MODEL = nil` (instead of emitting a dangling `--model` flag with no value).

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
