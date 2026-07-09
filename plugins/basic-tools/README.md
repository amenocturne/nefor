# basic-tools

NCP v0.1 plugin: file, image, search, edit, and shell tool primitives.

The shipped tool set is `read_file`, `read_image`, `write_file`, `edit_file`,
`bash`, and `search_text`. Destructive or long-running operations are intended
to be run behind `tool-gate` in the starter composition.

## Wire contract

See [`starter/chat/README.md`](../../starter/chat/README.md) → "Tool calling
(v1)" for the canonical spec. Quick reference:

| Event                     | Direction            | Routing                               |
| ------------------------- | -------------------- | ------------------------------------- |
| `tool.register`             | basic-tools → bus    | broadcast (standalone mode)           |
| `tool-gate.tools.advertise` | basic-tools → gate   | private advertisement (`--gate`)      |
| `basic-tools.tool.invoke`   | caller → basic-tools | targeted (engine prefix-routing)      |
| `basic-tools.tool.cancel`   | caller → basic-tools | cancel by `{ id }` where supported    |
| `tool.result`               | basic-tools → bus    | broadcast (caller correlates by `id`) |

`tool.invoke`'s kind is prefixed with `basic-tools` so the engine's
`<peer>.<rest>` routing in `starter/ncp.lua` delivers it directly to us.
`tool.register` and `tool.result` are unprefixed because consumers — the
provider's tool-call loop, debug listeners — need to see them.

When started with `--gate <name>`, basic-tools suppresses public
`tool.register` events and instead sends a private `<name>.tools.advertise`
event. Private advertisements include internal `context.folders` metadata;
public registrations strip that context before tools are exposed to models.

## v1 tool list

### `read_file`

Reads the contents of a UTF-8 text file. Arguments: `path` (required), optional
`cwd`, optional `offset`, and optional `max_bytes` (default/cap: 1 MiB). Returns
the requested UTF-8 slice on success or a human-readable error. If the file has
more data after the returned slice, the output includes a continuation marker
with the next offset to request.

Rejects:

- Missing file → `file not found: <path>`
- Path is a directory → `path is a directory: <path>`
- File larger than 1 MiB → `file too large (<N> bytes; cap is 1 MiB): <path>`
- Binary content (NUL byte in first 8 KiB) → `file appears to be binary: <path>`
- Invalid UTF-8 → `file is not valid UTF-8: <path>`
- IO error → `io error reading <path>: <message>`

`read_file` deliberately does NOT validate path traversal or sandbox. The
caller passes whatever path it wants; starter safety comes from routing tools
through the validator/gate layers before basic-tools sees mutation/execution
requests.

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

### Other shipped tools

- `write_file` — write text content to a path.
- `edit_file` — replace one exact string match in an existing text file.
- `bash` — run a shell command; supports cancellation via
  `basic-tools.tool.cancel { id }`.
- `search_text` — search text under files/directories.

In the starter these are composed behind `tool-validator` and `tool-gate`, so
mutation/execution can be auto-approved, prompted, or denied according to the
active policy/mode.

## Running

basic-tools is composed into the default starter. To run it ad-hoc against a
fake engine, see [`tools/fake-engine`](../../tools/fake-engine).
