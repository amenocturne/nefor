# Starter TUI

> **Unreleased after v0.4.0.**

This guide describes the chat interface shipped in `starter/`. It is not a contract for every Nefor configuration: the TUI engine supplies widgets, input, rendering, selection, clipboard-image handling, and link activation, while the starter Lua composition chooses the commands, keys, workflow controls, session behavior, and permission policy documented here.

## The screen

The main pane is a Markdown transcript above a multiline prompt. The status line reports the active provider/model, reasoning effort, permission mode, context use, and runtime state. `Ctrl+B` opens a sidebar for live MAG workflows; `Tab` moves focus between the prompt and that sidebar.

Send with `Enter`; insert a newline with `Shift+Enter`. While a turn is running, another submission is kept as one queued message. Repeated submissions coalesce into that queued entry rather than becoming separate pending turns. See [commands and keys](commands-and-keys.md) for steering and interruption.

The transcript deliberately favors semantic summaries:

- tool calls render as compact **receipts** rather than raw payload dumps;
- reasoning and tool details are collapsed until `Ctrl+O`;
- `Ctrl+R` reveals raw data for the latest expanded tool receipt, and `/raw <tool-call-id>` targets one receipt;
- selecting transcript text with the mouse copies it to the clipboard and shows a toast.

## Paths, images, and links

Type `@path` anywhere in a message to include a text file. Completion is cwd-relative, one directory level at a time; `Tab` applies a candidate. Paths containing spaces are quoted by completion. At submission, readable text files are inlined for the agent, with content beyond 16 KiB marked as truncated. Missing paths remain literal. See [workflows and tools](workflows-and-tools.md#path-references).

Paste an image from the system clipboard with `Ctrl+V` or `Super+V`. The TUI saves it as a PNG and inserts its absolute path into the prompt. This gives the agent a path it can inspect with `read_image`; it does not itself attach image bytes to the user message. Text paste continues through the normal prompt path. Image interpretation still depends on a vision-capable provider/model.

**Unreleased:** rendered Markdown links with absolute `http`, `https`, or `mailto` targets open through the system handler when the same linked text receives an unmodified left-button press and release. Dragging still selects text. Relative links, fragments, `file:` links, and effectful schemes render without activation. This is an engine behavior newly present after v0.4.0, not a starter command.

## Sessions and context

Every real user submission is persisted to a JSONL session. `/new` (alias `/clear`) starts a fresh session and clears the visible transcript. `/resume` opens the recent-session picker; `/resume <id>` switches directly and replays the session in-process. See [sessions and context](sessions-and-context.md) before sharing a session directory between installations.

`/compact` asks the active provider/model to compact its model context. The full persisted transcript remains the fallback, and compaction does not mean the transcript was deleted or exported.

## Permissions

The starter begins in `safe` unless this invocation explicitly selects another startup mode. `/safe`, `/auto`, and `/yolo` change the live mode. `yolo` accepts broad risk; `auto` runs without interactive prompts and denies ordinary requests that still need a human, although the current starter auto-approves its separate write-review plan gate. Read [permissions](permissions.md): the UI presents three distinct approval systems, and their boundaries matter.

## More

- [Commands and keys](commands-and-keys.md) — complete starter command and key reference
- [Sessions and context](sessions-and-context.md) — persistence, replay, resume, and compaction
- [Workflows and tools](workflows-and-tools.md) — sidebar, termination, paths, images, receipts, and links
- [Permissions](permissions.md) — safe/auto/yolo and all approval paths
