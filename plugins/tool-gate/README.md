# tool-gate

NCP v0.1 plugin: per-tool permission gate. Transparent proxy between
providers and tool-providing plugins (basic-tools, etc.).

Tool sources advertise privately to the gate via
`tool-gate.tools.advertise`. The gate aggregates and re-emits a single
public `tool.register` so providers see one canonical registry with
`tool-gate.tool.invoke` as the entry point.

Private advertisements may carry internal `context.folders` metadata. The
starter wrapper records that metadata and composes it with the shared
`libs.instruction-files` Lua library to emit one-shot reminders about nearby
`AGENTS.md` / `CLAUDE.md` files when a tool call touches a folder. Reminder
messages list file paths only; contents are never loaded automatically. Agents
read relevant instruction files through normal file tools.

Per-tool policy via CLI flags:

- `--auto <name>` -- forward without prompting
- `--prompt <name>` -- emit permission request, wait for user approval
- `--deny <name>` -- reject immediately
- `--default <auto|prompt|deny>` -- fallback for unlisted tools (default: `prompt`)

Runtime modes are `safe`, `auto`, and `yolo`. The starter's full mode × action
class table lives in [`docs/approval-model.md`](../../docs/approval-model.md).
At the transport layer, `yolo` overrides all policies to auto-approve.
