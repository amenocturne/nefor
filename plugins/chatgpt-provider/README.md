# chatgpt-provider

NCP v0.1 plugin: talks to OpenAI's Responses API using ChatGPT-subscription
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

Same event shape as `openai-provider` (`<prefix>.stream.delta`,
`<prefix>.stream.end`, `<prefix>.session.stats`, etc.) with `chatgpt` as
the default prefix. Tool calling is supported via a `ToolBroker` that
correlates `tool.result` events back to the in-flight turn.

## Run

Spawned by the engine over stdio. Use `chatgpt-provider login` first to
bootstrap OAuth credentials, then spawn normally in `init.lua`.
