# chatgpt-provider

NCP plugin: talks to OpenAI's Responses API using ChatGPT-subscription
OAuth credentials. Same multi-instance shape as `openai-provider` (`--name`
flag sets the event-kind prefix), but targets the ChatGPT backend
(`https://chatgpt.com/backend-api/codex`) instead of the standard
`/v1/chat/completions` path.

Includes a standalone OAuth PKCE login flow (`chatgpt-provider login`) that
persists tokens to `$XDG_DATA_HOME/nefor/chatgpt-auth.json`. The plugin
mode (default, no subcommand) runs as an NCP stdio plugin.

The model list is fetched from the backend at runtime -- no `--model` CLI
flag. Users pick via `/model` in the chat surface.

## Wire contract

Same chat-scoped event shape as `openai-provider` with `chatgpt` as the
default prefix: `<prefix>.chat.create`, `.chat.append`, `.chat.complete`,
`.chat.delete`, stream events (`<prefix>.stream.delta` / `.stream.end`),
session stats, auth status, model list/status, and lifecycle events. Tool
calling is supported via a `ToolBroker` that consumes `tool.register`, invokes
registered tools, and correlates `tool.result` events back to the in-flight
turn.

Image media returned by tools such as `read_image` is converted to Responses
API `InputImage` items for vision-capable models. If the active model cannot
accept images, the provider returns an explicit model-capability error instead
of silently dropping the media.

## Run

Spawned by the engine over stdio. Use `chatgpt-provider login` first to
bootstrap OAuth credentials, then spawn normally in `init.lua`.
