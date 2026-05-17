-- starter/reasoners/agent.lua — `agent` reasoner type.
--
-- Composes the `provider-wrapper` + `tool-executor` patterns inline
-- into a single self-contained reasoner that runs its own per-firing
-- agentic-loop: provider call → optional tool calls → results →
-- provider → … → terminal.
--
-- ## Dispatch envelope
--
--   tool.invoke {
--     id   = <firing_id>,
--     name = "agent",
--     args = {
--       system_prompt      = <string>,
--       model              = <string>,
--       tool_allowlist     = <list<string>>,
--       prompt             = <string>,    -- the user task
--       additional_context = <string?>,   -- optional, inlined after system
--       provider           = <string?>,   -- override; defaults to cfg.provider
--     }
--   }
--
-- ## Reply envelope (terminal)
--
--   tool.result {
--     id     = <firing_id>,
--     result = {
--       text       = <final assistant answer>,
--       structured = <opaque>?,    -- populated when the agent terminates
--                                  -- via the `finalize` tool (see below);
--                                  -- absent on text-only termination.
--       next_state = { chat_id = <string> },
--     }
--   }
--
-- ## `finalize` tool
--
-- A synthetic tool the agent reasoner injects into every firing's
-- advertised set. Sub-agents call `finalize(answer, ...arbitrary
-- fields...)` to terminate the run with structured output. The
-- reasoner intercepts the call BEFORE allowlist enforcement /
-- tool-gate dispatch:
--
--   * `result.structured` = the full args object (so downstream
--     reasoner combinators can read typed fields like `findings`,
--     `risks`, `confidence` to compose the next agent's prompt).
--   * `result.text`       = `args.answer` (a human-readable summary
--     for non-structured-aware consumers).
--
-- `finalize` is auto-included regardless of the caller's
-- `args.tool_allowlist` — it's an agent-internal terminator, not a
-- routed tool. If the model returns `finalize` alongside other
-- tool_calls in the same response, `finalize` wins (the others are
-- dropped). Empty / missing `args.answer` synthesizes a placeholder
-- text rather than erroring — the structured payload is still
-- captured.
--
-- ## Internal turn-cycle
--
-- Per-firing state is held in a module-level `agents[firing_id]` table
-- (NOT threaded via `next_state` because reasoner-graph re-fires only
-- on cyclic graphs; a single-node `agent` firing has no edge that would
-- carry `prev_state` back). We instead watch the bus directly for the
-- provider replies and tool results that target our chat_ids /
-- tool_ids:
--
--   on tool.invoke{name="agent"}:
--     1. mint chat_id, register agents[firing_id] = { chat_id, ... }
--     2. emit <prov>.chat.create { chat_id, model, tools = allowlist }
--        (note: the binary filters its outgoing tool-advertisement set,
--        but we still enforce per-call in-reasoner — see step 4)
--     3. emit <prov>.chat.append { role="system", content=system+ctx }
--     4. emit <prov>.chat.append { role="user", content=prompt+inputs }
--        Upstream dispatch-graph dependencies arrive on the outer
--        envelope as `body.inputs[<dep_id>] = { output = ... }`. Each
--        dep's terminal output is rendered as a `[<dep_id>]\n<text>`
--        section appended after the prompt — single user message,
--        prompt first then deps, separated by `---`. The model reads
--        the task instruction first, then the data it must act on
--        (recency-ordered so each downstream step sees the freshest
--        upstream output last). Top-level nodes (no deps) get the
--        bare prompt.
--     5. emit <prov>.chat.complete { chat_id }
--
--   on <prov>.chat.complete.result for our chat_id:
--     - if reply has tool_calls:
--         * dispatch each via tool-gate.tool.invoke (or, for disallowed
--           names, synthesize a local tool.result{error}) and await
--         * track outstanding tool_ids
--     - if reply has only text (no tool_calls):
--         * emit terminal tool.result{id=firing_id, result={text,
--           next_state={chat_id}}} and clear state
--
--   on tool.result for one of our tool_ids:
--     - record the result, decrement outstanding count
--     - when all outstanding tool results for this turn have landed:
--       * for each, emit <prov>.chat.append { role="tool", ... }
--       * emit <prov>.chat.complete { chat_id }  -- next turn
--
--   on <prov>.chat.error for our chat_id:
--     - emit terminal tool.result{id=firing_id, error=<msg>} and clear

local json = nefor.json

local envelope      = require("core.envelope")
local replay_window = require("core.history_replay")

local emit_as = envelope.emit_as
local emit    = envelope.emit
local next_id = envelope.next_id

local M = {}

-- The `finalize` tool's reserved name. Calls under this name are
-- intercepted as terminators, not dispatched to tool-gate.
local FINALIZE_NAME = "finalize"

-- JSON schema for the `finalize` tool. Only `answer` is required;
-- additional fields pass through into `result.structured` verbatim
-- (`additionalProperties = true`). Role-specific schemas layer on top
-- of this base by adding fields like `findings` / `risks` /
-- `context_for_next_agent` to the role's system prompt.
local FINALIZE_SCHEMA = {
  type = "function",
  ["function"] = {
    name = FINALIZE_NAME,
    description = "Terminate this agent's run with a final answer and any structured fields needed by downstream nodes.",
    parameters = {
      type       = "object",
      properties = {
        answer = {
          type        = "string",
          description = "Human-readable final answer.",
        },
      },
      required             = { "answer" },
      additionalProperties = true,
    },
  },
}

M.FINALIZE_NAME   = FINALIZE_NAME
M.FINALIZE_SCHEMA = FINALIZE_SCHEMA

-- Compose upstream graph-node outputs into a single context block to
-- append after the user prompt. Each dependency's terminal output (the
-- `result.{text,structured}` send_terminal_ok emitted) arrives as
--   inputs[<dep_id>] = { output = { text, structured?, ... } }
-- or as an upstream error
--   inputs[<dep_id>] = { error = <string> }.
--
-- Returns an empty string when there are no upstream inputs (top-level
-- nodes in the graph). When non-empty the caller appends it to the
-- user message after the prompt, separated from the prompt by `---`.
-- Append (not prepend) so the freshest data sits last in the model's
-- context — long-chain graphs amplify recency benefits.
local function build_inputs_block(inputs)
  if type(inputs) ~= "table" then return "" end

  local ids = {}
  for k, _ in pairs(inputs) do
    if type(k) == "string" then ids[#ids + 1] = k end
  end
  if #ids == 0 then return "" end
  table.sort(ids)

  local parts = {}
  for _, dep_id in ipairs(ids) do
    local entry = inputs[dep_id]
    local section
    if type(entry) ~= "table" then
      -- skip
    elseif type(entry.error) == "string" and #entry.error > 0 then
      section = string.format("[%s] [error] %s", dep_id, entry.error)
    elseif entry.output ~= nil then
      local out = entry.output
      local body
      if type(out) == "string" then
        body = out
      elseif type(out) == "table" then
        if type(out.text) == "string" and #out.text > 0 then
          body = out.text
        elseif out.structured ~= nil then
          body = json.encode(out.structured)
        else
          body = json.encode(out)
        end
      else
        body = tostring(out)
      end
      section = string.format("[%s]\n%s", dep_id, body)
    end
    if section ~= nil then
      parts[#parts + 1] = section
    end
  end

  if #parts == 0 then return "" end
  return "Inputs from upstream nodes:\n\n" .. table.concat(parts, "\n\n")
end

-- Forward-declared; bound on first dispatch (require cycle: agent.lua
-- is loaded by reasoners/init.lua, which is loaded by agentic-loop's
-- module path through indirect requires).
local agentic_loop

local function al()
  if agentic_loop == nil then
    agentic_loop = require("agentic-loop")
  end
  return agentic_loop
end

-- Per-firing state.
--
-- agents[firing_id] = {
--   firing_id      = string,
--   run_id         = string,         -- enclosing graph run_id; used to
--                                    -- match `graph.cancel { run_id }` so
--                                    -- in-flight provider streams under a
--                                    -- cancelled run are interrupted.
--   chat_id        = string,
--   provider       = string,         -- e.g. "ollama" / "mock-plugin"
--   tool_allowlist = { string -> true } | nil,
--   pending_tools  = {                -- per-turn outstanding tool calls
--     [tool_id] = {
--       call_id      = <model-side id>,  -- echoed back in the role=tool message
--       name         = string,
--       result_text  = string?,           -- filled when result arrives
--       error        = string?,           -- filled when result arrives or synthesised
--       received     = bool,
--     }
--   },
--   pending_order  = { tool_id, ... },    -- preserves dispatch order
--   pending_count  = int,                 -- outstanding (received=false)
-- }
--
-- chat_to_firing[chat_id] = firing_id
-- tool_to_firing[tool_id] = firing_id
local agents          = {}
local chat_to_firing  = {}
local tool_to_firing  = {}

local function build_allowlist_set(list)
  if type(list) ~= "table" then return nil end
  local s = {}
  for _, n in ipairs(list) do
    if type(n) == "string" and #n > 0 then s[n] = true end
  end
  return s
end

local function clear_firing(firing_id)
  local entry = agents[firing_id]
  if entry == nil then return end
  if type(entry.chat_id) == "string" then
    chat_to_firing[entry.chat_id] = nil
    -- Drop the stream-hidden registration so the chat_id doesn't leak
    -- across firings. Cheap (single map delete); skipping it would
    -- mean the agentic-loop's chat_id_stream_explicitly_hidden table
    -- grows monotonically across firings.
    al().unregister_chat_stream_hidden(entry.chat_id)
  end
  if type(entry.pending_tools) == "table" then
    for tool_id, _ in pairs(entry.pending_tools) do
      tool_to_firing[tool_id] = nil
    end
  end
  agents[firing_id] = nil
end

local function send_terminal_ok(firing_id, text, structured)
  local entry = agents[firing_id]
  local chat_id = entry and entry.chat_id or nil
  local result = {
    text       = text or "",
    next_state = { chat_id = chat_id },
  }
  if structured ~= nil then
    result.structured = structured
  end
  emit_as("agent", nil, {
    kind   = "tool.result",
    id     = firing_id,
    result = result,
  })
  clear_firing(firing_id)
end

local function send_terminal_err(firing_id, err)
  emit_as("agent", nil, {
    kind  = "tool.result",
    id    = firing_id,
    error = tostring(err),
  })
  clear_firing(firing_id)
end

-- Emit `<provider>.chat.complete` to start the next turn. The
-- `extra_tools` field carries the synthetic `finalize` schema so the
-- model sees `finalize` in its advertised tool list — the
-- openai-provider binary appends `extra_tools` to the catalog-derived
-- tools array before assembling the upstream HTTP request. Without
-- this the chat.create.tools list (the per-firing advertised set on
-- the wire) is parsed as a bool by the binary and the catalog is the
-- only tool source; the model would never learn `finalize` is an
-- option.
local function emit_chat_complete(entry)
  emit(entry.provider, {
    kind        = entry.provider .. ".chat.complete",
    chat_id     = entry.chat_id,
    extra_tools = { FINALIZE_SCHEMA },
  })
end

-- Append a single message to the chat.
local function emit_chat_append(entry, message)
  emit(entry.provider, {
    kind    = entry.provider .. ".chat.append",
    chat_id = entry.chat_id,
    message = message,
  })
end

-- Dispatch handler — entry from reasoners/init.lua.
--
-- body shape (post unwrap_invoke_body):
--   { run_id, node_id, firing_id, args, inputs, prev_state }
--
-- Returns:
--   nil       — handler accepted; reply will land later via the bus
--   "_already_replied" — reasoners/init.lua skips its err path
--   <string>  — synth-fail with this error string
local function handle(body)
  local firing_id = body.firing_id
  local args = body.args
  if type(args) ~= "table" then
    return "agent reasoner: missing args"
  end

  local system_prompt = args.system_prompt
  local prompt        = args.prompt
  local additional    = args.additional_context

  if type(prompt) ~= "string" or #prompt == 0 then
    return "agent reasoner: args.prompt must be a non-empty string"
  end

  local cfg = al().config()
  local provider = (type(args.provider) == "string" and args.provider) or cfg.provider
  -- Inherit the orchestrator's model when the per-node spec doesn't
  -- pin one. Without this fallback, sub-agent chat.create envelopes
  -- omit `model`, and the provider binary falls back to its own
  -- baked-in default (gpt-5-codex for chatgpt-provider) — which is
  -- rejected for ChatGPT-subscription accounts. The user's
  -- /model picker writes `cfg.model`, so the lead's choice naturally
  -- propagates to every node. Mirrors provider-wrapper's resolution
  -- order in reasoners/init.lua:127.
  local model = (type(args.model) == "string" and #args.model > 0 and args.model)
              or cfg.model
  if type(provider) ~= "string" or #provider == 0 then
    return "agent reasoner: no provider configured (set args.provider or config.provider)"
  end

  -- First-firing only path. The agent reasoner runs its full turn-cycle
  -- inline via module-level state + bus subscriptions; reasoner-graph
  -- never re-fires the node, so prev_state is always nil here.
  local chat_id = next_id("chat")

  local entry = {
    firing_id      = firing_id,
    run_id         = body.run_id,
    node_id        = body.node_id,
    chat_id        = chat_id,
    provider       = provider,
    tool_allowlist = build_allowlist_set(args.tool_allowlist),
    cwd            = type(args.cwd) == "string" and args.cwd or nil,
    pending_tools  = {},
    pending_order  = {},
    pending_count  = 0,
  }
  agents[firing_id] = entry
  chat_to_firing[chat_id] = firing_id

  -- Bind chat_id ↔ (run_id, node_id) for the chat surface. Sub-agent
  -- emissions that ride on chat_id (notably the AGENTS.md auto-load
  -- system message from tool-gate) are routed by the chat reducer to
  -- the matching DAG node's "last tool" slot rather than the main
  -- transcript. Without this binding the chat surface has no way to
  -- know whether a chat_id refers to the lead's chat (render) or a
  -- sub-agent's chat (route to sidebar).
  if type(body.run_id) == "string" and type(body.node_id) == "string"
      and #body.run_id > 0 and #body.node_id > 0 then
    emit_as("agent", nil, {
      kind    = "graph.node.chat.bound",
      run_id  = body.run_id,
      node_id = body.node_id,
      chat_id = chat_id,
    })
  end

  -- Register the agent's chat_id as stream-hidden so the
  -- openai-provider wrapper's gate suppresses the sub-agent's
  -- `<provider>.stream.delta` events from translating into
  -- `chat.stream.delta` (which the chat surface renders). Without
  -- this the user sees a noisy stream of every sub-agent's reasoning
  -- interleaved with the lead's response. The lead's own chat_id is
  -- NOT registered here — it goes through `track_provider_firing` as
  -- "provider-wrapper" which is in the STREAM_VISIBLE_TYPES set.
  al().register_chat_stream_hidden(chat_id)

  -- chat.create. The provider binary's tool-advertisement set rides on
  -- `tools` here (per provider-wrapper's existing seed). The agent
  -- reasoner ALSO enforces per-call in-reasoner (§4 of the spec) so an
  -- adversarial provider that ignores the advertised set still can't
  -- breach the role's tool sandbox.
  local create_body = {
    kind    = provider .. ".chat.create",
    chat_id = chat_id,
  }
  if type(model) == "string" and #model > 0 then
    create_body.model = model
  end
  -- Advertise the role's allowlist + the synthetic `finalize`
  -- terminator. `finalize` rides on every agent firing regardless of
  -- the caller's allowlist; it's intercepted here, not routed through
  -- tool-gate.
  if type(args.tool_allowlist) == "table" then
    local advertised = {}
    for _, n in ipairs(args.tool_allowlist) do advertised[#advertised + 1] = n end
    advertised[#advertised + 1] = FINALIZE_NAME
    create_body.tools = advertised
  end
  emit(provider, create_body)

  -- system message: system_prompt + optional additional_context
  if type(system_prompt) == "string" and #system_prompt > 0 then
    local sys = system_prompt
    if type(additional) == "string" and #additional > 0 then
      sys = sys .. "\n\n" .. additional
    end
    emit_chat_append(entry, { role = "system", content = sys })
  elseif type(additional) == "string" and #additional > 0 then
    emit_chat_append(entry, { role = "system", content = additional })
  end

  -- user message: prompt first, then upstream graph-node outputs (when
  -- this node has dependencies), separated by `---`. Append (not prepend)
  -- so the freshest upstream data sits last in the model's context —
  -- the model reads the task instruction first, then the data it must
  -- act on. Top-level nodes have empty inputs and the block collapses
  -- to nothing.
  local inputs_block = build_inputs_block(body.inputs)
  local user_content = prompt
  if #inputs_block > 0 then
    user_content = prompt .. "\n\n---\n\n" .. inputs_block
  end
  emit_chat_append(entry, { role = "user", content = user_content })

  -- kick off the first turn
  emit_chat_complete(entry)

  return nil  -- response arrives later via on_chat_complete_result
end

M.handle = handle

-- Dispatch a single provider tool_call. Returns:
--   true  — dispatched (or synthesised local result for disallowed)
--   false — tool_call malformed; caller should record an error
local function dispatch_tool_call(entry, call)
  if type(call) ~= "table" then return false end
  local name = call.name or call.tool
  if type(name) ~= "string" or #name == 0 then return false end
  local call_args = call.arguments or call.args or {}
  local model_call_id = call.id

  local tool_id = next_id("tool")
  entry.pending_tools[tool_id] = {
    call_id  = model_call_id or tool_id,
    name     = name,
    received = false,
  }
  entry.pending_order[#entry.pending_order + 1] = tool_id
  entry.pending_count = entry.pending_count + 1
  tool_to_firing[tool_id] = entry.firing_id

  -- In-reasoner allowlist enforcement (§4): synthesise a local error
  -- result for tools outside the allowlist. The result still flows
  -- through the chat-history append loop below so the model sees its
  -- own attempt was rejected and can adapt.
  if entry.tool_allowlist ~= nil and not entry.tool_allowlist[name] then
    local pt = entry.pending_tools[tool_id]
    pt.received = true
    pt.error = "Tool '" .. name .. "' not in allowlist for this agent"
    entry.pending_count = entry.pending_count - 1
    return true
  end

  if entry.cwd and type(call_args) == "table" and call_args.cwd == nil then
    if name == "bash" or name == "write_file" or name == "read_file" then
      call_args = call_args
      call_args.cwd = entry.cwd
    end
  end

  emit("tool-gate", {
    kind    = "tool-gate.tool.invoke",
    id      = tool_id,
    name    = name,
    args    = call_args,
    chat_id = entry.chat_id,
  })

  -- Paired observer envelope so the chat surface can show "agent in
  -- node X is currently calling tool Y" in the DAG sidebar. Only emit
  -- when the agent is running inside a graph node (run_id + node_id
  -- both present); standalone agent firings have no node to attach to.
  if type(entry.run_id) == "string" and type(entry.node_id) == "string"
      and #entry.run_id > 0 and #entry.node_id > 0 then
    emit_as("agent", nil, {
      kind      = "graph.node.tool.invoke",
      run_id    = entry.run_id,
      node_id   = entry.node_id,
      tool_id   = tool_id,
      tool_name = name,
      -- Args ride alongside so the DAG sidebar can render
      -- `bash(grep …)` instead of bare `bash` — distinguishes
      -- parallel agents that all happen to use the same tool.
      tool_args = call_args,
    })
  end
  return true
end

-- Forward-declared so on_chat_complete_result can reference it; the
-- definition lives below.
local finish_turn

-- Pull the `finalize` call out of a tool_calls list, if present.
-- Returns the matching call or nil. When multiple finalize calls
-- arrive in the same response (degenerate case), the first wins.
local function find_finalize_call(tool_calls)
  if type(tool_calls) ~= "table" then return nil end
  for _, call in ipairs(tool_calls) do
    if type(call) == "table" then
      local name = call.name or call.tool
      if name == FINALIZE_NAME then return call end
    end
  end
  return nil
end

-- Build the terminal payload from a finalize tool_call.
--   text       = args.answer  (or a placeholder if missing/empty)
--   structured = the full args object verbatim
--
-- A non-table args slot (some providers stream "{}" as a string;
-- normally already JSON-decoded by the provider — see openai-provider
-- main.rs:1460) is treated as empty. We never error on a malformed
-- finalize: the contract is "terminate", and synthesising a
-- placeholder is strictly more useful than re-firing chat.complete.
local function payload_from_finalize(call)
  local raw = call.arguments or call.args
  local args
  if type(raw) == "table" then
    args = raw
  elseif type(raw) == "string" then
    local ok, decoded = pcall(json.decode, raw)
    if ok and type(decoded) == "table" then
      args = decoded
    else
      args = {}
    end
  else
    args = {}
  end

  local answer = args.answer
  local text
  if type(answer) == "string" and #answer > 0 then
    text = answer
  else
    text = "[finalize: no answer provided]"
  end
  return text, args
end

-- Provider-reply handler. The wire shape is the same as
-- `chat_complete_result_body` in openai-provider:
--   { chat_id, output: { text, tool_calls?, finish_reason?, ... } }
local function on_chat_complete_result(body)
  local chat_id = body.chat_id
  if type(chat_id) ~= "string" then return end
  local firing_id = chat_to_firing[chat_id]
  if firing_id == nil then return end
  local entry = agents[firing_id]
  if entry == nil then return end

  local out = body.output
  if type(out) ~= "table" then
    send_terminal_err(firing_id, "agent reasoner: provider returned non-object output")
    return
  end

  local tool_calls = out.tool_calls
  local has_calls = type(tool_calls) == "table" and #tool_calls > 0

  -- Finalize wins over any other tool_call in the same response. The
  -- terminator runs BEFORE allowlist enforcement / tool-gate dispatch
  -- so it's never blocked, never reaches a real tool plugin, and any
  -- sibling tool_calls in the same turn (e.g. the model emitted
  -- bash + finalize together) are dropped.
  if has_calls then
    local fin = find_finalize_call(tool_calls)
    if fin ~= nil then
      local text, structured = payload_from_finalize(fin)
      send_terminal_ok(firing_id, text, structured)
      return
    end
  end

  if not has_calls then
    -- Terminal: text-only reply ends the agent loop.
    send_terminal_ok(firing_id, out.text)
    return
  end

  -- Reset per-turn pending state and dispatch each tool call.
  entry.pending_tools = {}
  entry.pending_order = {}
  entry.pending_count = 0

  for _, call in ipairs(tool_calls) do
    if not dispatch_tool_call(entry, call) then
      -- Malformed call — synth an error placeholder so the loop still
      -- progresses (the model gets a tool-result entry telling it the
      -- call shape was invalid).
      local tool_id = next_id("tool")
      entry.pending_tools[tool_id] = {
        call_id  = "(invalid)",
        name     = "(invalid)",
        received = true,
        error    = "agent reasoner: provider emitted malformed tool_call",
      }
      entry.pending_order[#entry.pending_order + 1] = tool_id
      tool_to_firing[tool_id] = entry.firing_id
    end
  end

  -- Allowlist-blocked / malformed calls land synchronously in
  -- pending_tools with received=true; if every call was rejected
  -- locally, advance the loop now.
  if entry.pending_count == 0 then
    finish_turn(entry)
  end
end

-- Defined below the forward-declare above.
finish_turn = function(entry)
  -- Append each tool result to chat history in dispatch order, then
  -- kick off the next provider turn.
  for _, tool_id in ipairs(entry.pending_order) do
    local pt = entry.pending_tools[tool_id]
    if pt ~= nil then
      local content
      if type(pt.error) == "string" and pt.error ~= "" then
        content = "[tool error] " .. pt.error
      elseif type(pt.result_text) == "string" then
        content = pt.result_text
      else
        content = ""
      end
      emit_chat_append(entry, {
        role         = "tool",
        content      = content,
        tool_call_id = pt.call_id,
      })
    end
  end
  -- Reset the per-turn pending state and re-fire chat.complete.
  entry.pending_tools = {}
  entry.pending_order = {}
  entry.pending_count = 0
  emit_chat_complete(entry)
end

-- Tool-result handler. Wire shape:
--   tool.result { id=<our tool_id>, output=<string|table>, error?=<string> }
local function on_tool_result(body)
  local tool_id = body.id
  if type(tool_id) ~= "string" then return end
  local firing_id = tool_to_firing[tool_id]
  if firing_id == nil then return end
  local entry = agents[firing_id]
  if entry == nil then return end
  local pt = entry.pending_tools[tool_id]
  if pt == nil or pt.received then return end

  pt.received = true
  if type(body.error) == "string" and #body.error > 0 then
    pt.error = body.error
  elseif body.error == true then
    pt.error = "tool failed (no message)"
  end
  if type(body.output) == "string" then
    pt.result_text = body.output
  elseif body.output ~= nil then
    pt.result_text = json.encode(body.output)
  end

  -- Drop the tool_id mapping — no further results expected for this id.
  tool_to_firing[tool_id] = nil
  entry.pending_count = entry.pending_count - 1
  if entry.pending_count <= 0 then
    finish_turn(entry)
  end
end

-- Provider-error handler. Wire shape:
--   <provider>.chat.error { chat_id, message }
-- Closes the firing with an error.
local function on_chat_error(body)
  local chat_id = body.chat_id
  if type(chat_id) ~= "string" then return end
  local firing_id = chat_to_firing[chat_id]
  if firing_id == nil then return end
  send_terminal_err(firing_id, body.message or "provider error")
end

-- Sub-graph cancel propagation. Companion to the chat-side cancel_all
-- → <provider>.interrupt path: when lead-workflow cancels the
-- enclosing graph (session_end / user /quit mid-run), in-flight
-- `<provider>.chat.complete` streams under our firing keep producing
-- tokens unless we tear them down explicitly. Walk every firing under
-- the cancelled run_id and:
--   1. emit `<provider>.interrupt { chat_id }` so the provider binary
--      closes the streaming HTTP call (mock honours chat_id; openai's
--      bare `interrupt` is chat-agnostic — a separate gap tracked
--      against the openai binary, NOT this fix).
--   2. emit a terminal `tool.result { error }` for the firing so
--      reasoner-graph's `firing_by_request_id` gets cleaned up the same
--      way a provider error close would. Idempotent: a firing that's
--      already terminated has no entry, so the cancel is a no-op for it.
--
-- Wire shape: graph.cancel { run_id }. Lead-workflow emits this as a
-- BROADCAST (not targeted at reasoner-graph) so the in-VM bus surfaces
-- it to every actor including us — see lead-workflow/init.lua's
-- terminate_active_graph.
local function on_graph_cancel(body)
  local run_id = body.run_id
  if type(run_id) ~= "string" or #run_id == 0 then return end

  -- Snapshot the matching firings before we mutate. clear_firing inside
  -- send_terminal_err deletes from `agents`; iterating it directly under
  -- mutation is undefined in Lua.
  local victims = {}
  for firing_id, entry in pairs(agents) do
    if entry.run_id == run_id then
      victims[#victims + 1] = { firing_id = firing_id, entry = entry }
    end
  end

  for _, v in ipairs(victims) do
    local entry = v.entry
    -- 1. interrupt the provider stream (per-chat). Mock honours chat_id;
    -- openai-provider currently fanouts to all chats.
    emit(entry.provider, {
      kind    = entry.provider .. ".interrupt",
      chat_id = entry.chat_id,
    })
    -- 2. close the firing with a terminal error so the scheduler
    -- de-registers it.
    send_terminal_err(v.firing_id, "[Graph cancelled by user]")
  end
end

-- receive_msg — bus subscriber for provider replies + tool results.
--
-- Called from reasoners/init.lua's receive_msg before its tool.invoke
-- dispatch path. We watch for the bus envelopes that carry per-turn
-- progress (provider replies, tool results, provider errors) targeting
-- our tracked chat_ids / tool_ids and advance the loop. Anything else
-- is ignored.
local function receive_msg(entry)
  -- Skip per-peer broadcast fan-out copies (matches the filter in
  -- reasoners/init.lua and agentic-loop/init.lua).
  if entry.origin == "step" and entry.target ~= nil then return end

  local ok, decoded = pcall(json.decode, entry.payload)
  if not ok then return end

  -- Skip during replay — the agent reasoner's per-firing state lives
  -- in module-level tables that don't survive a process restart, so
  -- replayed envelopes have nothing to advance.
  if replay_window.active() then return end

  local body = decoded.body
  local kind = body.kind

  -- graph.cancel handler — sub-graph cancel propagation. The
  -- lead-workflow actor broadcasts `graph.cancel { run_id }` on
  -- session_end / user-quit; we tear down any of OUR firings under the
  -- cancelled run by interrupting the provider stream + emitting a
  -- terminal tool.result. Idempotent: firings already terminated have
  -- no entry in `agents` and are skipped by on_graph_cancel.
  if kind == "graph.cancel" then
    on_graph_cancel(body)
    return
  end

  -- chat.message.append { role = "system" } fold (lead-workflow-spec
  -- §5 follow-up): tool-gate's smart AGENTS.md loader emits these
  -- envelopes when an inner tool call touches a path under an
  -- AGENTS.md-bearing dir. The envelope is TUI/persistence-shaped —
  -- nothing translates it into provider chat history by default, so
  -- the model never sees the AGENTS.md content. The agent reasoner
  -- folds every system-role chat.message.append into a
  -- <provider>.chat.append for each of OUR active firings so the
  -- model picks up the context on its next turn. role=user / role=
  -- assistant are NOT folded — user input rides through the chat-
  -- runner / agentic-loop's normal path and assistant content comes
  -- from the provider itself; double-folding either would corrupt
  -- history. Skipped when no firing is active so the AGENTS.md emit
  -- doesn't leak when the agent reasoner isn't the one using
  -- tool-gate.
  if kind == "chat.message.append" and body.role == "system" then
    local text = body.text or body.content
    if type(text) ~= "string" or #text == 0 then return end
    -- Scope to the matching agent when chat_id is present; fall back
    -- to fan-out for legacy envelopes without chat_id.
    local target_chat = body.chat_id
    if type(target_chat) == "string" and #target_chat > 0 then
      local fid = chat_to_firing[target_chat]
      local fentry = fid and agents[fid]
      if fentry then
        emit_chat_append(fentry, { role = "user", content = text })
      end
    else
      for _, fentry in pairs(agents) do
        emit_chat_append(fentry, { role = "user", content = text })
      end
    end
    return
  end

  -- tool.result envelopes targeting one of our tool_ids advance the
  -- per-turn loop. Everything else (run-close tool.results owned by
  -- agentic-loop, sub-graph synth replies, real-tool returns destined
  -- for OTHER firings) is skipped because tool_to_firing keys lookup.
  if kind == "tool.result" then
    on_tool_result(body)
    return
  end

  -- <provider>.chat.complete.result — provider replied. Match by
  -- chat_id; non-tracked chat_ids are silently skipped.
  -- We can't gate on a fixed prefix because the provider name is
  -- per-firing; instead the chat_to_firing map is the discriminator
  -- (only chat_ids we minted are in it).
  local chat_id = body.chat_id
  if type(chat_id) == "string" and chat_to_firing[chat_id] ~= nil then
    -- pattern: "<provider>.chat.complete.result" or
    --         "<provider>.chat.error"
    -- match by suffix.
    if string.sub(kind, -#".chat.complete.result") == ".chat.complete.result" then
      on_chat_complete_result(body)
      return
    end
    if string.sub(kind, -#".chat.error") == ".chat.error" then
      on_chat_error(body)
      return
    end
  end
end

M.receive_msg = receive_msg

M._internals = {
  agents          = agents,
  chat_to_firing  = chat_to_firing,
  tool_to_firing  = tool_to_firing,
  reset = function()
    for k, _ in pairs(agents)         do agents[k]         = nil end
    for k, _ in pairs(chat_to_firing) do chat_to_firing[k] = nil end
    for k, _ in pairs(tool_to_firing) do tool_to_firing[k] = nil end
  end,
  -- Synchronous test driver: feed wire-shaped bodies directly.
  on_chat_complete_result = on_chat_complete_result,
  on_tool_result          = on_tool_result,
  on_chat_error           = on_chat_error,
  on_graph_cancel         = on_graph_cancel,
}

return M
