# tool-gate

NCP plugin: per-tool permission gate. Transparent proxy between
providers and tool sources (the Lua `libs.tool` sources wrapping
basic-tools and git-worktree, lead-workflow, read-only-tools, provider web
tools). The gate only decides whether an invocation may execute; argument
validation belongs to each tool's source, which checks the arguments it
actually receives, including arguments an approval replaced.

Tool sources advertise privately to the gate via
`tool-gate.tools.advertise`. The gate aggregates and re-emits a single
public `tool.register` so providers see one canonical registry with
`tool-gate.tool.invoke` as the entry point. Each public descriptor retains its
advertising `owner`; MAG snapshots this owner-qualified registry and projects
authored agent allowlists through it for every provider request. A profile can
therefore contain a stale name without advertising a nonexistent tool or
aborting the provider turn.
Advertisement `source` and every tool `name` must be non-empty strings;
whitespace-only identifiers are rejected. Any invalid advertisement emits a
deterministic `tool-gate.advertise_error` and leaves the prior valid registry
unchanged.

## Calls to a tool the caller cannot use

A `tool-gate.tool.invoke` may carry `allowlist`, the tool names its caller may
call. A name outside it and a name no source advertises get the same
`tool.result` error, so a caller cannot tell an excluded tool from a
nonexistent one:

```text
unknown tool `bash`; available tools: read_file, mag-apply
```

The list is what that caller's model was offered, in the same order: the
allowlist in its own order, less the names no source advertises (providers
project the same names out of a request). Without an allowlist it is the whole
registry in `tool.register` order. A caller that offered its model tools the
gate never routes names them in `request_local_tools`; they follow the
allowlisted ones. With nothing to list the message ends `no tools are
available`. The gate owns this wording because it alone holds the registry;
`libs.tool` words the identical message for a single source, in the style of
its uniform `unknown argument` message.

Private advertisements may carry internal `context.folders` metadata. The Rust
gate strips private context from the public registry and enforces the transport
policy; it does not load instruction files or synthesize reminders.

The shipped starter Lua composition wraps the gate with
`lua/libs/instruction-files` and `plugins/tool-gate/lua/tool-gate/agents_md.lua`.
That wrapper consumes `context.folders` before forwarding outbound tool invokes
and emits path-only, one-shot reminders for nearby `AGENTS.md` / `CLAUDE.md`
files. Reminders are de-duped per chat/scope. File contents are not loaded
automatically; agents must read the referenced files explicitly when relevant.

## What happens to a call

The chain is always: a caller invokes a tool, the gate decides, and the tool
executes or the caller receives an error saying why it did not. Nothing
executes before the decision.

1. `tool-gate.tool.invoke { id, name, args, approval_policy, allowlist? }`.
   `approval_policy` is the calling agent's policy by name, `ReadOnly` or
   `Default`, and is required: a call that states none, or another value, is
   answered with an error. Nothing is assumed for a caller that does not say.
2. The gate sends `<source>.tool.prepare` with the same fields. The source
   checks the arguments, runs the tool's `prepare` step and its decision
   function, and answers `tool.prepared { id, decision, reason? }` with
   `decision` one of `safe`, `unsure`, `denied`; `unsure` and `denied` carry a
   reason. A source may instead end the call with `tool.result { error }`.
   Every source must answer one way or the other; `libs.tool` sources do.
3. The gate applies the decision table to the caller's policy, the permission
   mode and the assessment, policy first:

   | assessment | ReadOnly (every mode) | Default, safe | Default, auto | Default, yolo |
   | ---------- | --------------------- | ------------- | ------------- | ------------- |
   | safe       | run                   | run           | run           | run           |
   | unsure     | deny                  | ask the user  | run           | run           |
   | denied     | deny                  | deny          | deny          | run           |

   Run: `<source>.tool.invoke` under the id the source prepared, with
   `approval_policy`. Ask: `chat.tool.permission_request { id, tool, args,
   approval_policy, reason? }`, answered by `tool.permission_response`. Deny:
   a `tool.result` error to the caller and `<source>.tool.cancel` so the
   source drops what it prepared.

A `ReadOnly` caller is never asked about and no mode widens it. The table is
`verdict` in [`src/policy.rs`](src/policy.rs) and exists nowhere else. The
gate does not know what any tool does: meaning comes from the tool's decision
function.

A caller whose call was not run is told why:

```text
tool `write_file` was not run: your approval policy is ReadOnly and it writes a file
tool `write_file` was not run: write_file requires an approved plan
```

The gate never stands in for a tool that did not decide. An answer without a
decision, with an unknown decision, or with `unsure` / `denied` and no reason
ends the call with an error, in every mode and under every policy:

```text
tool `read_file` could not be decided: its source answered without a decision
```

A call marked `workflow_node: true` was made by a node of a running workflow
(a script, a process, a worktree), not by a model. It has no agent and no
policy; the workflow was approved as a unit before it started, so the gate
only transports such a call: it goes straight to `<source>.tool.invoke`, and
neither the policy nor the mode is consulted. A call that carries both the
marker and a policy is an error, as is a marker that is not `true`.

One setting, on the command line:

- `--deny <name>` -- every call of the tool is denied, whatever the tool says

There is no `--auto`, `--prompt` or `--default`: what a call means is defined
by its tool, for every policy.

An advertised tool may carry `policy_descriptions`, an object from a policy
name to text the tool adds to its description for an agent with that policy.
The gate passes it to the public registry unchanged; whoever assembles an
agent's request appends it.

Runtime modes are `safe`, `auto`, and `yolo`. The starter's user-facing mode
behavior is documented in [the agent example permission guide](../../examples/nefor-agent/docs/permissions.md).

## Runtime events

- `tool-gate.set_mode` `{ mode }` switches the transport mode (`safe`, `auto`,
  or `yolo`).
- `tool-gate.mode_changed` announces the active mode after a change.
- `tool.prepared` is a source's answer to a prepare request.
- `tool.permission_response` answers a pending prompt with approval/rejection.
- `tool-gate.tool.cancel` terminally settles a call by its outer caller ID. A
  pending permission request is removed before stale approval can forward it;
  an active source correlation is removed before cancellation is forwarded, so
  late source results and duplicate cancellation are ignored.

The starter starts `tool-validator` before the gate; its actor forwards the
gate's question to the chat popup. It decides nothing.

## Tool display contract

Every advertised tool includes a declarative display contract beside its schema.
It declares an explicit lower-case, space-separated display label (literal or
selected argument), optional primary/argument fields, and whether successful
results render as content or a receipt. Labels are presentation metadata only:
wire names remain unchanged, punctuation is never auto-transformed, and a
consumer falls back to the wire name only when metadata is absent. The optional
`lifecycle = "delayed"` marker lets a surface suppress an invocation until its
source settles the grace decision. The gate rejects missing or malformed contracts. Ctrl+O expands tool receipts using this
semantic projection. Press Ctrl+R to toggle raw input/output for the latest
expanded tool receipt, or use `/raw <tool-call-id>` to toggle a specific entry.
Raw reveal is presentation-only and does not change persisted or model-facing
data.
