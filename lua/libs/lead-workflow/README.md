# lead-workflow

The tools through which agents run MAG workflows in a Nefor session, and the
lead's plan-review tool. MAG itself only compiles programs to artifacts; this
library writes session source, compiles it through the MAG plugin, dispatches
the result to the kernel, and tracks the runs it starts.

## Running MAG code

`mag-apply {file, content?}` is the direct path. With `content`, it writes the
file (replacing existing contents), validates and compiles it, and runs the
workflow in one call. With `file` only, it runs the workflow already in that
file. If writing, validation, or compilation fails, nothing runs and the error
is returned; the file keeps what was written so it can be fixed with a
selective edit and run again. A single command is an ordinary workflow: a
program with one `script` node.

The remaining source tools are single steps of that path:

- `mag-write-file {file, new_string, old_string?}` writes a source file, or
  replaces one exact unique occurrence of `old_string`, without running it.
- `mag-preview {file}` validates and compiles a source file without running it
  and returns the complete node tree the workflow would run.

Every call to `mag-apply` starts a new run. Editing source describes a
different future run; it never changes a running one.

The tools are built with [`libs.tool`](../tool/init.lua), so their schemas are
closed: an undeclared argument such as `run_id` on `mag-apply` is rejected
with the same message format every tool uses, before the tool runs.

## Approval: the workflow is the unit

Every `mag-apply` goes through the tool gate like any tool call, in three
steps: `prepare` writes `content` to the file (or checks that the file
exists), the decision function compiles it and assesses the compiled workflow
under the calling agent's approval policy, and `execute` runs exactly the
artifact that was assessed. A file written by `prepare` stays when the call is
then refused, so it can be fixed and applied again. A compile error ends the
call with the compiler's message; there is nothing to decide.

The assessment is a fold over every node the kernel would spawn
(`libs.mag-workspace`'s `fold_nodes(decoded, initial, f)`), templates of
dynamic workflows included:

- a script or process node means what the `shell.script` or `process.exec`
  tool says its command means under the caller's policy (the configured
  command classification; a process's argument vector is classified as the
  equivalent quoted shell command);
- a worktree node means what its tool says;
- a command whose text is bound to a run-time value is unsure;
- an agent node whose approval policy is wider than the caller's is a denial;
  the agent is not silently narrowed;
- routing, typing, waiting and human-approval nodes change nothing; a kind of
  node nobody has described is unsure.

Denied dominates unsure, which dominates safe. The gate's decision table turns
the result into run, ask or deny, so a `ReadOnly` agent's workflow that is not
entirely safe is refused in every permission mode, with every offending node
listed:

```text
tool `mag-apply` was not run: your approval policy is ReadOnly and the workflow declares 2 nodes not shown to be allowed under the ReadOnly approval policy:
- node `build` (shell script) `make all`: not shown to be read-only
- node `writer` (agent) with approval policy Default: wider than ReadOnly, the approval policy of the agent applying the workflow
```

Once a workflow runs, its own nodes are not asked about again. The
composition supplies the capability meanings with
`configure_approval { decide_capability = require("libs.tool").decision { sources } }`;
without it no command is vouched for and every command node is unsure.

The plan-review rule below is separate and unchanged: in `safe` mode a
workflow with write-capable agents also needs an approved plan.

## Runs

A run that finishes within the completion grace period
(`SYNC_COMPLETION_GRACE_MS`, currently 3 seconds) returns its output, or its
failure with the workflow tree, directly from `mag-apply`. A longer run returns
a `run_id`, the authored node tree, and how its result will reach the caller:

- The root lead receives the run's completion as a notification and ends its
  turn while it waits.
- A delegated agent calls `mag-await {run_id}` when its next step depends on
  the result.

Run handles are session-scoped. The root lead can address any run in its
session; a delegated agent can await or control only runs it started itself.
Recent terminal outcomes are retained for a bounded window, so an old handle
can expire.

- `mag-status {run_id?}` returns the current state of active and recently
  completed runs, or of one run, without waiting.
- `mag-await {run_id}` waits for one run's result. A run finishes only when all
  its processes exit, so awaiting a run that never ends, such as a server,
  never returns. Cancelling the wait does not stop the run.
Script and process nodes run without a wall-clock bound when their timeout is
`Timeout.Unlimited`; give a bound with `Timeout.Milliseconds(n)`,
`Timeout.Seconds(n)`, or `Timeout.Minutes(n)` when an operation needs one.

A `run_id` that cannot be used reports why: malformed (not a run handle),
unknown (no retained run), wrong session, expired (its retained outcome aged
out), or unauthorized (a delegated agent addressing a run it did not start).

- `mag-terminate {run_id}` stops one active run and returns once the run
  confirms it has stopped (`TERMINATION_CONFIRM_TIMEOUT_MS`, currently 30
  seconds, bounds that wait). The caller does not also receive that run's
  completion notification.

## Session workspace

Each session gets a writable MAG workspace under its session data, created by
`libs.mag-workspace`. Session-local source modules may live directly in it.
Canonical and configuration-owned libraries stay in their package roots; the
composition passes those roots to every compilation and nothing is copied into
the session.

Paths given to the source tools are normalized and relative to the workspace;
absolute paths, `..` traversal, and symlinks are rejected. Imports resolve
through the configured module roots. Files loaded with `read(...)` or
`read_json(...)` are snapshotted on first access within one compilation, so
compile again after changing them.

## Plan review

`write-review {plan}` submits a plan for the user's review. The gate mode
decides what happens:

- `safe` (the default): the call blocks until the user responds. `/approve`
  authorizes the plan for the current turn, `/reject` returns a reason, and any
  other reply discards the pending review and becomes new input.
- `auto` and `yolo`: the plan is approved immediately.

In `safe` mode, `mag-apply` rejects a program containing write-capable agents
until a plan is approved. Approval is turn- and session-scoped: new ordinary
user input invalidates it. Approving a plan does not bypass per-tool risk
policy inside agents.

`nefor.human.approval_gate` is a different mechanism: a node inside a graph
whose human decision is part of the workflow's meaning.

## Files

- `init.lua` — tool definitions and their functions, workflow approval, MAG load/apply coordination,
  run control and result delivery.
- `run-registry.lua` — ownership and waiter registry for detached runs.
- `role.lua` — loads the config-owned lead and worker system prompts.
