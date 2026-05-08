-- starter/reasoners/agent.lua — `agent` reasoner type.
--
-- Lead-workflow keystone (lead-workflow-spec §2 / §7-a). Composes the
-- existing `provider-wrapper` + `tool-executor` patterns inline into a
-- single self-contained reasoner that runs its own per-firing
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
--       structured = <opaque>?,    -- reserved for the `finalize` tool;
--                                  -- absent in v0.1
--       next_state = { chat_id = <string> },
--     }
--   }
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
--     4. emit <prov>.chat.append { role="user", content=prompt }
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

local envelope      = require("lib.envelope")
local replay_window = require("lib.replay_window")

local emit_as = envelope.emit_as
local emit_to = envelope.emit_to
local next_id = envelope.next_id

local M = {}

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

-- ------------------------------------------------------------------
-- per-firing state
-- ------------------------------------------------------------------
--
-- agents[firing_id] = {
--   firing_id      = string,
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

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

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
  end
  if type(entry.pending_tools) == "table" then
    for tool_id, _ in pairs(entry.pending_tools) do
      tool_to_firing[tool_id] = nil
    end
  end
  agents[firing_id] = nil
end

local function send_terminal_ok(firing_id, text)
  local entry = agents[firing_id]
  local chat_id = entry and entry.chat_id or nil
  emit_as("agent", nil, {
    kind   = "tool.result",
    id     = firing_id,
    result = {
      text       = text or "",
      next_state = { chat_id = chat_id },
    },
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

-- Emit `<provider>.chat.complete` to start the next turn.
local function emit_chat_complete(entry)
  emit_to(entry.provider, {
    kind    = entry.provider .. ".chat.complete",
    chat_id = entry.chat_id,
  })
end

-- Append a single message to the chat.
local function emit_chat_append(entry, message)
  emit_to(entry.provider, {
    kind    = entry.provider .. ".chat.append",
    chat_id = entry.chat_id,
    message = message,
  })
end

-- ------------------------------------------------------------------
-- dispatch handler — entry from reasoners/init.lua
-- ------------------------------------------------------------------
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
  local model         = args.model
  local additional    = args.additional_context

  if type(prompt) ~= "string" or #prompt == 0 then
    return "agent reasoner: args.prompt must be a non-empty string"
  end

  local cfg = al().config()
  local provider = (type(args.provider) == "string" and args.provider) or cfg.provider
  if type(provider) ~= "string" or #provider == 0 then
    return "agent reasoner: no provider configured (set args.provider or config.provider)"
  end

  -- First-firing only path. The agent reasoner runs its full turn-cycle
  -- inline via module-level state + bus subscriptions; reasoner-graph
  -- never re-fires the node, so prev_state is always nil here.
  local chat_id = next_id("chat")

  local entry = {
    firing_id      = firing_id,
    chat_id        = chat_id,
    provider       = provider,
    tool_allowlist = build_allowlist_set(args.tool_allowlist),
    pending_tools  = {},
    pending_order  = {},
    pending_count  = 0,
  }
  agents[firing_id] = entry
  chat_to_firing[chat_id] = firing_id

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
  if type(args.tool_allowlist) == "table" then
    create_body.tools = args.tool_allowlist
  end
  emit_to(provider, create_body)

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

  -- user message: the task
  emit_chat_append(entry, { role = "user", content = prompt })

  -- kick off the first turn
  emit_chat_complete(entry)

  return nil  -- response arrives later via on_chat_complete_result
end

M.handle = handle

-- ------------------------------------------------------------------
-- bus event handlers
-- ------------------------------------------------------------------

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

  emit_to("tool-gate", {
    kind = "tool-gate.tool.invoke",
    id   = tool_id,
    name = name,
    args = call_args,
  })
  return true
end

-- Forward-declared so on_chat_complete_result can reference it; the
-- definition lives below.
local finish_turn

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

-- ------------------------------------------------------------------
-- receive_msg — bus subscriber for provider replies + tool results
-- ------------------------------------------------------------------

-- Called from reasoners/init.lua's receive_msg before its tool.invoke
-- dispatch path. We watch for the bus envelopes that carry per-turn
-- progress (provider replies, tool results, provider errors) targeting
-- our tracked chat_ids / tool_ids and advance the loop. Anything else
-- is ignored.
local function receive_msg(entry)
  -- Skip per-peer broadcast fan-out copies (matches the filter in
  -- reasoners/init.lua and agentic-loop/init.lua).
  if entry.origin == "step" and entry.target ~= nil then return end

  local payload = entry.payload
  if type(payload) ~= "string" or payload == "" then return end
  local ok, decoded = pcall(json.decode, payload)
  if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end

  -- Skip during replay — the agent reasoner's per-firing state lives
  -- in module-level tables that don't survive a process restart, so
  -- replayed envelopes have nothing to advance.
  if replay_window.active() then return end

  local body = decoded.body
  local kind = body.kind
  if type(kind) ~= "string" then return end

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

-- ------------------------------------------------------------------
-- test escape hatch
-- ------------------------------------------------------------------

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
}

return M
