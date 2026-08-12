# Sessions and context

## What a session is

The starter opens a session at startup. It delays creating a JSONL file until the first real user submission, and empty startup-only sessions are omitted/pruned. The resolved session root is:

1. `NEFOR_SESSIONS_DIR`, when set;
2. otherwise `$NEFOR_DATA_DIR/sessions` (with Nefor's normal data-root default).

A persisted session can include:

```text
<session-root>/<id>.jsonl
<session-root>/<id>/mag/
<session-root>/<id>/metadata.json
```

The JSONL is an append-only bus-event record used to rebuild runtime projections. It is not documented as a stable export format. Do not treat it as a transcript API or promise that every internal event will remain readable by future minor versions.

Submitted prompt history is separate: the starter keeps at most 50 newest submitted prompt values in `<data-root>/input-history` for `Up`/`Down` recall. That convenience history is not the session transcript.

## New and resume

- `/new` and its `/clear` alias first request interruption plus a replacement session. The old session remains authoritative until the sessions actor acknowledges the new ID; only then does the TUI replace session-scoped transcript, conversation, queue, compaction, popup, and workflow/sidebar projections. If acquisition fails, the current session is retained and the failure is shown.
- `/resume` lists recent non-empty sessions newest-first, showing the header timestamp and first submitted prompt.
- `/resume <id>` switches directly. The parser uses the first word-like/hyphenated ID from the argument.
- `nefor run --session <id>` starts on a saved session.

Resume occurs in the running process: the example acquires the target, acknowledges the replacement, opens it, ends the old session, emits a new session lifecycle, and replays valid recorded engine-origin entries in bounded cooperative chunks. The TUI replaces the old transcript only after that acknowledgement, shows byte-based loading progress, suppresses historical run panels/popups, and reveals the rebuilt transcript when replay completes. Live work should not be inferred from historical MAG lifecycle events; the sidebar is a live observation surface.

The session log is the replay authority, and conversation-manager is the canonical consumer for recorded conversation facts, context, and provider-neutral transcript projection. Agentic-loop uses that state to orchestrate new turns; it does not own a second replay history. Replay reconstructs state but never re-runs recorded tools, provider calls, or MAG workflows.

A session replacement resets session-scoped semantic and UI state, not every Lua value in the process. Provider catalogs, authentication/runtime registrations, installation configuration, and other process-level actors can survive `/new`, `/clear`, or `/resume` and then publish fresh live state into the new session. Restart Nefor when a full process reset is required.

Malformed or non-replayable rows are skipped rather than converted into promises about recovery. A failed target open does not intentionally abandon the current session, but command-level failures are primarily logged and are not a migration facility.

## One writer, shared roots, and provenance

A session should have **one active writer**. Stable and edge installations may share `NEFOR_SESSIONS_DIR`, but the current implementation adds no synchronization for opening the same session in two processes. Concurrent writers can corrupt assumptions even if both processes appear healthy.

Installed distributions record an opaque installation generation in `metadata.json` as `created_with` plus ordered `installation_history`. This records custody, not compatibility. Successful resume appends the active generation after opening; failed resume does not.

Nefor's pre-public compatibility policy guarantees compatibility only within one minor line (`0.y.x` with `0.y.0`). Across minor lines, session wire/layout compatibility is not guaranteed; an old session failing to resume after `0.y → 0.y+1` is acceptable. There is no promised automatic migration, downgrade path, cross-minor repair, retention period, cloud sync, or export mechanism.

## Replay is reconstruction, not re-execution

During replay, the sessions actor republishes recorded traffic inside replay windows while suppressing persistence of replay-derived traffic. Consumers rebuild their own projections. In particular:

- transcript/conversation state is reconstructed from canonical facts;
- historical permission popups and live run panels are not reopened;
- pending plan approval is not restored;
- resolved permission responses in the log do not become fresh approval prompts;
- live provider/model catalogs can supersede older replayed catalogs.

The full session log remains on disk unless external/user action removes it; `/new`, `/resume`, and `/compact` do not implement export or retention management.

## Model context and compaction

The visible transcript, persisted session events, and provider model context are related but not identical.

`/compact` requests native compaction for the active conversation and records a pending then complete/failed semantic entry in the transcript. Compaction materializes model-specific context through the active provider/model and restores that opaque artifact into later ephemeral MAG chats when compatible. The canonical full transcript remains the fallback, including when provider/model changes make the artifact unsuitable.

Therefore:

- compaction is not transcript deletion;
- it is not guaranteed lossless summarization or portable export;
- the opaque artifact is not promised to migrate between providers/models or minor versions;
- a failed compaction leaves the full transcript as the usable basis rather than establishing a new compacted context.

See [commands and keys](../chat/slash.lua) for `/compact` and `/resume`, and [permissions](permissions.md) for why approvals do not survive session boundaries.
