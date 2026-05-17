# tool-gate

NCP v0.1 plugin: per-tool permission gate. Transparent proxy between
providers and tool-providing plugins (basic-tools, etc.).

Tool sources advertise privately to the gate via
`tool-gate.tools.advertise`. The gate aggregates and re-emits a single
public `tool.register` so providers see one canonical registry with
`tool-gate.tool.invoke` as the entry point.

Per-tool policy via CLI flags:

- `--auto <name>` -- forward without prompting
- `--prompt <name>` -- emit permission request, wait for user approval
- `--deny <name>` -- reject immediately
- `--default <auto|prompt|deny>` -- fallback for unlisted tools (default: `prompt`)

Yolo mode: `tool-gate.set_mode { mode: "yolo" }` overrides all policies
to auto-approve. Useful for unattended testing.
