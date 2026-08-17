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

Image media returned by tools such as `read_image` is converted to Responses
API `InputImage` items for vision-capable models. If the active model cannot
accept images, the provider returns an explicit model-capability error instead
of silently dropping the media.

## Run

Spawned by the engine over stdio. Use `chatgpt-provider login` first to
bootstrap OAuth credentials, then spawn normally in `init.lua`.
