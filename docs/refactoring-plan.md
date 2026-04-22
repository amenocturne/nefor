# Nefor Refactoring Plan

Date: 2026-04-07

## Background

The pi agent harness (nefor) was designed with a 9-extension architecture. After testing, 4 extensions (workspace-context, context-loader, model-router, agent-teams) were found to be too coupled and intentionally consolidated into the single disguise extension — which allows building custom agent workflows on the fly. The docs were never updated to reflect this architectural decision. Meanwhile, Pi itself (v0.60→v0.65) has shipped features that overlap with some custom code, and introduced breaking API changes.

## Problems

### P1: Review tool doesn't work from orchestrator (Bug)

**Symptom**: After orchestrator writes plan.md and invokes review skill via `ctx.skill("review", ["--mode", "text", ...])`, the browser doesn't open. User must run review manually, add comments, then copy-paste the review file contents.

**Root cause**: Process architecture — no TTY in subprocess chain.

The call chain:
```
disguise.ts:130        ctx.skill("review", [...])
  → runtime.ts:83      host.runSkill()
    → host.ts:134       looks up skill, calls runSkill()
      → skills.ts:57    spawn("sh", [...], { stdio: ["ignore", "pipe", "pipe"] })
        → just launch → bun run server.ts
          → server.ts:408  Bun.spawn(["open", url])  ← FAILS SILENTLY
```

`skills.ts:57` spawns the subprocess with `stdio: ["ignore", "pipe", "pipe"]` to capture stdout (needed to read the `saved: <path>` output). This creates a process chain with no TTY and no macOS WindowServer access. The `open` command requires GUI context to launch a browser — it fails silently from a piped subprocess. `Bun.spawn()` at `server.ts:408` is fire-and-forget with no error checking.

The server stays alive correctly (it waits for submission, then exits after 500ms — that part is fine). The problem is solely that the browser never opens.

**Why it works in Claude Code**: The review SKILL.md says "CRITICAL: Use `run_in_background` parameter." In Claude Code, the agent launches the review server as a background Bash task (which has TTY access and can run `open`). The agent reads the URL from output, tells the user, then waits for the background task notification. This is the intended flow.

**Why it fails in Pi**: `ctx.skill()` in `skills.ts:57` runs synchronously with `stdio: ["ignore", "pipe", "pipe"]` — wrong execution model. The review tool needs background execution with TTY access, matching what Claude Code does.

**Fix options** (in order of preference):
1. **Add `ctx.backgroundSkill()` to workflow host**: Runs the skill via the background task manager (`spawnCommand()` from `task-manager.ts`), which may have different stdio handling. Returns a handle the caller can await. The `write-review` tool would use this, read the URL from output, present it to the user, and wait for completion.
2. **Have the skill output a URL line, let the host open it**: Review server prints `open: http://localhost:PORT` to stdout early. `skills.ts` or `host.ts` detects this pattern and calls `pi.exec("open", [url])` from the parent process (which has TTY). Server continues running until submission.
3. **Use Pi's `ui.notify()` or `sendMessage()`**: Instead of `open`, have the host present the URL to the user via Pi's UI. User clicks/copies the URL manually. Less elegant but reliable.

### P2: Plan file overwrite blocked by permission-gate (Bug)

**Symptom**: After getting review feedback, the agent tries to write a revised plan to `tmp/plans/plan.md`. The Write tool fails with "you must read a file before editing it." The agent stops and the user has to delete the file manually and resume.

**Root cause**: Permission-gate enforces read-before-edit — the agent wrote `plan.md` initially but never read it, so permission-gate blocks the overwrite. The filename is hardcoded at `disguise.ts:328`: `"Write your plan to tmp/plans/plan.md first."`

**Fix**: Use timestamped filenames like review files already do. Changes:
- `disguise.ts:328` — change prompt to: `"Write your plan to tmp/plans/plan-<timestamp>.md first."`
- `prompts/lead.md` — update any references to hardcoded `plan.md` filename
- The `write-review` tool handles plan creation and review in a single blocking call — no separate write step needed
- Consider having the tool track the latest plan path in state for reference

### P3: Stale documentation (Debt)

Docs describe the pre-consolidation 9-extension architecture. Reality is 6 extensions + workflow framework + disguise as the composition layer.

| Document | Issue |
|----------|-------|
| README.md | Says "9 extensions" and "9 feature extensions" — should be 6 |
| architecture.md | References `lib/model-router.ts` (doesn't exist), lists 4 non-existent extensions in session flow |
| extensions.md | 4 full sections describing consolidated extensions as if they're standalone |
| model-routing.md | References `getModelForRole()` which doesn't exist as standalone function |
| teams.md | Describes `bg-dispatch` tool and `/team` command that don't exist |
| workflow-plan.md | Completed work still formatted as TODO with unchecked checkpoints |

**Fix**: Update all docs to reflect the actual architecture. The story is: "We started with 9 extensions, found coupling issues, consolidated into disguise + workflow framework. The disguise extension is the composition layer that provides context-loading, model routing, team dispatch, and workspace context — all configured per-agent in disguise.ts."

### P4: Provider clutter in UI (UX)

provider-filter extension registers dummy providers with empty model arrays to hide unwanted built-in providers. This is a hack.

**Finding**: Pi's model picker only shows providers with configured auth (`getAvailable()` filters by `hasConfiguredAuth()`). Built-in providers without auth keys simply don't appear.

**Fix**: Remove provider-filter extension entirely. Just don't configure auth for unwanted providers. If the user only has nestor and openrouter auth configured, only those appear. This is cleaner, zero-maintenance, and uses Pi's intended mechanism.

### P5: Bugs in existing extensions

| Extension | Bug | Severity |
|-----------|-----|----------|
| **disguise** | `originalTools` not reset on `/new` session — stale tool restrictions carry over | High |
| **nestor-provider** | Global mutable `dpPath`/`piRef` — race condition with concurrent sessions | Medium |
| **permission-gate** | `callHashKey()` doesn't sort object keys — inconsistent hashing for equivalent objects | Low |
| **behavioral-reminders** | Content echoing detection uses naive 50-char overlap — false positives | Low |

### P6: Pi API compatibility (Risk)

Pi v0.60→v0.65 had breaking changes in 4 out of 6 minor versions. Need to verify our code:

| Breaking Change | Version | Risk |
|-----------------|---------|------|
| `sourceInfo` replaces `extensionPath`/`location`/`path` on tools/commands | v0.62.0 | Needs audit |
| `ToolDefinition.renderCall/renderResult` semantics changed | v0.62.0 | background-tasks may be affected |
| `ModelRegistry` constructor removed, use `.create()` | v0.64.0 | Needs audit |
| `session_switch`/`session_fork` removed → `session_start` with `event.reason` | v0.65.0 | disguise already uses `event.reason`, likely OK |

### P7: Missed Pi built-in features (Opportunity)

| Pi Feature | Our Code | Opportunity |
|------------|----------|-------------|
| `resources_discover` hook | `workflow/skills.ts` manual discovery | Could simplify skill registration |
| `context` event (transform messages before LLM) | No context-loader | Could add lightweight instruction injection |
| `defineTool()` helper (v0.65) | Manual TypeBox schemas | Cleaner tool definitions |
| `prepareArguments` hook (v0.64) | permission-gate's repair logic | Could simplify tool name/arg repair |

## Execution Plan

### Phase A: Bug fixes & compatibility (do first)

1. **Fix review tool execution model** — add `ctx.backgroundSkill()` or equivalent to workflow host so review can run with TTY access and background completion notification
2. **Fix plan file overwrite** — use timestamped filenames in disguise.ts prompts, update lead.md
3. **Fix disguise `originalTools` reset** — add reset in the `/new` session handler
4. **Fix nestor-provider global state** — scope per-session via closure
5. **Audit v0.62+ compatibility** — grep for `extensionPath`, `location`, `renderCall`, `renderResult` and update
6. **Fix permission-gate `callHashKey`** — sort object keys before stringifying

### Phase B: Remove provider-filter, simplify provider setup

1. **Delete `extensions/provider-filter/`** entirely
2. **Remove `provider-filter` from `nefor/manifest.yaml`** extensions list
3. **Ensure auth is only configured for nestor/openrouter** — verify `.pi/settings.json` and auth setup
4. **Update docs** — remove provider-filter references

### Phase C: Update documentation

1. **README.md** — correct extension count, add note about consolidation into disguise
2. **architecture.md** — remove ghost references, describe actual architecture (disguise as composition layer)
3. **extensions.md** — remove 4 consolidated extension sections, add section explaining the consolidation
4. **model-routing.md** — describe actual mechanism (`config/` + disguise.ts inline routing)
5. **teams.md** — either describe how teams work via disguise.ts, or mark bg-dispatch/team as future work
6. **workflow-plan.md** — convert to implementation summary or delete (workflow-spec.md already documents the implemented system)

### Phase D: Adopt Pi built-ins (opportunistic, after A-C)

1. **Evaluate `resources_discover`** for skill registration — if it simplifies workflow/skills.ts, adopt it
2. **Evaluate `context` event** for instruction injection — lightweight context-loader via disguise
3. **Consider `defineTool()`** for new tool definitions (don't rewrite existing working code)
4. **Consider `prepareArguments`** for permission-gate's tool name repair (only if it simplifies significantly)

Note: Extension independence is a core principle. Extensions must work independently — no extension should depend on another. Any use of `pi.events` EventBus must be fire-and-forget, with each extension gracefully handling the absence of others.

### Phase E: Behavioral improvements (low priority)

1. **behavioral-reminders**: Improve content echoing detection or remove the unreliable pattern
2. **nestor-provider**: Make model capability inference configurable instead of hardcoded pattern matching
3. **prod.yaml**: Update model references if stale (currently Claude Sonnet 4)

## Non-goals

- **Rebuilding consolidated extensions as separate modules** — the consolidation into disguise was intentional and correct
- **Adding teams UI (bg-dispatch, /team)** — only if there's a concrete need
- **Rewriting working extensions** to use new Pi APIs — only adopt new APIs where they clearly simplify
