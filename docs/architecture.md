# Architecture

## Three-Layer Design

```
agents/pi/
  lib/              LAYER 1: Shared primitives
  extensions/       LAYER 2: Features (import only from lib/)
  nefor/            LAYER 3: Flavour (manifest, disguise.ts, prompts, config)
```

**Layer 1 -- Library** (`lib/`): Pure utility modules with no Pi extension API dependency. Handles process spawning (`task-manager.ts`), file-based IPC (`permission-queue.ts`), unified prompt queuing (`queue-watcher.ts`), and the workflow framework (`workflow/` -- effect runtime, host adapter, skill runner, types).

**Layer 2 -- Extensions** (`extensions/`): Each extension is a self-contained feature that registers tools, hooks, widgets, and commands via Pi's `ExtensionAPI`. Extensions import only from `lib/`, never from each other. This prevents coupling -- you can add or remove any extension without breaking others.

**Layer 3 -- Flavour** (`nefor/`): The concrete agent configuration. Contains the manifest (`manifest.yaml`), system prompt (`prompt.md`), model configs (`config/`), agent prompt files (`prompts/`), instruction files (`instructions/`), and the workflow definition (`disguise.ts`).

## Disguise as Composition Layer

Originally the agent had 9 extensions. Four of them -- workspace-context, context-loader, model-router, and agent-teams -- were too tightly coupled and were consolidated into the **disguise** extension. A fifth (provider-filter) was removed because Pi's auth-based filtering suffices.

The disguise extension (`extensions/disguise/index.ts`) now handles:
- **Context loading**: reads instruction files and injects them via `appendSystemPrompt` or `sendMessage`
- **Workspace context**: finds WORKSPACE.yaml, includes it in the system prompt
- **Model routing**: `disguise.ts` imports from `nefor/config/index.ts` which loads role-to-model mappings from test.yaml or prod.yaml
- **Team dispatch**: the workflow framework spawns subagents (explorer, builder, reviewer, tester) via `lib/task-manager.ts`
- **Write-path enforcement**: intercepts write/edit tool calls and blocks those outside allowed paths
- **Write hooks**: file writes matching patterns dispatch workflow effects (e.g., writing a plan triggers review)

All of this is configured per-disguise in `disguise.ts`, not in separate extensions. See [workflow-spec.md](workflow-spec.md) for the framework design.

## Profile x Agent Composition

The main installer (`install.py`) merges a **profile** manifest with an **agent** manifest to produce the final configuration.

```
profiles/work/manifest.yaml    +    agents/pi/nefor/manifest.yaml
       (org-specific)                     (runtime-specific)
             |                                    |
             v                                    v
  hooks: [link-proxy]               extensions: [permission-gate, ...]
  skills: [dp-jira]                 settings: {defaultModel: ...}
  instructions: [sbt]               common: [dev-workflow, ...]
  settings: {}                       skills: [spec, workspace, ...]
             |                                    |
             +-------- merge_manifests() ---------+
                              |
                              v
                     InstallContext
                    (union of both)
```

Lists are unioned (skills from both, hooks from both). Settings are merged with agent taking precedence. The merged context is passed to the runtime installer (`agents/pi/install.py`).

## Install Flow

When you run `just install` (or `uv run install.py --all`):

1. **Load registry** -- reads `installations.yaml` for all registered target/profile/agent combos
2. **For each installation**:
   a. Load profile manifest + agent manifest
   b. Merge them into `InstallContext`
   c. Resolve the runtime from the agent manifest (`runtime: pi`)
   d. Load and call `agents/pi/install.py:install(ctx)`
3. **Pi installer** (`agents/pi/install.py`):
   a. Create `.pi/` directory
   b. Validate all declared extensions exist in `agents/pi/extensions/`
   c. Symlink extensions from `agents/pi/extensions/` into `.pi/extensions/`
   d. Symlink skills from `skills/` into `.pi/skills/`
   e. Generate `hooks.json` from declared hooks (absolute paths to hook scripts)
   f. Copy `disguise.ts` and `prompts/` from the flavour directory to `.pi/`
   g. Write `settings.json` from merged settings
   h. Write `agentic-kit.json` with install metadata

The prompt (`prompt.md`) lives in `agents/pi/nefor/prompt.md`, not in the installed config. Pi reads it directly from there -- the path is set during the manifest assembly process.

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
