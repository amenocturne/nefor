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
