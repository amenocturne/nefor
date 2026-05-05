# Changelog

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
