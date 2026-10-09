# basic-tools

NCP plugin: file, image, edit, and process capabilities.

The shipped capabilities are `read_file`, `read_image`, `write_file`,
`process.exec`, and `shell.script`. The plugin defines no model-facing tools:
the tools of the same names — descriptions, parameter schemas, display
contracts, argument validation, and tool-gate advertisement — are Lua
definitions in [`lua/libs/basic-tools`](../../lua/libs/basic-tools/init.lua),
built with [`libs.tool`](../../lua/libs/tool/init.lua). The starter wraps this
process with `libs.compositors.tools.basic_actor_spec`, which advertises those
tools to `tool-gate` as source `basic-tools` and forwards each validated
invocation to the capability. Destructive or long-running operations are run
behind `tool-gate` in the starter composition.

## Wire contract

| Event                                                    | Direction            |
| -------------------------------------------------------- | -------------------- |
| `basic-tools.capability.invoke { id, capability, args }` | caller → basic-tools |
| `basic-tools.capability.cancel { id }`                   | caller → basic-tools |
| `capability.stream { id, stream, text }`                 | basic-tools → caller |
| `capability.result { id, output \| error }`              | basic-tools → caller |

Each capability still parses its own typed input and reports malformed
arguments as `capability.result { error }`; model-facing argument validation
happens in Lua before a request reaches the plugin. `--read-file-max-bytes`
supplies the read_file cap; the composition passes the same value to the Lua
`read_file` definition, which advertises it as the `max_bytes` maximum.

## Capabilities

### `read_file`

UTF-8 slices preserve the explicit byte cap. An artificial cap cut floors to
its complete scalar prefix; if no scalar fits, the tool returns a bounded-read
argument error rather than a nonprogressing continuation. Incomplete UTF-8 at
actual EOF is an error, not a successful truncated read.

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

### Other shipped capabilities

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
Both capabilities support `basic-tools.capability.cancel { id }`.

These two names replace the old ambiguous command helpers; there is no current
`bash`, `BashOptions`, `command-with-options`, or `pipe-command` API.

In the starter these are composed behind `tool-validator` and `tool-gate`, so
mutation/execution can be auto-approved, prompted, or denied according to the
active policy/mode.

## Running

basic-tools is composed into the default starter. To run it ad-hoc against a
fake engine, see [`tools/fake-engine`](../../tools/fake-engine).

Process capture retains the first and last 512KiB of each stream. Up to 1MiB
is complete; larger output omits the middle with an exact raw-byte count.
Cuts are byte-based: valid UTF-8 characters split at retained edges may be
replaced lossily. Results and timeout/cancel diagnostics are bounded captures,
not recoverable transcripts.

Live preview independently keeps the latest 16KiB per stream per drain.
The internal `PreviewChunk { bytes, skipped_bytes }` reports source-byte loss
immediately before those bytes; marker-like source text is ordinary data.
The decoder helper defers incomplete UTF-8 suffixes across contiguous spans,
finishes/resets at an explicit discontinuity, and flushes a partial EOF suffix
lossily exactly once. Complete invalid sequences use whole-buffer lossy
replacement semantics. The wire adapter maintains independent incremental
decoders per stream and publishes explicit `skipped_bytes` alongside textual
gap markers. The invocation deadline includes pipe and stdin draining after
the group leader exits. During termination the owner repeats group signalling
until pipe/stdin tasks finish, retaining partial captures rather than abandoning
readers held open by a concurrently spawned descendant. Descendants that escape
the dedicated process group and unconditional OS cleanup guarantees are outside
this mechanism.

Pipe readers never await the outbound queue. With 8192-byte reads, lazy buffers
have logical lengths of at most 32KiB pending preview and 512KiB head plus 1MiB
tail per stream after push; transient overshoot is at most one read. Finalized
raw capture is at most 1MiB plus its marker. These are not Vec capacity, RSS,
serialized-frame or global/concurrent invocation bounds.

## Output admission and transport loss

Every outgoing frame is checked before enqueue (16 MiB including LF). Admission
is not proof of write, receipt, or execution. A rejected result or stream stops
that invocation's output and produces one checked `tool.result {id, error}` with
the actual invocation ID, `frame_too_large` or `invalid_framing`, and an explicit
warning that earlier/external effects may have occurred. No operation is retried.
Stream publication failure cancels, kills, and reaps its owned process group
before the terminal error; the cancellation registration is released, and later
independent invocations remain usable.

If the constant diagnostic plus unmodified ID also cannot fit, or the writer is
closed, the error reaches the task supervisor. It cancels and joins owned work
before exiting. Terminal stdin framing, UTF-8, and I/O faults take the same
cleanup path; malformed bounded JSON remains parse noise. Accepted effects are
unknown after transport loss. Required startup/advertisement failures are fatal;
goodbye is checked but best effort during selected teardown. Frame admission is separate from the per-stream logical capture and preview
bounds; IDs, rendered errors and serialization add further memory.
