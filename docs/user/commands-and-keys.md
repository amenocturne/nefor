# Commands and keys

> Starter behavior, Unreleased after v0.4.0 (HEAD `505a764`). Commands may be extended or replaced by another configuration.

## Slash commands

Command-name completion is case-insensitive prefix matching over names and aliases. Type `/` at the start of the prompt, use `Up`/`Down`, and press `Tab` to complete or `Enter` to run the highlighted match. Unknown slash commands are emitted as `chat.command` for config extensions; the base starter does not promise a visible result.

| Command                | Aliases           | Behavior                                                                                                                                                                         |
| ---------------------- | ----------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `/new`                 | `/clear`          | Interrupt all workflows, create a fresh session, and clear transcript/run UI state.                                                                                              |
| `/help`                | —                 | Open the starter help popup. `?` does the same when the prompt is empty.                                                                                                         |
| `/quit`                | `/exit`           | Interrupt workflows and exit.                                                                                                                                                    |
| `/login [provider]`    | —                 | Authenticate a provider. Without an argument, open the provider picker. Only providers advertising login support participate.                                                    |
| `/logout [provider]`   | —                 | Revoke provider authentication. Without an argument, pick from connected providers that support logout.                                                                          |
| `/model [model]`       | —                 | Without an argument, load model lists and open a searchable provider/model picker. With a model, request it on the first connected provider in sorted order.                     |
| `/usage`               | —                 | Show quota/reset information for the active provider when it advertises usage support.                                                                                           |
| `/mode [default]`      | —                 | With no argument, complete the available workflow mode. `/mode default` (also accepts `/mode normal`) interrupts workflows and starts a fresh session using configured defaults. |
| `/think <level>`       | `/effort <level>` | Set reasoning effort. The starter usage contract is `low`, `medium`, `high`, or `xhigh`; provider support is authoritative.                                                      |
| `/compact`             | —                 | Request compaction of the active model context and show a pending/completed/failed transcript entry.                                                                             |
| `/resume [session-id]` | —                 | Without an argument, open the recent-session picker. With an ID, switch and replay that session in-process.                                                                      |
| `/safe`                | —                 | Select interactive permission mode.                                                                                                                                              |
| `/auto`                | —                 | Select non-interactive permission mode. See the write-review exception in [permissions](permissions.md).                                                                         |
| `/yolo`                | —                 | Approve gated actions broadly. Dangerous. Capability allowlists still fail closed.                                                                                               |
| `/approve [note]`      | —                 | Approve the pending write-review plan. Only meaningful while a plan is pending.                                                                                                  |
| `/reject [reason]`     | —                 | Reject the pending write-review plan. Only meaningful while a plan is pending.                                                                                                   |
| `/raw <tool-call-id>`  | —                 | Toggle raw data for one tool receipt and expand details.                                                                                                                         |

While a write-review plan is pending, **any submitted text** is routed to review rather than starting a normal turn. `/approve` and `/reject` are verdicts; any other text discards the pending plan and becomes review feedback.

## Global and prompt keys

| Key                 | Behavior                                                                                                                  |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `Enter`             | Submit the prompt; in a completion popup, run/select the highlighted item.                                                |
| `Shift+Enter`       | Insert a newline.                                                                                                         |
| `Tab` / `Shift+Tab` | Apply/navigate completion when one is open; otherwise cycle prompt/sidebar focus if the sidebar is visible and non-empty. |
| `Ctrl+B`            | Show or hide the workflow sidebar. Hiding a focused sidebar returns focus to the prompt.                                  |
| `Ctrl+O`            | Expand/collapse reasoning and tool details, including inside a node inspector.                                            |
| `Ctrl+R`            | Toggle raw data for the latest tool call, but only while details are expanded.                                            |
| `?`                 | Open help only when the prompt is empty.                                                                                  |
| `Up` / `Down`       | Let prompt history/completion consume the key when applicable; otherwise scroll the transcript one line.                  |
| `PgUp` / `PgDn`     | Scroll the active popup, or the transcript, by ten lines.                                                                 |
| `Home` / `End`      | Jump the active popup/transcript to top/bottom.                                                                           |
| `Ctrl+C` twice      | Exit when both presses occur within 600 ms. The first press warns/arms; another user action resets it.                    |
| `Ctrl+D`            | Exit immediately.                                                                                                         |
| Mouse drag          | Select text; release copies a non-empty selection to the clipboard.                                                       |
| Mouse wheel         | Scroll the scrollable region under the pointer.                                                                           |

## Escape precedence

`Esc` is contextual before it is a workflow gesture. In order, it:

1. dismisses an info/warning/error popup;
2. denies a tool or MAG approval popup, or closes another popup/toasts;
3. leaves sidebar focus and returns to the prompt;
4. closes slash or `@path` completion;
5. cancels prompt-history navigation and clears the recalled value;
6. only then participates in workflow control.

Workflow Esc presses use a 600 ms window per press:

- **single `Esc`:** if a queued message exists and no second press arrives, steer it into the active lead at the next provider boundary, after the current LLM exchange;
- **double `Esc`:** hard-stop the active lead and restore queued text to the prompt;
- **triple `Esc`:** hard-stop the lead and kill every active MAG workflow.

A lone `Esc` with no queued message merely expires. Because higher-priority UI state consumes Esc, close the popup/completion or leave the sidebar first; a subsequent Esc sequence controls workflows.

## Popup keys

- **Permission:** `A` or `Enter` approves; `D` or `Esc` denies.
- **Terminate confirmation:** `Enter`/`Y` confirms; `Esc`/`N`/`Q` cancels.
- **Session/login pickers:** `Up`/`Down`, `Enter`, `Esc`.
- **Model picker:** type to filter, `Up`/`Down`, `Enter`, `Esc`.
- **Message/help/node inspector:** `Esc` or `Q` closes (`Enter` also closes ordinary message popups). Scroll keys follow the table above.

## Sidebar keys

With sidebar focus:

| Key             | Behavior                                                                                                                                         |
| --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| `Up` / `Down`   | Move one visible row.                                                                                                                            |
| `PgUp` / `PgDn` | Move ten rows.                                                                                                                                   |
| `Home` / `End`  | First/last row.                                                                                                                                  |
| `Enter`         | Fold/unfold a group row; no effect on run headers or actor rows.                                                                                 |
| `Space`         | Open a read-only inspector for the selected actor, merged group, or whole run. If only the bounded recent-completion target remains, inspect it. |
| `x`             | Ask to terminate the selected active run.                                                                                                        |
| `X`             | Ask to terminate all active workflows, including the lead.                                                                                       |
| `Esc`           | Return focus to the prompt; it does not interrupt the turn.                                                                                      |
