# Session provenance

Installed distributions launch Nefor with a full immutable generation ID in `NEFOR_INSTALLATION_ID`. Nefor records that opaque ID rather than deriving it from mutable source state.

`NEFOR_SESSIONS_DIR` is the explicit session directory. When it is unset, Nefor keeps the existing default of `$NEFOR_DATA_DIR/sessions`. This lets installed stable and edge launchers share only sessions while their data roots—and therefore locks, caches, logs, plugin state, and other writable data—remain isolated.

Each session stores:

```text
<session-dir>/<id>.jsonl
<session-dir>/<id>/mag/
<session-dir>/<id>/metadata.json
```

```json
{
  "created_with": "<full generation id>",
  "installation_history": ["<full generation id>"]
}
```

Creation records `created_with` and the first ordered history entry. A successful resume appends the active generation only after the session opens successfully; a failed open does not change provenance or abandon the current session. The history records custody, not compatibility.

The stable and edge feature adds no synchronization for simultaneously opening one session in multiple processes. A session should have one active writer.
