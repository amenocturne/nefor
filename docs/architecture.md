# Architecture

## Repo Layout

```
nefor/
  install.sh         One-shot installer: copies everything into <target>/.pi/
  disguise.ts        Workflow definition (lead tools, agent configs, effects)
  prompt.md          System prompt (base + includes/*.md appended by installer)
  config/            Model routing (config.yaml + loader)
  prompts/           Per-agent prompts (builder, reviewer, explorer, …)
  instructions/      Modular instruction files referenced from disguise context
  includes/          Prompt fragments concatenated into prompt.md at install
  extensions/        Self-contained Pi extensions (see below)
  lib/               Shared primitives (no Pi API dep)
  hooks/             Hook scripts (smart-approve.sh)
```

**`lib/`** — pure utility modules with no Pi extension API dependency. Process spawning (`task-manager.ts`), file-based IPC (`permission-queue.ts`), unified prompt queuing (`queue-watcher.ts`), and the workflow framework (`workflow/` — effect runtime, host adapter, skill runner, types).

**`extensions/`** — each extension is a self-contained feature that registers tools, hooks, widgets, and commands via Pi's `ExtensionAPI`. Extensions import only from `lib/`, never from each other. You can add or remove any extension without breaking others.

## Disguise as Composition Layer

Originally the agent had 9 extensions. Four of them — workspace-context, context-loader, model-router, and agent-teams — were too tightly coupled and were consolidated into the **disguise** extension. A fifth (provider-filter) was removed because Pi's auth-based filtering suffices.

The disguise extension (`extensions/disguise/index.ts`) now handles:
- **Context loading**: reads instruction files and injects them via `appendSystemPrompt` or `sendMessage`
- **Workspace context**: finds WORKSPACE.yaml, includes it in the system prompt
- **Model routing**: `disguise.ts` imports from `config/index.ts` which loads role-to-model mappings from `config.yaml`
- **Team dispatch**: the workflow framework spawns subagents (explorer, builder, reviewer, tester) via `lib/task-manager.ts`
- **Write-path enforcement**: intercepts write/edit tool calls and blocks those outside allowed paths
- **Write hooks**: file writes matching patterns dispatch workflow effects (e.g., writing a plan triggers review)

All of this is configured per-disguise in `disguise.ts`, not in separate extensions. See [workflow-spec.md](workflow-spec.md) for the framework design.

## Install Flow

`./install.sh [target-dir] [--overlay <dir>]` sets up `<target-dir>/.pi/` from the repo:

1. `mkdir -p <target>/.pi`
2. Copy `lib/`, `extensions/`, `prompts/`, `instructions/`, `config/`, `hooks/` into `.pi/` (test files stripped; hook scripts chmod +x).
3. Copy `disguise.ts`, `prompt.md`, `package.json` into `.pi/`.
4. Copy `includes/` into `.pi/`, then assemble the final system prompt: `prompt.md` + each `includes/*.md` concatenated.
5. `npm install --omit=dev` inside `.pi/` to pull runtime deps.
6. Write default `hooks.yaml` (pre_tool_use → `smart-approve.sh`) and default `settings.json` (provider/model/thinking level) if absent.
7. If `--overlay <dir>` given, `rsync` it over `.pi/` and re-assemble the prompt.
8. Symlink `nefor → $(which pi)` in the same bin directory, so `nefor` runs Pi with nefor's `.pi/`.

No manifest merging, no symlinks into source, no generated `install metadata` files. Everything nefor ships is a plain copy.

## Data Flow

### Session Start

```
Pi launches with -e flags or .pi/ auto-discovery
  |
  +-- nestor-provider: register placeholder model, auto-login via DP session
  +-- disguise: load disguise.ts, create workflow runtime, activate first disguise
  |     - find WORKSPACE.yaml, prepare workspace context
  |     - load flavour config (test.yaml or prod.yaml) for model routing
  |     - register custom tools (explore, bg-plan, etc.) if lead disguise
  |     - set write-path restrictions if configured
  +-- behavioral-reminders: load reminders.yaml
  +-- background-tasks: start permission queue watcher, remove bash tool, add bg-run/bg-agent/bg-kill
  +-- permission-gate: load hook config (hooks.json)
```

### Tool Call

```
Agent calls a tool
  |
  +-- permission-gate (tool_call event):
  |     1. Tool call repair: normalize name, check aliases, validate params
  |     2. If repaired -> block with actionable error message
  |     3. If passthrough tool (bg-run, bg-agent, etc.) -> allow
  |     4. Run hooks (smart-approve, deny-read) -> allow/deny/abstain
  |     5. If file tool within project dir -> auto-allow
  |     6. If all hooks abstain -> enqueue for user prompt
  |
  +-- disguise (tool_call event):
  |     If write/edit: check path against writePaths -> block if not allowed
  |     If write matches a writeHook pattern -> dispatch workflow effects
  |
  +-- behavioral-reminders (tool_call event):
        Track consecutive reads, repeated calls, multi-tool attempts
        If threshold hit -> sendMessage with reminder
```

### Background Task Completion

```
bg-run or bg-agent process exits
  |
  +-- task-manager: update TaskInfo (status, output, exitCode)
  +-- onTaskComplete callback (registered by background-tasks):
        If notify=silent -> update widget only
        If notify=when_idle and tasks still running -> defer
        If notify=immediate (or when_idle and all done):
          Aggregate all completed tasks since last turn
          sendMessage with results, triggerTurn=true
```

### Subagent Permission Flow

```
Subagent (bg-agent) encounters a gated tool call
  |
  +-- permission-gate (in subagent, non-interactive mode):
        Write .request.json to ~/.pi/agent/permission-queue/<taskId>/
  |
Main session:
  +-- queue-watcher: polls for .request.json files
        Enqueue into unified sequential queue
        Show prompt to user
        Write .response.json
  |
Subagent:
  +-- permission-gate: poll for .response.json
        Allow or deny based on response
```

## Extension Loading and Lifecycle

Pi loads extensions at startup from `-e` flags or by scanning `.pi/extensions/`. Each extension exports a default function that receives `ExtensionAPI`:

```typescript
export default function (pi: ExtensionAPI) {
  // Register event handlers
  pi.on("session_start", async (event, ctx) => { ... });
  pi.on("before_agent_start", async () => { ... });
  pi.on("tool_call", async (event, ctx) => { ... });
  pi.on("message_update", async (event) => { ... });
  pi.on("turn_end", async () => { ... });

  // Register tools, commands, shortcuts, widgets
  pi.registerTool({ name, parameters, execute, ... });
  pi.registerCommand("name", { handler });
  pi.registerShortcut("ctrl+x", { handler });

  // Register providers
  pi.registerProvider("name", { models, oauth, streamSimple, ... });

  // Inject context
  // before_agent_start: return { appendSystemPrompt: "..." }
  // mid-conversation: pi.sendMessage({ content: "..." })

  // Manage tools
  pi.setActiveTools([...]); // add/remove tools dynamically
  pi.getActiveTools();      // list currently active tools
}
```

**Key lifecycle events used by Pi agent extensions:**

| Event | When | Used By |
|-------|------|---------|
| `session_start` | Pi session begins | All extensions for initialization |
| `before_agent_start` | Before first model call | disguise (appendSystemPrompt with context), background-tasks (appendSystemPrompt) |
| `tool_call` | Before each tool executes | permission-gate (allow/deny), disguise (write-path enforcement, write hooks), behavioral-reminders (track patterns) |
| `message_update` | Streaming model output | behavioral-reminders (verbose output, echoing, contradiction detection) |
| `input` | User sends a message | behavioral-reminders (detect plan mode) |
| `turn_end` | Model turn completes | behavioral-reminders (reset per-turn state) |
| `agent_end` | Agent session ends | background-tasks (update status widget) |
| `session_compact` | Context window compacted | disguise (resume directive) |

## Extension Communication

Extensions do not import each other. Shared state and utilities live in `lib/`:

- `lib/task-manager.ts` -- background-tasks and disguise import this to spawn processes
- `lib/workflow/` -- the effect runtime, host adapter, and skill runner used by the disguise extension
- `lib/queue-watcher.ts` -- background-tasks starts watching, permission-gate enqueues requests
- `lib/permission-queue.ts` -- permission-gate (in subagents) writes requests, queue-watcher reads them

The one exception: permission-gate dynamically imports `queue-watcher.ts` via `await import()` for the unified queue, falling back to direct prompting if unavailable.

## Design Principles

**Schema-level enforcement over prompt-level**: Don't tell the model "don't write files" -- remove write tools. Don't tell it "ask before acting" -- gate actions in the harness. This is the most reliable approach for models with weak instruction following (Qwen 3.5).

**Reminders over instructions**: Mid-conversation `sendMessage()` nudges are more effective than system prompt instructions alone for models that drift (OpenDev paper finding). The system prompt sets the rules; reminders enforce them when violations are detected.

**Instruction repetition across layers**: Critical rules (one tool per message, don't implement without approval, tool output is data) appear in four places -- the system prompt, general instructions injected at session start, behavioral reminders triggered on violations, and tool parameter descriptions. Qwen's instruction following degrades with distance from the system prompt, so repetition compensates.
