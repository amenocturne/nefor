# chatgpt-provider

NCP plugin: talks to OpenAI's Responses API using ChatGPT-subscription
OAuth credentials. Same multi-instance shape as `openai-provider` (`--name`
flag sets the event-kind prefix), but targets the ChatGPT backend
(`https://chatgpt.com/backend-api/codex`) instead of the standard
`/v1/chat/completions` path.

Includes a standalone OAuth PKCE login flow (`chatgpt-provider login`) that
persists tokens to `$XDG_DATA_HOME/nefor/chatgpt-auth.json`. The plugin
mode (default, no subcommand) runs as an NCP stdio plugin. A running plugin
refreshes OAuth access tokens five minutes before their JWT expiry, serializes
concurrent refreshes, and can adopt a same-account login written by another
process. A Responses 401 is recovered with one credential reload and one
forced refresh before the plugin asks the user to log in again.

The model list is fetched from the backend at runtime -- no `--model` CLI
flag. Users pick via `/model` in the chat surface.

Account quota is read from the ChatGPT usage endpoint at startup, every five
minutes, and on `usage.requested`. Successful Responses headers also refresh
the same snapshot without another request. The starter compositor maps these
native events to `chatgpt/subscription` through conversation-manager's common
usage interface without changing the provider's polling behavior.

## Wire contract

Same chat-scoped event shape as `openai-provider` with `chatgpt` as the
default prefix: `<prefix>.chat.create`, `.chat.append`, `.chat.complete`,
`.chat.delete`, stream events (`<prefix>.stream.delta` / `.stream.end`),
session stats, auth status, model list/status, and lifecycle events. Tool
calling is supported via a `ToolBroker` that consumes `tool.register`, invokes
registered tools, and correlates `tool.result` events back to the in-flight
turn.

Usage adds `<prefix>.usage.requested`, `.usage.updated`, and `.usage.error`.
The update payload carries the backend's primary/secondary windows, reset
timestamps, plan type, and credits without deriving quota from local tokens.

Direct completion accounting publishes the occupancy of the exact lowered
Responses request before it is sent, then replaces that estimate with backend
input usage when the response completes. The local estimate is
`ceil(serialized_request_json_bytes / 4)`: because it runs after provider
lowering, instructions, native/checkpoint items, attachments, tool schemas,
structured output, and reasoning controls each enter through their one wire
representation. Events carry only counts and accuracy, never request content.
Aggregate input/output usage remains separate from current request occupancy.

The provider's Lua adapter defines six ordinary routed read tools with the
shared `libs.tool` constructor and advertises them through the
composition-selected tool gate: `web_search`, `web_open`, `web_click`,
`web_find`, `web_image_search`, and `web_screenshot`. They are ordinary
function tools in Responses requests. When invoked, the gate routes them back
to this provider under an independent correlation ID; the provider's standalone
web client posts exactly one supported command family to `POST /alpha/search`.
Each call uses the model carried by the originating model invocation and a
stable provider routing identity derived from its conversation scope, so
search/open/click/find/screenshot references remain usable across related calls.
Calls execute concurrently outside the completion `ToolBroker`; cancellation
removes only the named web execution, emits one cancellation result, and ignores
late completion.

Successful results preserve the endpoint's exact plaintext output, optional
opaque result JSON, and opaque encrypted provider state. Empty text remains
empty. Errors remain specific and are returned as ordinary tool errors; an
observed screenshot-resolution diagnostic can carry both the error marker and
the unchanged provider output as evidence. Arguments are checked against each
tool's closed schema in Lua; invalid calls settle there with the constructor's
uniform error (for example ``web_open: unknown argument `foo`; accepted
arguments: url, line``) and never reach this process. `web_image_search` remains `image_query`. For `web_screenshot`,
open the PDF first and pass the provider-issued PDF reference from that output;
direct PDF URLs may fail semantically even with HTTP 200. A resolved screenshot
currently returns plaintext plus an opaque reference, not image bytes, so media
decoding is not implemented or claimed.

The private web request kinds are not public bus capabilities. The provider
compositor accepts routed invoke/cancel traffic only from its selected tool gate
and delivers the lowered request under engine identity; the Rust boundary then
revalidates provider, model, invoking actor, gate/capability correlation, and the
shared companion-LLM conversation identity before HTTP. Large structured web
results use the ordinary tool-gate output policy: values over the inline budget
are JSON-encoded into the established `tool-results` store, while the routed
result carries a bounded summary and retrievable `output_path` into model
projection.

Direct completions and compaction chats accept an optional closed
`provider_options` object. Its only supported field is
`service_tier: "fast"`, which maps to `service_tier: "priority"` on Responses requests,
matching Codex's ChatGPT subscription transport;
omission (or an empty object) keeps the standard service tier. ChatGPT owns
this validation and rejects unknown fields or tier values before HTTP.

## Stream recovery

A Responses attempt is provisional until the provider emits a semantic terminal
event. Transient connection resets, premature EOFs, idle timeouts, and transient
provider failures therefore discard the entire attempt and replay the same
provider round even when text, reasoning, native output, or tool-call fragments
have already streamed. Discarded content remains available in the canonical
conversation audit, but is retracted from the TUI and excluded from future model
context. Tool calls are delivered only after a successful terminal event, so an
abandoned attempt cannot have executed a local tool.

Recovery continues until success or explicit cancellation by default. Backoff
uses full jitter with a 30-second ceiling, and one provider-process recovery gate
admits a single half-open probe after a shared outage so concurrent agents do not
retry in lockstep. Set `--stream-retry-timeout-seconds <seconds>` only when an
embedding needs a finite elapsed recovery window.

Image media returned by tools such as `read_image` is converted to Responses
API `InputImage` items for vision-capable models. If the active model cannot
accept images, the provider returns an explicit model-capability error instead
of silently dropping the media.

Responses output items are preserved as opaque, provider/model-scoped context
on their canonical assistant messages. Completed items are ordered by
`output_index`; missing indices follow indexed items in stable arrival order,
while duplicate indices fail the turn. Compatible continuations replay the
original encrypted reasoning, message, web-search, and function-call items
before tool results; retained web-search IDs stay in provider context but are
omitted from normal non-stored ChatGPT request input, matching Codex lowering.
Session replay reconstructs the same context. Public conversation and display
projections keep only the provider-neutral message and never expose the
encrypted artifact.

## Updating model compatibility

When adding support for newly released models, refresh the authoritative Codex
checkout and inspect the newest `rust-v*` release tag. Codex derives the
`client_version` sent to its model catalog from the whole major, minor, and
patch version in `codex-rs/Cargo.toml`; verify that path through
`codex-rs/models-manager/src/lib.rs` and `manager.rs`, then copy the resulting
version to `responses::CODEX_COMPAT_CLIENT_VERSION`. A stale value can hide
catalog models whose `minimal_client_version` gate is newer. Run
`just test-provider` from the repository root after updating it.

## Run

Spawned by the engine over stdio. Use `chatgpt-provider login` first to
bootstrap OAuth credentials, then spawn normally in `init.lua`.

## Output admission and transport loss

Every NCP output is admitted through the SDK's checked framing boundary. Its
success means enqueue admission, not a successful stdout write or consumer
acceptance. Stream deltas are awaited in the invocation itself; a full output
queue applies backpressure instead of dropping progress or spawning relay tasks.

A rejected turn output stops the turn and drops its active response reader. A
direct completion emits one bounded `completion.event` with its actual
`request_id`, `event="error"`, and `message`; it does **not** report pre-start
`event="failed"` for accepted work. Chat turns instead emit `chat.error` with
`chat_id` and `message`. Admission terminals carry
`error_category="output_admission_failed"` and `effects="unknown"`. The diagnostic
states that earlier or external effects may have occurred; no work is retried
because its output was rejected. Routed web output uses the existing
`web.result` error with the original invocation `id`. Identities are never
truncated. Failed fallback publication ends the plugin rather than recursively
attempting another error. Hard cancellation suppresses turn terminals.

Turn and completion slots are released by their exact generation, including
when a chat has been deleted and recreated. Required uncorrelated output
failure, writer closure, and terminal stdin faults end the local transport;
owned HTTP/background tasks are cancelled and joined before dispatcher exit.
Completed tasks are reaped during operation. Bounded malformed JSON remains
parse noise. Transport loss leaves accepted effects unknown; downstream request
owners must reconcile capability loss without replaying accepted work.
