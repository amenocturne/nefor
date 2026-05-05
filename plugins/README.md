# plugins/

Process-isolated NCP plugins. Each plugin is its own crate with a binary entry point.

## Layout

- `nefor-tui/` — declarative TUI: layout primitives + reconciler + line-diff renderer + Lua VM.
- `openai-provider/` — OpenAI-compatible HTTP provider.
- `reasoner-graph/` — typed graph scheduler with cycles and per-firing lifecycle.
- `tool-gate/` — tool advertisement + permission gate.
- `basic-tools/` — `read_file` / `write_file` / `bash` built-ins.
- `generic-provider/`, `generic-tool/` — passive type-registry hubs for graph composition.
- `nefor-combinators/` — typed combinator registry.
- `mock-plugin/` — scriptable NCP actor for integration tests.

## Authoring

A plugin reads NCP envelopes from stdin and writes them to stdout, line-delimited. See `protocol/v0.1/spec.md` for the wire shape and `docs/plugin-authoring.md` for guidance.
