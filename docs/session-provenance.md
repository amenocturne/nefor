# Session provenance and distribution copies

Installed distributions must launch Nefor with a full, immutable generation ID using either `--installation-id <id>` or `NEFOR_INSTALLATION_ID=<id>`. The launcher/installer owns the ID; Nefor treats it as opaque and records it rather than deriving it from mutable source state.

Each session keeps its event log at `<data-root>/sessions/<id>.jsonl`, its MAG tree at `<data-root>/sessions/<id>/mag`, and flat provenance metadata at `<data-root>/sessions/<id>/metadata.json`:

```json
{
  "created_with": "<full generation id>",
  "installation_history": ["<full generation id>"]
}
```

A successful create records `created_with` and the first history entry. Every successful resume appends the active generation ID exactly once. Opening is serialized between processes; validation and metadata updates happen before Lua changes the active session, so failed opens do not append history or abandon the current session. The history records custody only. It is not a compatibility assertion.

## Copying sessions between distributions

Use Nefor's non-destructive command against explicit data roots:

```sh
nefor copy-sessions \
  --source ~/.local/share/nefor-edge \
  --destination ~/.local/share/nefor \
  --installation-id '<destination full generation id>' \
  <session-id>...
```

Omit session IDs to copy every provenance-aware source session. Source JSONL/event bytes and the complete session tree, including MAG artifacts, are copied without interpretation. File and directory permission modes and symlinks are preserved, including the source metadata file's mode after destination provenance is appended. The destination generation ID is appended to copied metadata, but Nefor makes no claim that the destination can understand the payload. Any destination collision aborts that session rather than overwriting it. Per-session interprocess locks and same-filesystem staging/rename prevent readers from observing partial session metadata or trees.
