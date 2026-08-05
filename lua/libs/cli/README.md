# agentic-cli

Headless CLI plugin for nefor. Prompts enter through `agentic-loop`; output comes from conversation-manager's provider-neutral projection, the same source rendered by the TUI.

The composition must call `configure { readiness = ... }` with its required plugin names, complete required tool-name catalog, source-to-tool diagnostics, and optional timeout. Prompt submission is held until all required plugin liveness events and one complete `tool-gate` public catalog have been observed; timeout exits with the missing plugins, tools, and expected advertisement sources rather than hanging.

## Usage

```sh
# Single-shot prompt
nefor plugin agentic-cli "summarize this codebase"

# With file context
nefor plugin agentic-cli -f src/main.rs "explain this entry point"

# Switch model
nefor plugin agentic-cli -m claude-sonnet-4-20250514 "quick question"

# Interactive REPL (reads prompts from stdin until EOF)
nefor plugin agentic-cli

# JSON output (one line per turn)
nefor plugin agentic-cli --format json "what is 2+2"

# NCP wire format passthrough
nefor plugin agentic-cli --format stream-json "do something"
```

## Output formats

- **text** (default) — streams canonical conversation text deltas to stdout in real time; tool one-liners to stderr.
- **json** — single JSON line per turn on completion: `{ answer, tool_calls, duration_ms }`.
- **stream-json** — every `conversation.*` / `mag.*` / `tool.*` envelope as one JSON line on stdout (NCP wire format).

## Flags

| Flag                  | Description                                                       |
| --------------------- | ----------------------------------------------------------------- |
| `-m`, `--model MODEL` | Switch model on the active provider                               |
| `--format FMT`        | Output format: `text` / `json` / `stream-json`                    |
| `-f`, `--file PATH`   | Prepend file contents to the prompt                               |
| `--mode MODE`         | Set approval mode: `safe`, `auto`, or `yolo`                      |
| `--yolo`              | Alias for `--mode yolo`                                           |
| `-h`, `--help`        | Show help (pass `--` first: `nefor plugin agentic-cli -- --help`) |
