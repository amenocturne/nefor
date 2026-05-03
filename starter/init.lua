-- starter/init.lua — default engine composition.
--
-- Post Slice 2 I4 the engine is pure glue: no hardcoded NCP behavior, no
-- bundled widgets. This file is the canonical reference config:
--
--   1. Wire `package.path` so `require("ncp")` resolves to the bundled
--      protocol module next to this file.
--   2. Optionally declare a parent session id to resume from (commented out
--      by default — uncomment and fill in a uuid to continue a prior run).
--   3. Define the global `step` hook the engine calls on every inbound line.
--      Delegates to `ncp.step` — the protocol module is where the NCP v0.1
--      semantics live.
--   4. Register plugins via `nefor.plugins.spawn`. Mirrors the pre-split
--      reference config (`tmp/smoke-config-m2/init.lua`) plus the
--      combinators plugin; swap or remove entries to compose your own stack.
--
-- ### T7 — Stage 1 starter wiring (post-Phase-1B)
--
-- The chat plugin no longer talks to a provider directly. Instead a single
-- `agentic_workflow` module owns the orchestration glue: it intercepts
-- `chat.input.submit`, drives the reasoner-graph against the provider via
-- a template orchestrator graph (provider-wrapper + tool-executor +
-- adapter + terminal cycle), wires the spawn_graph tool, and surfaces
-- run completions back to nefor-chat. See
-- `starter/agentic_workflow.lua` for the full event flow.
--
-- Run:
--   NEFOR_PLUGIN_DIR=$PWD/plugins cargo run --bin nefor -- --config ./starter

-------------------------------------------------------------------------
-- 1. Lua module path — bundled protocol + json alongside this file
-------------------------------------------------------------------------
local STARTER_ROOT = NEFOR_CONFIG_DIR or "."

package.path = table.concat({
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  package.path,
}, ";")

-------------------------------------------------------------------------
-- 2. Optional parent session id (resume a prior run)
-------------------------------------------------------------------------
--
-- Manual resume: uncomment + fill a session UUID to force a one-shot
-- resume of a specific past run. The engine reads
-- `~/Library/Application Support/nefor/sessions/<id>.jsonl`, hydrates
-- saved_log, and calls every step invocation with it.
--
-- nefor.parent_session = "00000000-0000-0000-0000-000000000000"
--
-- Auto-resume via sidechannel: when chat.lua's `/resume` picker selects
-- a session, it writes the chosen id to the resume_target file below
-- and calls `nefor.engine.exit(0)`. The user re-launches; this block
-- reads the file, sets parent_session, deletes the file (so the *next*
-- boot starts fresh), and flips `resume.is_active()` so ncp.lua routes
-- saved_log entries through per-plugin transforms registered below.
--
-- Why a sidechannel + relaunch instead of in-place resume: rebuilding
-- every plugin's state from saved_log requires shutting them down first
-- (a provider mid-stream can't be told "you have a different chat now").
-- Re-execve of the engine binary is the cleanest reset; sidechannel is
-- the smallest surface that survives the process boundary.

local resume = require("resume")

local function read_resume_target()
  -- macOS XDG-equivalent path. The engine session writer uses this same
  -- root via dirs::data_dir(). Linux/Windows users would need to adjust;
  -- v1 ships the macOS path because that's the developer's host.
  local home = os.getenv("HOME") or ""
  if home == "" then return nil end
  local path = home .. "/Library/Application Support/nefor/resume_target"
  local fh = io.open(path, "r")
  if fh == nil then return nil, path end
  local content = fh:read("*a") or ""
  fh:close()
  -- Trim whitespace including trailing newline.
  content = content:gsub("^%s+", ""):gsub("%s+$", "")
  if #content == 0 then return nil, path end
  -- Defence-in-depth: refuse anything that doesn't look like a UUID. A
  -- garbled file shouldn't blow up boot.
  if not content:match("^[%w%-]+$") then return nil, path end
  return content, path
end

do
  local target_id, target_path = read_resume_target()
  if target_id ~= nil then
    nefor.parent_session = target_id
    resume.set_active(true)
    -- Best-effort delete so the next fresh boot doesn't auto-resume.
    -- Failure here is non-fatal — the only consequence is the next boot
    -- also resumes the same session, which is at worst confusing.
    if target_path ~= nil then
      os.remove(target_path)
    end
  end
end


-------------------------------------------------------------------------
-- 3. Step function
-------------------------------------------------------------------------
local ncp = require("ncp")

function step(saved_log, current_log)
  ncp.step(saved_log, current_log)
end

-------------------------------------------------------------------------
-- 4. Plugin composition
-------------------------------------------------------------------------

local agentic_workflow = require("agentic_workflow")

-- Plugin cwd is <plugin_root>/<name>/ (engine policy).
local PROJECT_ROOT = STARTER_ROOT:match("^(.*)/[^/]+$") or "."
local function bin(name) return PROJECT_ROOT .. "/target/debug/" .. name end

-------------------------------------------------------------------------
-- 4a. Spawn order
-------------------------------------------------------------------------
--
-- Order matters because plugins register types/Into declarations
-- against `nefor-combinators` at startup, and the scheduler queries
-- combinators at submit time. The safe order:
--
--   1. nefor-combinators       (registry)
--   2. generic-provider        (canonical type tags)
--   3. generic-tool            (canonical type tags)
--   4. openai-provider(s)      (declare Into against canonical types)
--   5. reasoner-graph          (queries combinators on submit)
--   6. tool-gate               (aggregates tool advertisements)
--   7. basic-tools             (advertises tools to the gate)
--   8. nefor-tui                (UI; can come up any time — chat is a Lua composition inside it)
--
-- ncp.lua's replay-on-attach means a late-attaching plugin still sees
-- prior events, so this ordering is a robustness measure rather than a
-- hard correctness requirement. It's still worth respecting because
-- the combinators registry is queried synchronously during submit —
-- if reasoner-graph submitted a graph before combinators readied, the
-- query would block on a peer that doesn't exist yet.

ncp.spawn {
  name    = "nefor-combinators",
  command = { bin("nefor-combinators") },
}

ncp.spawn {
  name    = "generic-provider",
  command = { bin("generic-provider") },
}

ncp.spawn {
  name    = "generic-tool",
  command = { bin("generic-tool") },
}

-------------------------------------------------------------------------
-- 4b. Provider — real openai-provider against Ollama, OR mock-plugin
-------------------------------------------------------------------------
--
-- USE_MOCK_PROVIDER=true swaps the live LLM for a deterministic mock.
-- The mock-plugin binary loads `starter/mock_provider.lua` and emits
-- the same `<name>.chat.{create,append,complete[.result]}` /
-- `<name>.stream.delta`/`<name>.stream.end` shape openai-provider
-- emits, with hardcoded canned responses for the smoke test prompt.
-- See `starter/mock_provider.lua` for the response selection logic.
--
-- For the real provider: one chat session per orchestrator instance;
-- agentic_workflow's reasoner-graph adapter calls
-- `<name>.chat.create / chat.append / chat.complete` directly. The
-- static_token=ollama-local trick unlocks openai-provider's auth gate
-- without a real key (required for local Ollama). Real remote providers
-- would supply an --api-key CLI arg.
local USE_MOCK_PROVIDER = false

local PROVIDER_NAME, PROVIDER_MODEL, provider_chain, provider_command

if USE_MOCK_PROVIDER then
  PROVIDER_NAME  = "mock-plugin"
  PROVIDER_MODEL = "mock-model"
  provider_chain = agentic_workflow.for_provider(PROVIDER_NAME)
  provider_command = {
    bin("mock-plugin"),
    "--script", STARTER_ROOT .. "/mock_provider.lua",
  }
else
  PROVIDER_NAME  = "ollama"
  -- qwen3.6:35b-a3b-coding-mxfp8 — MoE (3B active params, 35B total) with
  -- strong tool-calling. Verified one-shot to emit a clean spawn_graph
  -- with proper terminal wiring; faster than the dense 27b model.
  PROVIDER_MODEL = "qwen3.6:35b-a3b-coding-mxfp8"
  provider_chain = agentic_workflow.for_provider(PROVIDER_NAME, { static_token = "ollama-local" })
  provider_command = {
    bin("openai-provider"),
    "--name",     PROVIDER_NAME,
    "--base-url", "http://localhost:11434",
    "--model",    PROVIDER_MODEL,
  }
end

-------------------------------------------------------------------------
-- 4c. Orchestrator setup — single configuration call
-------------------------------------------------------------------------
--
-- Stage-1 system prompt: teaches the orchestrator model when and how
-- to use `spawn_graph`. Kept terse on purpose — Gemma 3 reasons itself
-- into a "stop" finish without committing to the tool call when the
-- prompt is dense. Schema-only worked-example was enough to make it
-- emit a well-formed graph reliably; the verbose version was not.
-- Two reasoner types are documented because those are the ones
-- agentic_workflow handles for sub-graphs (`responder` = one-shot LLM,
-- `terminal` = sink); other reasoner types are private to the
-- orchestrator's chat loop and would just confuse the model.
local ORCHESTRATOR_SYSTEM_PROMPT = [[
You are a helpful assistant. Use the `spawn_graph` tool for parallel decomposition tasks (multiple independent sub-questions to combine).

Graph schema:
{ "nodes": [{ "id": str, "reasoner": str, "args": {...} }], "edges": [{ "from": str, "to": str }] }

Reasoner types:
- `responder` — one-shot LLM call. args: { "prompt": string }. Upstream nodes' outputs become user messages prepended to the prompt.
- `terminal` — sink. args: {}. Exactly one per graph; its input becomes the run's result.

To combine parallel branches into a single output, add a `responder` combine node downstream of the parallel branches and feed it into terminal. Do NOT wire parallel branches directly into terminal — terminal is a sink, not a combiner. Pattern:
  branchA, branchB → combine (responder) → terminal

Emit the tool call directly after deciding the structure. For simple chat turns (no decomposition benefit), just answer directly.
]]

agentic_workflow.setup {
  provider = PROVIDER_NAME,
  model    = PROVIDER_MODEL,
  system   = ORCHESTRATOR_SYSTEM_PROMPT,
}

ncp.spawn {
  name        = PROVIDER_NAME,
  command     = provider_command,
  from_plugin = provider_chain.from_plugin,
  to_plugin   = provider_chain.to_plugin,
}

-------------------------------------------------------------------------
-- 4d. Reasoner graph
-------------------------------------------------------------------------

ncp.spawn {
  name        = "reasoner-graph",
  command     = { bin("reasoner-graph") },
  from_plugin = agentic_workflow.for_reasoner_graph().from_plugin,
}

-------------------------------------------------------------------------
-- 4e. Tool gate + basic-tools + spawn_graph advertisement
-------------------------------------------------------------------------

ncp.spawn {
  name        = "tool-gate",
  command     = {
    bin("tool-gate"),
    "--prompt",  "read_file",
    "--default", "prompt",
  },
  from_plugin = agentic_workflow.for_tool_gate("tool-gate").from_plugin,
}

ncp.spawn {
  name    = "basic-tools",
  command = { bin("basic-tools"), "--gate", "tool-gate" },
}

-------------------------------------------------------------------------
-- 4f. Chat
-------------------------------------------------------------------------
--
-- Post-phase-6 cutover: the chat surface is a Lua composition (`chat.lua`)
-- running inside the new declarative `nefor-tui` plugin. The plugin loads
-- the script via `--script <path>` and exposes a `tui.*` primitive surface
-- (text, column, row, scrollable, text_input, markdown, ...) that
-- `chat.lua` composes into the transcript + statusline + input. The
-- legacy split (`nefor-chat` + ratatui-based `nefor-tui`) is gone.

ncp.spawn {
  name        = "nefor-tui",
  command     = { bin("nefor-tui"), "--script", STARTER_ROOT .. "/chat.lua" },
  from_plugin = agentic_workflow.for_chat().from_plugin,
}
