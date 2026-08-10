# Workflows and tools

> Starter behavior, Unreleased after v0.4.0 (HEAD `505a764`). MAG/kernel lifecycle and capability enforcement are runtime mechanisms; their presentation and keyboard controls here belong to the starter chat composition.

## Queueing and steering

The lead turn is a short-lived MAG program. If you submit while it is active, the starter keeps one optimistic queued entry; later submissions coalesce into it. A single `Esc`, after its 600 ms gesture window expires, claims that queue and sends `chat.steer`. The agentic loop injects it at the lead LLM's next provider boundary, after the current exchange.

Double `Esc` hard-stops the active lead, drops the engine-side queue, and restores the queued text to the prompt so it is not silently lost. Triple `Esc` additionally requests termination of every MAG workflow. See [Escape precedence](commands-and-keys.md#escape-precedence): a popup, completion, history recall, or focused sidebar consumes Esc first.

## Sidebar and run inspection

`Ctrl+B` toggles the sidebar. `Tab` focuses it when it has live rows or the bounded recent-completion inspection target.

Runs are keyed by run ID and can overlap. A run contains a header, groups, and actor rows with pending/running/idle/completed/failed/killed states and elapsed/activity information. Completed runs remain visible briefly (about two seconds); after that, the newest expired completion remains as one bounded inspection target rather than an unbounded run history.

With sidebar focus:

- `Enter` folds/unfolds a group;
- `Space` opens a read-only chronological inspector for an actor, a merged group, or the whole run;
- `Ctrl+O` in the inspector reveals additional reasoning/tool payload detail;
- scroll with arrows, page keys, `Home`, and `End`; `Esc`/`Q` closes;
- `x` asks to terminate the selected active run; `X` asks to terminate every run, including lead; both require confirmation.

Terminating a lead row uses the lead hard-stop path and restores queued input. Terminating another run sends a scoped `mag.kill_run`. Global termination sends both the starter workflow request and `mag.kill_all_runs`. These controls request cooperative runtime termination; the UI distinguishes killed from failed but does not promise transactional rollback of effects already performed.

Historical run panels are not reconstructed on `/resume`; transcript results are. The sidebar is for live and just-finished observation, not retained workflow export.

## Path references

Type `@<path>` in a message. Completion:

- resolves relative paths from the process working directory and accepts absolute paths;
- walks one directory at a time rather than recursively searching;
- filters the current leaf by case-insensitive prefix;
- orders directories first;
- excludes dotfiles and `.git`, `node_modules`, `target`, and `__pycache__`;
- caps a directory listing at 200 entries;
- adds `/` to directory candidates and quotes completed paths containing spaces.

On submission, readable **text** paths are replaced by a `<file path="…">` fenced block before the message reaches the lead. At most 16 KiB is inlined; larger files carry a truncation marker directing the agent to `read_file`. Common trailing prompt punctuation is kept outside the path. Quoted `@"path with spaces"` and `@'path with spaces'` forms work. Missing/unreadable paths remain literal without a TUI error.

Audio extensions (`mp3`, `wav`, `flac`, `aif`, `aiff`, `m4a`) deliberately remain as path references. Binary files are not a general attachment mechanism.

## Images

`Ctrl+V` or `Super+V` asks the TUI engine whether the system clipboard contains an image. If so, it writes a PNG beneath `$NEFOR_DATA_DIR/clipboard-images/` (or a temporary Nefor directory if no data dir is available) and inserts the absolute path. If no image is available, ordinary text paste behavior continues.

An absolute image path beginning with `/` is treated as plain chat text rather than an unknown slash command. Image paths are not inlined by the text `@path` preprocessor. The lead can call `read_image`, which accepts PNG, JPEG, GIF, and WebP by file content. The tool has a 50 MiB source cap and targets a 5 MiB provider payload, downscaling/re-encoding oversized images. It does not OCR or describe the image; a provider/model must support image input or return an explicit limitation.

The saved clipboard PNG's existence is current behavior, not a documented retention or attachment-export guarantee. Manage sensitive clipboard images accordingly.

## Tool receipts

The transcript renders registered tools through semantic display contracts: a compact action/summary and a settled result/error rather than dumping raw inputs and outputs by default. This keeps base64 media and large outputs out of the normal transcript.

- `Ctrl+O` expands/collapses tool calls and reasoning globally.
- `Ctrl+R`, while expanded, toggles raw data for the latest tool call.
- `/raw <tool-call-id>` targets a specific receipt and turns details on.

A receipt is an observability view, not proof that every external side effect completed atomically. The tool result and terminal run status are the available completion evidence. Raw display can contain sensitive arguments or output.

## Markdown links — Unreleased

After v0.4.0, text rendered by the engine's Markdown widget carries safe link targets through wrapping and layout. An unmodified left-button press and release on the same linked text opens absolute `http`, `https`, or `mailto` URLs with the system handler. A drag cancels activation so selection remains usable. Opener failure is logged rather than turned into a chat popup.

Relative links, `#fragments`, `file:` URLs, and schemes such as `javascript:` are deliberately non-activating. The engine guarantee is limited to recognized Markdown link cells and the allowed schemes; the starter does not add a URL command, browsing sandbox, confirmation screen, or link-history export.
