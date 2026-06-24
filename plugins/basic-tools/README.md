# basic-tools

NCP v0.1 plugin: file/bash/etc. tool primitives.

v1 ships read-only file primitives plus gated mutation/execution tools.

This is phase 1 of nefor's tool-calling story. Phase 2 wires the tool
catalog through `openai-provider`'s API tool-call loop so model-driven calls
flow `LLM → openai-provider → basic-tools → result`.

## Wire contract

See [`starter/chat/README.md`](../../starter/chat/README.md) → "Tool calling
(v1)" for the canonical spec. Quick reference:

| Event                     | Direction            | Routing                               |
| ------------------------- | -------------------- | ------------------------------------- |
| `tool.register`           | basic-tools → bus    | broadcast                             |
| `basic-tools.tool.invoke` | caller → basic-tools | targeted (engine prefix-routing)      |
| `tool.result`             | basic-tools → bus    | broadcast (caller correlates by `id`) |

`tool.invoke`'s kind is prefixed with `basic-tools` so the engine's
`<peer>.<rest>` routing in `starter/ncp.lua` delivers it directly to us.
`tool.register` and `tool.result` are unprefixed because consumers — the
provider's tool-call loop, debug listeners — need to see them.

When basic-tools advertises privately through `tool-gate.tools.advertise`, each
tool also includes internal `context.folders` metadata. The gate wrapper uses
that metadata for runtime hooks such as instruction-file reminders. This field
is stripped from public `tool.register` and is not exposed to models.

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

### `read_image`

Reads an image file and returns a structured media object:

```json
{
  "type": "media",
  "media_type": "image/png",
  "filename": "screenshot.png",
  "data": "<base64>"
}
```

Supported formats are PNG, JPEG, GIF, and WebP, detected from file bytes.
Images over 5 MiB are downscaled and re-encoded as JPEG before being returned;
the source file read has a 50 MiB hard cap. The tool does not describe or OCR
the image; providers either pass the media to a vision-capable model or replace
it with an explicit error when the active model does not support image input.

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
