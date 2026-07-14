# git-worktree

Stateless NCP capability provider behind MAG's `nefor.worktree` library.

It advertises two distinct operations through the tool gate:

- `git_worktree_create` creates a new branch and worktree. Existing paths or local branches are errors; creation never adopts.
- `git_worktree_open` validates an existing repository/path/branch triple without changing it. Opening never creates.

Both operations require explicit absolute repository and worktree paths. Successful worktrees outlive the plugin request and the MAG run; this plugin has no removal, merge, inventory, or cleanup surface.
