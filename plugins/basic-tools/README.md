# basic-tools

NCP v0.1 plugin: file/bash/etc. tool primitives.

v1 ships a single tool — `read_file`. `write_file` and `bash` land later
behind permission gating; the wire contract is shaped to accommodate them
without changes.

This is phase 1 of nefor's tool-calling story. Phase 2 wires the tool
catalog through `openai-provider`'s API tool-call loop so model-driven calls
flow `LLM → openai-provider → basic-tools → result`.

## Wire contract

See [`docs/chat-contract.md`](../../docs/chat-contract.md) → "Tool calling
(v1)" for the canonical spec. Quick reference:

| Event              | Direction       | Routing   |
|--------------------|-----------------|-----------|
| `tool.register`    | basic-tools → bus | broadcast |
| `basic-tools.tool.invoke` | caller → basic-tools | targeted (engine prefix-routing) |
| `tool.result`      | basic-tools → bus | broadcast (caller correlates by `id`) |

`tool.invoke`'s kind is prefixed with `basic-tools` so the engine's
`<peer>.<rest>` routing in `starter/ncp.lua` delivers it directly to us.
`tool.register` and `tool.result` are unprefixed because consumers — the
provider's tool-call loop, debug listeners — need to see them.

## v1 tool list

### `read_file`

Reads the contents of a UTF-8 text file. Returns the text on success or a
human-readable error.

Rejects:

- Missing file → `file not found: <path>`
- Path is a directory → `path is a directory: <path>`
- File larger than 1 MiB → `file too large (<N> bytes; cap is 1 MiB): <path>`
- Binary content (NUL byte in first 8 KiB) → `file appears to be binary: <path>`
- Invalid UTF-8 → `file is not valid UTF-8: <path>`
- IO error → `io error reading <path>: <message>`

v1 deliberately does NOT validate path traversal or sandbox. The caller
passes whatever path they want; basic-tools is trusted on the bus. Sandboxing
lands with the permission-gating story alongside `write_file` and `bash`.

## Future direction

- `write_file` — file writes, gated behind a permission popup that the user
  approves once per session per path.
- `bash` — shell command execution, gated behind the same popup with a
  per-command-prefix allowlist.
- A permission-gating layer between provider plugins and basic-tools so
  destructive ops require user consent before basic-tools sees them.

## Running

basic-tools is composed into the default starter. To run it ad-hoc against a
fake engine, see [`tools/fake-engine`](../../tools/fake-engine).
