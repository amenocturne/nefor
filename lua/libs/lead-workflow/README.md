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
dynamic workflows included. What a kind of node means is not decided here:
each kernel factory declares it, and the MAG runtime publishes the
declarations as `factory_approvals`.

- a script or process node means what the `shell.script` or `process.exec`
  tool says its command means under the caller's policy (the configured
  command classification; a process's argument vector is classified as the
  equivalent quoted shell command);
- a worktree node means what its tool says;
- a param bound to a run-time value means what the node's factory says: for a
  command's text, unsure;
- an agent node whose approval policy is wider than the caller's is a denial;
  the agent is not silently narrowed;
- routing, typing, model, waiting and human-approval nodes are declared safe
  by their factories.

A kind of node the runtime did not describe, a capability no tool defines, a
classifier that fails, or an agent node without a policy cannot be assessed.
That is an error for the call, naming every such node, never a result:

```text
mag-apply: the workflow could not be assessed:
- node `x`: the MAG runtime does not say what a `nefor.factory.teleport` node means for approval
```

Denied dominates unsure, which dominates safe. The gate's decision table turns
the result into run, ask or deny, so a `ReadOnly` agent's workflow that is not
entirely safe is refused in every permission mode, with every offending node
listed:

```text
tool `mag-apply` was not run: your approval policy is ReadOnly and the workflow declares 2 nodes not shown to be allowed under the ReadOnly approval policy:
- node `build` (shell script) `make all`: not shown to be read-only
- node `writer` (agent) with approval policy Default: its approval policy is wider than that of the agent applying the workflow (ReadOnly)
```

Once a workflow runs, its own nodes are not asked about again. The
composition supplies the capability meanings with
`configure_approval { decide_capability = require("libs.tool").decision { sources } }`;
without it a workflow with a capability node cannot be assessed.

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

## Workspaces

Every calling node has its own writable MAG workspace inside its run's folder.
The source tools operate on the caller's workspace only, so one caller cannot
read, overwrite, or run another's source through them.

```text
<sessions root>/<session id>/mag/
  runs/<run id>/<actor>.output        node outputs, written by the runtime
  runs/<run id>/<node>/.../<node>/    one workspace per calling node
  lead/                               the lead's workspace (exception, see below)
```

A workspace is identified by the run and the caller's logical node path, the
same nesting the TUI sidebar shows for that run. Nothing in the layout or in
the resolution depends on what kind of node the caller is.

The caller is identified by the invocation provenance the kernel stamps on the
tool call (`session_id`, `run_id`, `actor_id`), never by anything the model
supplies:

- The top-level key of the path comes from one function of the invocation,
  `workspace_key` in `init.lua`. Today it is the invocation's `run_id`.
- The node path comes from `libs.mag-run-projection`, which the sidebar also
  uses, applied to the logical nodes the run declared (the compiled program's,
  then each materialized template's through `mag.nodes_declared`). The
  workspace belongs to the node that directly contains the calling actor's own
  row: `enclosing_path` takes the path of the node listing the actor as a
  member and drops its last segment. A calling actor whose node is top-level
  has no containing node, so its own node is its workspace.
- A call whose run this library does not track, or whose actor has no node in
  that run, is rejected rather than given a guessed folder.

A node that applies a workflow starts a new run; that run's nodes get folders
under the new run id, and the applying node stays under its own. A node
expanded from a template gets its folder from its materialized path, for
example `runs/<run id>/fan-out/items%3A0/worker/`.

The lead is the one exception, and the only caller-specific branch
(`caller_workspace` in `init.lua`, marked `TODO`). The agentic loop starts the
lead's turn as a new short-lived run per user message, so the general rule
would lose its files between turns; it keeps `lead/` for the session. The
intended redesign makes the lead an ordinary long-lived workflow with a stable
run identity, which removes the exception and the hardcoded folder name.

A run's folder holds both what the runtime wrote and what the run's nodes
authored, so names are chosen to keep them apart:

- A node name becomes one directory name. Lowercase letters, digits, `_`,
  `-`, and a non-leading `.` are kept; every other byte, including uppercase
  letters and `%` itself, is written as `%XX`. Two different names therefore
  never share a directory, also on a filesystem that ignores case or
  normalizes Unicode. A name longer than 255 bytes after encoding is rejected.
- The runtime's files in a run folder are all named `<actor>.output`. The dot
  of a node name ending in `.output` is written `%2E` (`task.output` becomes
  `task%2Eoutput`), so no directory can take an output file's name.
- The run id is used as given, because the runtime writes into the same
  folder under that id. A run id that is not already a plain folder name by
  the rule above, or that starts or ends with `-`, is rejected.

A workspace is created by its first write, together with a minimal `mag.toml`
that makes it a MAG project. Reading from a workspace that was never written
reports an ordinary missing file and creates nothing. Each model actor's
prompt names its node's directory in its `# MAG workspace` section. A template
actor's prompt is composed before its node path exists, so it names the run's
folder and says its own folder is at its node path beneath it.

Paths given to the source tools are normalized and relative to the caller's
workspace; absolute paths, `..` traversal, and symlinks are rejected, including
a symlinked run or node folder above the workspace.

Imports resolve through the configured module roots plus the caller's own
workspace, which is also the project root of the compilation. Modules written
into a workspace are importable by programs in that workspace only: a program
cannot import a module that exists only in another workspace. Code several
callers need belongs in a configured package root. Canonical and
configuration-owned libraries stay in their package roots; the composition
passes those roots to every compilation and nothing is copied into the
session. Files loaded with `read(...)` or `read_json(...)` are snapshotted on
first access within one compilation, so compile again after changing them.

Workspaces nest as their nodes do. If two calling actors' containing nodes are
an ancestor and a descendant of each other, the descendant's workspace lies
inside the ancestor's, and the ancestor can reach it by relative path. Nefor's
own node constructors give calling actors sibling containing nodes, so this
does not occur with them.

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
