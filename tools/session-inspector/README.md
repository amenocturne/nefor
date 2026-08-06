# Nefor session inspector

Local structural explorer for persisted Nefor session logs. It lists the
sessions in the resolved Nefor data directory, streams the selected JSONL file,
and reports byte weight by semantic family, event kind, and top-level body
field. Exact hashes measure mirrored manager/provider message content without
retaining the content itself.

Run it from the repository root:

```sh
just inspect-sessions
```

Then open `http://127.0.0.1:3939`. Override the data directory with
`NEFOR_DATA_DIR`, exactly as for Nefor itself. The host and port can be supplied
as recipe arguments:

```sh
just inspect-sessions 127.0.0.1 4040
```

Summaries are cached under `$XDG_CACHE_HOME/nefor/session-inspector`, falling
back to `~/.cache/nefor/session-inspector`. A cache entry is reused only while
the source file's byte length and modification time match. The server binds to
localhost by default and serves no raw event or message endpoint.

Run the focused tests with `just test-session-inspector`.
