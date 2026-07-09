# plugins/

Process-isolated NCP plugins. Each plugin is its own crate with a binary entry point.

## Layout

- `nefor-tui/` — declarative TUI: layout primitives + reconciler + line-diff renderer + Lua VM.
- `openai-provider/` — OpenAI-compatible `/v1/chat/completions` HTTP provider.
- `chatgpt-provider/` — ChatGPT-subscription Responses API provider with OAuth login.
- `mag/` — MAG actor kernel: executes compiled `.mag` programs as actor constellations.
- `tool-gate/` — tool advertisement + permission gate.
- `basic-tools/` — file/image/search/edit/shell tools (`read_file`, `read_image`, `search_text`, `write_file`, `edit_file`, `bash`).
- `generic-provider/`, `generic-tool/` — passive hubs that emit type-registry events for graph composition.
- `mock-plugin/` — scriptable NCP actor for integration tests.

## Authoring

A plugin reads NCP envelopes from stdin and writes them to stdout, line-delimited. See `protocol/v0.1/spec.md` for the wire shape and `docs/plugin-authoring.md` for guidance.
