# Behavioral Reminders

Mid-conversation nudges sent via `pi.sendMessage()` when the model drifts from instructions. Configured in `extensions/behavioral-reminders/reminders.yaml`.

## All 8 Reminders

| Name | Trigger | Message | Cooldown | Max/Session |
|------|---------|---------|----------|-------------|
| `exploration_spiral` | 5+ consecutive read-only tool calls | "Break the exploration spiral. You have enough context -- propose a plan or ask a clarifying question." | 10 | 3 |
| `write_after_plan_only` | Write/edit tool while plan mode is active | "STOP. The user asked for a plan only. Do not modify files." | 1 | 5 |
| `verbose_output` | Response exceeds ~2000 tokens (~8000 chars) | "Be concise. Your response is too long. Summarize key points." | 5 | 3 |
| `repeated_tool_call` | Same tool + same arguments 3 times | "You're in a loop -- same tool with same arguments 3 times. Step back." | 5 | 3 |
| `premature_summary` | Model outputs 100+ chars of text while bg tasks are running | "Background tasks are still running. Wait for all completion messages." | 3 | 5 |
| `multi_tool_attempt` | 2+ tool calls in the same turn | "ONE tool per message. Pick the most important one. Call it alone." | 1 | 10 |
| `content_echoing` | 500+ chars of model output overlap with last read file content | "Do not echo file contents. Summarize what you found in 1-3 lines." | 5 | 3 |
| `self_contradiction` | 3+ deliberation phrases in one response | "Stop deliberating. Pick the best option and commit to it." | 3 | 3 |

## Detection Details

### exploration_spiral

Tracks `consecutiveReads` -- incremented for read, grep, find, ls; reset to 0 on any write/edit/bash/bg-run call. Fires when the counter reaches the threshold (default 5).

This catches the pattern where Qwen reads file after file without proposing anything. The reminder forces it to stop exploring and state what it knows.

### write_after_plan_only

**Plan mode detection** happens on the `input` event (user messages):
- **Activated by**: "just plan", "plan only", "don't implement", "do not implement", "only plan"
- **Deactivated by**: "go ahead", "proceed", "implement it", "implement this"

When plan mode is active, any call to `write` or `edit` tools triggers this reminder. Cooldown of 1 means it fires on every violation -- this is the strictest reminder because the user explicitly asked for plan-only.

### verbose_output

Tracks `turnTextBuffer` via the `message_update` event (streaming model output). Uses a character-based proxy: threshold of 2000 tokens x 4 chars/token = 8000 characters.

Fires when the buffer exceeds the threshold in a single turn. The per-turn buffer is reset on `turn_end`.

### repeated_tool_call

Maintains a ring buffer of the last 10 tool calls as `{tool, argsHash}` pairs. On each tool call, checks if the last N entries (default 3) are all identical (same tool name and same JSON-serialized arguments).

The `argsHash` is a deterministic JSON serialization with sorted keys, so `{path: "a", pattern: "b"}` and `{pattern: "b", path: "a"}` produce the same hash.

### premature_summary

Checks `runningBgTaskCount()` (imported from `lib/task-manager.ts`) whenever the model produces text output. If any background tasks are still running and the model has output 100+ characters, it fires.

This catches the pattern where the orchestrator starts summarizing after the first worker completes but before the others finish.

### multi_tool_attempt

Tracks `turnToolCallCount` -- incremented on each `tool_call` event, reset on `turn_end`. Fires when the count reaches 2 in the same turn.

This is a safety net for when `parallel_tool_calls: false` doesn't work (the Nestor API may silently ignore it). Cooldown of 1 and max of 10 means it fires aggressively.

### content_echoing

After each `read` tool call, the extension reads the file itself and stores the content in `lastReadResult`. On subsequent `message_update` events, it uses a sliding window overlap detector: divides the model's output into 50-character chunks and checks how many appear verbatim in the read content.

If 500+ characters overlap, the reminder fires and `lastReadResult` is cleared to prevent re-firing on the same content.

### self_contradiction

Pattern-matches the model's text output against deliberation phrases:

```typescript
const CONTRADICTION_PHRASES = [
  "wait,", "actually,", "no, ", "let me reconsider",
  "on second thought", "hmm,", "let me think again",
];
```

Fires when 3+ distinct phrases appear in a single turn's output. This targets Qwen's overthinking loops ("wait... actually... no, let me reconsider...").

## Tuning Guide

### When to increase thresholds

- **exploration_spiral**: If the agent legitimately needs to read many files before acting (e.g., large codebase exploration), increase to 7-10
- **verbose_output**: If tasks routinely require detailed responses, increase to 3000-4000 tokens
- **content_echoing**: If the agent needs to quote file contents (e.g., code review), increase overlap threshold to 1000+

### When to decrease thresholds

- **exploration_spiral**: If the agent wastes tokens on 3-file spirals, decrease to 3
- **multi_tool_attempt**: Keep at 2 (minimum possible) -- there's never a valid reason for multi-tool calls

### When to increase cooldowns

- If a reminder fires too often and the model starts ignoring it (reminder fatigue), increase the cooldown. A reminder that fires every other turn is noise.
- `premature_summary` cooldown of 3 is calibrated for typical 3-5 worker dispatches. For larger teams, increase to 5-8.

### When to increase max_per_session

- `write_after_plan_only` at 5 is already generous -- if the model violates plan-only 5 times, it's a model problem, not a config problem
- `multi_tool_attempt` at 10 is high because the violation is common with Qwen

## Adding a New Reminder

1. Add an entry to `reminders.yaml`:

```yaml
my_new_reminder:
  trigger:
    type: my_trigger_type
    threshold: 5
  message: >
    Your corrective message here. Be direct and actionable.
  cooldown: 3
  max_per_session: 5
```

2. Add detection logic in `index.ts`. Decide which event to hook:
   - `tool_call` -- for tool-related patterns (tool names, frequency, arguments)
   - `message_update` -- for output-related patterns (verbosity, content, phrasing)
   - `input` -- for user input-driven state (mode switches)

3. In the handler, call `fireReminder("my_new_reminder", reminders.my_new_reminder)`. The `canFire()` check handles cooldown and cap automatically.

4. If the trigger needs per-turn state, reset it in the `turn_end` handler.

The config name in YAML must match the key passed to `fireReminder()`. The `trigger.type` field is documentation only -- detection logic is in code, not dispatched by type string.
