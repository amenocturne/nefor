# git-worktree

Stateless NCP capability provider behind MAG's `nefor.worktree` library.

It executes two distinct capabilities:

- `git_worktree_create` creates a new branch and worktree. Existing paths or local branches are errors; creation never adopts.
- `git_worktree_open` validates an existing repository/path/branch triple without changing it. Opening never creates.

Both operations require explicit absolute repository and worktree paths. Successful worktrees outlive the plugin request and the MAG run; this plugin has no removal, merge, inventory, or cleanup surface.

## Wire contract

| Event                                                | Direction             |
| ---------------------------------------------------- | --------------------- |
| `git-worktree.capability.invoke { id, capability, args }` | caller → git-worktree |
| `capability.result { id, output }`                   | git-worktree → caller |

`output` is `{ ok: true, worktree }` or `{ ok: false, error: { operation, kind, message } }`.

The plugin defines no model-facing tools. The `git_worktree_create` and
`git_worktree_open` tools — descriptions, parameter schemas, display contracts,
argument validation, and tool-gate advertisement — are Lua definitions in
[`lua/libs/git-worktree`](../../lua/libs/git-worktree/init.lua), built with
[`libs.tool`](../../lua/libs/tool/init.lua) and wrapped around this process by
`libs.compositors.tools.git_worktree_actor_spec`.

## Output and teardown failures

Wire output is checked before enqueue; enqueue success is not proof of delivery.
If a result exceeds the transport frame limit, the plugin substitutes one bounded
`capability.result` with the actual invocation `id` and an `error` explaining output
admission failure. Git effects may already exist: this is not a rejected request
or a rollback, and created worktrees/branches are not removed. A later independent
invocation can proceed when the writer remains available.

Ready, hello and goodbye are required uncorrelated output. Their
admission failure, a closed writer, terminal stdin framing/I/O failure or an
undeliverable fallback ends the plugin with a transport error. Bounded malformed
JSON remains parse noise. Detached invocation errors reach the plugin supervisor;
trusted task panics are not converted to successful tool outcomes.

Shutdown and transport teardown retain outstanding Git task ownership until each
blocking operation finishes. Dropping an async waiter cannot cancel a running
Git operation. There is no retry or deletion during teardown; downstream consumers
must treat accepted effects as unknown if transport disappears before a result.
The engine's process termination policy may still interrupt this local drain.
