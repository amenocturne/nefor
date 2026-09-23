# basic-tools

NCP plugin: file, image, edit, and process capability primitives.

The shipped tool set is `read_file`, `read_image`, `write_file`,
`process.exec`, and `shell.script`. Destructive or long-running
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

Reads the contents of a regular UTF-8 text file. Symlinks to regular files are
accepted. Arguments: `path` (required), optional
`cwd`, optional `offset`, and optional `max_bytes`. The composition supplies
the default/cap through `--read-file-max-bytes`; requests above it are rejected
rather than silently clamped. Returns
the requested UTF-8 slice on success or a human-readable error. If the file has
more data after the returned slice, the output includes a continuation marker
with the next offset to request.

Rejects:

- Missing file → `file not found: <path>`
- Path is a directory → `path is a directory: <path>`
- Path is another filesystem object, such as a device, FIFO, or socket → `path is not a regular file: <path>`
- Unsliced file larger than the configured maximum → `file too large (<N> bytes; ...): <path>`
- Binary content (NUL byte in first 8 KiB) → `file appears to be binary: <path>`
- Invalid UTF-8 → `file is not valid UTF-8: <path>`
- IO error → `io error reading <path>: <message>`

`read_file` deliberately does NOT validate path traversal or sandbox. The
caller passes whatever path it wants; starter safety comes from routing tools
through the validator/gate layers before basic-tools sees mutation/execution
requests.

### `read_image`

Reads a regular image file and returns a structured media object. Symlinks to
regular image files are accepted; devices, FIFOs, sockets, and other special
filesystem objects are rejected before they are opened.

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

- `write_file` — with `path` and `new_string`, create or overwrite a complete
  text file; add non-empty `old_string` to replace exactly one match in an
  existing UTF-8 file. An empty `new_string` is valid in both forms. The tool
  has no `cwd`, mode, action, or model-controlled policy fields.

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
the working directory inherited by the Nefor/MAG host. Both capabilities require
an optional-millisecond timeout record: `{present: false, milliseconds: 0}` is
unbounded; `{present: true, milliseconds: N}` requires positive milliseconds.
MAG authors use nominal `Timeout` constructors (`Unlimited`, `Milliseconds`,
`Seconds`, or `Minutes`). Process and shell node construction normalizes them to
this same wire record, rejecting nonpositive and overflowing durations during
compilation. The external tool API does not accept MAG constructor envelopes.
An unbounded process that never exits keeps its MAG run nonterminal.

Direct tool invocations may pass optional string `stdin`. In a MAG graph a
`Unit` input starts the process with no stdin; the MAG node constructors expose
no stdin-bearing input. The result is
structured data with independent `stdout`, `stderr`, and `termination`; MAG
translates the raw capability result into the nominal `ProcessExited {code}` or
`ProcessSignaled {signal}` constructor. A nonzero exit code is still result
data. Validation, spawn, I/O, timeout, and cancellation
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
