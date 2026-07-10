# tool-gate

NCP plugin: per-tool permission gate. Transparent proxy between
providers and tool-providing plugins (basic-tools, etc.).

Tool sources advertise privately to the gate via
`tool-gate.tools.advertise`. The gate aggregates and re-emits a single
public `tool.register` so providers see one canonical registry with
`tool-gate.tool.invoke` as the entry point.

Private advertisements may carry internal `context.folders` metadata. The Rust
gate strips private context from the public registry and enforces the transport
policy; it does not load instruction files or synthesize reminders.

The shipped starter Lua composition wraps the gate with
`lua/libs/instruction-files` and `plugins/tool-gate/lua/tool-gate/agents_md.lua`.
That wrapper consumes `context.folders` before forwarding outbound tool invokes
and emits path-only, one-shot reminders for nearby `AGENTS.md` / `CLAUDE.md`
files. Reminders are de-duped per chat/scope. File contents are not loaded
automatically; agents must read the referenced files explicitly when relevant.

Per-tool policy via CLI flags:

- `--auto <name>` -- forward without prompting
- `--prompt <name>` -- emit permission request, wait for user approval
- `--deny <name>` -- reject immediately
- `--default <auto|prompt|deny>` -- fallback for unlisted tools (default: `prompt`)

Runtime modes are `safe`, `auto`, and `yolo`. The starter's full mode × action
class table lives in [`docs/approval-model.md`](../../docs/approval-model.md).
At the transport layer, `yolo` overrides all policies to auto-approve.

## Runtime events

- `tool-gate.set_mode` `{ mode }` switches the transport mode (`safe`, `auto`,
  or `yolo`).
- `tool-gate.mode_changed` announces the active mode after a change.
- `tool.permission_response` answers a pending prompt with approval/rejection.
- `tool-gate.tool.cancel` forwards cancellation for an in-flight tool call.

The starter starts `tool-validator` before the gate and routes prompt requests
through it. The validator may auto-approve, deny, or defer to the chat popup
before a request reaches the user.
