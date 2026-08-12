# basic-tools

NCP plugin: file, image, search, edit, and process capability primitives.

The shipped tool set is `read_file`, `read_image`, `write_file`, `edit_file`,
`process.exec`, `shell.script`, and `search_text`. Destructive or long-running
operations are intended to be run behind `tool-gate` in the starter composition.

## Wire contract

Quick reference:

| Event                       | Direction            | Routing                               |
| --------------------------- | -------------------- | ------------------------------------- |
| `tool.register`             | basic-tools → bus    | broadcast (standalone mode)           |
| `tool-gate.tools.advertise` | basic-tools → gate   | private advertisement (`--gate`)      |
| `basic-tools.tool.invoke`   | caller → basic-tools | targeted (engine prefix-routing)      |
| `basic-tools.tool.cancel`   | caller → basic-tools | cancel by `{ id }` where supported    |
| `tool.result`               | basic-tools → bus    | broadcast (caller correlates by `id`) |

`tool.invoke`'s kind is prefixed with `basic-tools` so the engine's
`<peer>.<rest>` routing in `examples/nefor-agent/ncp.lua` delivers it directly to us.
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
- `search_text` — search text under files/directories.

### Process capabilities

`process.exec` is the structured default. It requires a non-empty `argv`, keeps
arguments separate through spawn, and never inserts a shell. Pipelines,
redirection, expansion, and shell built-ins therefore have no special meaning.
If Bash is specifically required, make it explicit in `argv`, for example
`["/bin/bash", "-lc", "set -o pipefail; rg -n TODO src/ | sort"]`.

`shell.script` is the explicit POSIX-shell surface. It requires a non-empty
`script` and executes exactly `["/bin/sh", "-c", script]`. It does not promise
Bash syntax; invoke `/bin/bash` explicitly when Bash semantics are part of the
program.

Both capabilities require a non-empty `cwd`. Relative paths are resolved by the
child process from that directory; in MAG, `nefor.process.cwd` is `"."`, meaning
the working directory inherited by the Nefor/MAG host. Both also require an
explicit timeout record: `no-timeout` is intentionally unbounded, while
`timeout-ms N` must use a positive millisecond value. An unbounded process that
never exits keeps its MAG run nonterminal.

Direct tool invocations may pass optional string `stdin`. In a MAG graph a
`Unit` input starts the process with no stdin, while an upstream
`nefor.contracts.Text` value supplies its `content` as stdin. The result is
structured data with independent `stdout`, `stderr`, and `termination`; MAG
normalizes termination to `{ kind = "code" | "signal", value = N }`. A nonzero
exit code is still result data. Validation, spawn, I/O, timeout, and cancellation
failures use the error channel; timeout and cancellation diagnostics retain
partial stdout/stderr after killing and reaping the dedicated process group.
Both capabilities support `basic-tools.tool.cancel { id }`.

These two names replace the old ambiguous command helpers; there is no current
`bash`, `BashOptions`, `command-with-options`, or `pipe-command` API.

In the starter these are composed behind `tool-validator` and `tool-gate`, so
mutation/execution can be auto-approved, prompted, or denied according to the
active policy/mode.

## Running

basic-tools is composed into the default starter. To run it ad-hoc against a
fake engine, see [`tools/fake-engine`](../../tools/fake-engine).
