-- starter/mock_provider.lua — script for mock-plugin to impersonate an
-- openai-provider for deterministic smoke testing of the spawn_graph
-- pipeline.
--
-- Speaks the same wire shape as openai-provider:
--   <name>.chat.create  { chat_id, model? }
--   <name>.chat.append  { chat_id, message: { role, content, ... } }
--   <name>.chat.complete { chat_id }
-- responds with:
--   <name>.stream.delta { id, chat_id, text }   (one or more)
--   <name>.stream.end   { id, chat_id, text, model, duration_ms, finish_reason? }
--   <name>.chat.complete.result { chat_id, output: ProviderOut }
--
-- ProviderOut shape (matching openai-provider's chat_complete_result_body):
--   { text, tool_calls?: [{id, name, arguments: object}], finish_reason?, usage }
--
-- ### Response selection
--
-- For each chat.complete, look at the chat's history and pick a canned
-- response by pattern. The pipeline should drive this sequence:
--
-- 1. Orchestrator turn: user asks about octopuses + lighthouses + parallel.
--    No tool result yet → respond with a `spawn_graph` tool call carrying
--    a 4-node graph (sx, sy, combine, terminal).
-- 2. Sub-graph node `sx`: prompt about octopuses → text response.
-- 3. Sub-graph node `sy`: prompt about lighthouses → text response.
-- 4. Sub-graph node `combine`: history holds upstream summaries as user
--    messages plus the combine instruction → return combined paragraph.
-- 5. Orchestrator's wrap node fires again with the spawn_graph tool
--    result in history → relay text response.
--
-- ### Why a Lua script and not a new Rust plugin
--
-- mock-plugin is already a scriptable NCP peer; reusing it costs a Lua
-- file instead of a fresh crate. The wire shape we need to emit is
-- mechanical (no real model in the loop), so all the work is pattern
-- matching + canned strings.

local NAME = nefor.name -- "mock-plugin"

-- per-chat history: chat_id -> array of {role, content, tool_call_id?, tool_calls?}
local chats = {}

-- The graph the orchestrator-turn responds with via spawn_graph.
-- Encoded as a Lua table; mock-plugin serialises nested tables to JSON.
-- IMPORTANT: rg_adapter expects `arguments` to be a JSON object on the
-- chat.complete.result wire (openai-provider de-nests it before emit;
-- we emit the de-nested shape directly).
local CANNED_GRAPH = {
  nodes = {
    { id = "sx",       reasoner = "responder", args = { prompt = "Summarise octopuses in one sentence." } },
    { id = "sy",       reasoner = "responder", args = { prompt = "Summarise lighthouses in one sentence." } },
    { id = "combine",  reasoner = "responder", args = { prompt = "Combine the two summaries above into one paragraph." } },
    { id = "terminal", reasoner = "terminal",  args = {} },
  },
  edges = {
    { from = "sx",      to = "combine"  },
    { from = "sy",      to = "combine"  },
    { from = "combine", to = "terminal" },
  },
}

-- Canned text responses keyed by pattern in the last user message.
-- Order matters: more specific patterns must come before general ones.
local CANNED_TEXT = {
  -- Combiner sees three user messages: octopus summary, lighthouse summary,
  -- and the explicit "Combine..." instruction. Match the instruction first.
  { pattern = "[Cc]ombine.*paragraph",
    text = "Octopuses, with their remarkable intelligence and adaptive camouflage, share an unlikely kinship with the steadfast lighthouse — both serve as vigilant sentinels of their respective worlds, the cephalopod beneath the waves and the beacon above them, each watchful in its solitary post." },
  { pattern = "[Ss]ummarise octopuses",
    text = "Octopuses are highly intelligent invertebrate cephalopods known for problem-solving, dynamic camouflage, and eight prehensile arms lined with chemosensitive suckers." },
  { pattern = "[Ss]ummarise lighthouses",
    text = "Lighthouses are tall coastal towers crowned with bright rotating beams that guide ships safely past hazards and into harbours, dating back to the Pharos of Alexandria." },
}

-- After spawn_graph returns its serialised result, the orchestrator's
-- wrap node fires again with a "tool" message carrying that text. We
-- relay it as the assistant's final answer.
local FINAL_RELAY_PREFIX = ""

-- Async spawn_graph (post-2026-04-30): the immediate `tool.result` is
-- just an ack ("Submitted sub-graph run_id=..."). The real result
-- arrives later as a USER-role message starting with
-- "[spawn_graph(run_id=...) result]". Pattern-match on that prefix in
-- the latest user message to drive the relay turn — old behaviour
-- (relaying last_tool when no deferred user message exists) still
-- covers the synchronous case if anything reverts.
--
-- The marker shape comes from agentic_workflow.format_deferred (see
-- starter/agentic_workflow.lua); it changed during the Phase-1B
-- consolidation. The legacy "[Deferred result for spawn_graph" prefix
-- is also accepted so older fixtures keep working.
local DEFERRED_RESULT_MARKER = "%[spawn_graph%(run_id="
local DEFERRED_LEGACY_MARKER = "%[Deferred result for spawn_graph"
local DEFERRED_FAILURE_MARKER = "%[spawn_graph%(run_id=[^)]*%) FAILED%]"
local DEFERRED_FAILURE_LEGACY = "%[Deferred FAILURE for spawn_graph"
local SUBMITTED_ACK_MARKER = "Submitted sub%-graph run_id="

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

local function pick_response_for(chat_id)
  local history = chats[chat_id] or {}

  -- Find the most recent user message and detect whether a tool message
  -- has landed (chat-orchestrator wrap node, second turn).
  local last_user
  local last_tool
  for i = #history, 1, -1 do
    local m = history[i]
    if m.role == "tool" and not last_tool then last_tool = m.content end
    if m.role == "user" and not last_user then last_user = m.content end
  end

  -- Deferred-result branch (async spawn_graph): the real result was
  -- injected as a user-role message starting with
  -- "[spawn_graph(run_id=...) result]". Relay the content (everything
  -- after the marker line) as the final answer. Two marker shapes
  -- accepted: the current agentic_workflow form, and the legacy form
  -- "[Deferred result for spawn_graph" for older fixtures.
  if type(last_user) == "string"
      and (string.find(last_user, DEFERRED_RESULT_MARKER)
        or string.find(last_user, DEFERRED_LEGACY_MARKER)) then
    -- Strip the leading marker line; what remains is the actual
    -- combined paragraph the model should relay. agentic_workflow
    -- emits a long `--- output ---` framing block; pull just the body.
    local body = string.match(last_user, "%-%-%- output %-%-%-\n(.*)$")
    if body == nil then
      body = string.match(last_user, "^%[Deferred result for spawn_graph%([^)]*%)%]\n(.*)$")
    end
    return {
      text = FINAL_RELAY_PREFIX .. tostring(body or last_user),
      finish_reason = "stop",
      with_reasoning = true,
    }
  end
  if type(last_user) == "string"
      and (string.find(last_user, DEFERRED_FAILURE_MARKER)
        or string.find(last_user, DEFERRED_FAILURE_LEGACY)) then
    return {
      text = "The spawned sub-graph failed: " .. tostring(last_user),
      finish_reason = "stop",
    }
  end

  -- Async ack branch: the only "tool" message in history is the
  -- spawn_graph immediate ack ("Submitted sub-graph run_id=..."). We
  -- can't relay that to the user as a final answer — the real result
  -- hasn't arrived yet. Emit a short transitional acknowledgement so
  -- the orchestrator graph terminates and the chat unblocks.
  if last_tool ~= nil and string.find(tostring(last_tool), SUBMITTED_ACK_MARKER) then
    return {
      text = "Started the sub-graph; I'll relay the results when they arrive.",
      finish_reason = "stop",
    }
  end

  -- Legacy / synchronous-style fallback: relay the tool result text as
  -- the final answer. Kept for safety if anything reverts spawn_graph
  -- to synchronous semantics.
  if last_tool ~= nil then
    return {
      text = FINAL_RELAY_PREFIX .. tostring(last_tool),
      finish_reason = "stop",
    }
  end

  if type(last_user) ~= "string" then
    return { text = "[mock provider: no user message]", finish_reason = "stop" }
  end

  -- Orchestrator first turn — emit spawn_graph tool call.
  if string.find(last_user, "octopus") and string.find(last_user, "lighthouse")
      and (string.find(last_user, "parallel") or string.find(last_user, "[Cc]ombine") or string.find(last_user, "spawn_graph")) then
    return {
      text = "",
      finish_reason = "tool_calls",
      tool_calls = {
        {
          id        = "call_mock_spawn_graph",
          name      = "spawn_graph",
          -- arguments is a JSON OBJECT in the openai-provider's
          -- de-nested wire shape (see plugins/openai-provider/src/
          -- main.rs:1233-1238 for the parse). rg_adapter forwards
          -- this verbatim to tool_split which routes by tool_calls
          -- presence; tool-executor reads `arguments` as the call's
          -- parameter map.
          arguments = { graph = CANNED_GRAPH },
        },
      },
    }
  end

  -- Sub-graph nodes — match against canned text patterns in registration order.
  for _, entry in ipairs(CANNED_TEXT) do
    if string.find(last_user, entry.pattern) then
      return { text = entry.text, finish_reason = "stop" }
    end
  end

  return {
    text = "[mock provider: no canned match for: " .. string.sub(last_user, 1, 60) .. "]",
    finish_reason = "stop",
  }
end

-- Canned reasoning chunks emitted ahead of content for the orchestrator's
-- relay turn. Five chunks, deterministic — this is what the user sees as
-- the live "thinking" preview before it collapses on first content delta.
-- For sub-graph chats, rg_adapter's gate drops these silently.
local CANNED_REASONING_CHUNKS = {
  "Reading the deferred sub-graph result.\n",
  "It carries a single combined paragraph already, so",
  " I don't need to recompose anything — relaying it",
  " verbatim is the right call.\n",
  "Producing the final answer now.",
}

local function emit_reasoning(chat_id, id)
  -- Emit reasoning chunks ahead of the content stream, then a
  -- reasoning_end carrying the full accumulated text. Mirrors what
  -- openai-provider does on a real Qwen 3 turn.
  local full = ""
  for _, chunk in ipairs(CANNED_REASONING_CHUNKS) do
    full = full .. chunk
    nefor.emit("stream.reasoning_delta", {
      id      = id,
      chat_id = chat_id,
      text    = chunk,
    })
  end
  nefor.emit("stream.reasoning_end", {
    id          = id,
    chat_id     = chat_id,
    text        = full,
    duration_ms = 250,
  })
end

local function emit_stream(chat_id, text, opts)
  if type(text) ~= "string" or #text == 0 then return end
  opts = opts or {}
  local id = "resp-" .. chat_id

  if opts.with_reasoning then
    emit_reasoning(chat_id, id)
  end

  -- Three roughly-equal chunks for a more realistic streaming feel.
  -- The wrap-node chat is the only one that surfaces these to the
  -- user (rg_adapter gates non-wrap streams), so this only matters
  -- for the orchestrator's relay turn — but emitting them
  -- unconditionally keeps the mock simple and rg_adapter's gate
  -- handles the rest.
  local n = math.max(1, math.floor(#text / 3))
  local i = 1
  while i <= #text do
    local stop = math.min(i + n - 1, #text)
    nefor.emit("stream.delta", {
      id      = id,
      chat_id = chat_id,
      text    = string.sub(text, i, stop),
    })
    i = stop + 1
  end
  nefor.emit("stream.end", {
    id            = id,
    chat_id       = chat_id,
    text          = text,
    model         = "mock-model",
    duration_ms   = 0,
  })
end

-- ------------------------------------------------------------------
-- handlers
-- ------------------------------------------------------------------

nefor.on_ready_ok(function()
  -- Synthetic `<name>.hello { model = ... }` so chat_orchestrator's
  -- adapter learns the model name. Mirrors openai-provider's hello.
  nefor.emit("hello", { model = "mock-model" })
end)

nefor.on(NAME .. ".chat.create", function(body)
  local chat_id = body.chat_id
  if type(chat_id) ~= "string" then return end
  chats[chat_id] = {}
  nefor.log("chat.create chat_id=" .. chat_id)
end)

nefor.on(NAME .. ".chat.append", function(body)
  local chat_id = body.chat_id
  local message = body.message
  if type(chat_id) ~= "string" or type(message) ~= "table" then return end
  if not chats[chat_id] then chats[chat_id] = {} end
  table.insert(chats[chat_id], {
    role            = message.role,
    content         = message.content,
    tool_call_id    = message.tool_call_id,
    tool_calls      = message.tool_calls,
  })
  nefor.log(string.format(
    "chat.append chat_id=%s role=%s content_len=%d",
    chat_id,
    tostring(message.role),
    type(message.content) == "string" and #message.content or 0))
end)

nefor.on(NAME .. ".chat.complete", function(body)
  local chat_id = body.chat_id
  if type(chat_id) ~= "string" then return end

  local resp = pick_response_for(chat_id)
  nefor.log(string.format(
    "chat.complete chat_id=%s finish=%s text_len=%d tool_calls=%s",
    chat_id,
    tostring(resp.finish_reason),
    type(resp.text) == "string" and #resp.text or 0,
    resp.tool_calls and #resp.tool_calls or 0))

  -- Stream phase (only when there's text — tool-call turns skip
  -- streaming, matching openai-provider's behaviour). The
  -- `with_reasoning` flag is set on the deferred-result relay turn so
  -- the orchestrator's wrap node demonstrates the live thinking →
  -- collapse rendering path.
  if type(resp.text) == "string" and #resp.text > 0 then
    emit_stream(chat_id, resp.text, { with_reasoning = resp.with_reasoning })
  end

  -- chat.complete.result with ProviderOut shape.
  local output = {
    text          = resp.text or "",
    finish_reason = resp.finish_reason,
    usage         = {
      prompt_tokens     = 0,
      completion_tokens = type(resp.text) == "string" and #resp.text or 0,
      model             = "mock-model",
    },
  }
  if resp.tool_calls and #resp.tool_calls > 0 then
    output.tool_calls = resp.tool_calls
  end
  if resp.with_reasoning then
    -- Mirrors openai-provider's chat.complete.result.output.reasoning
    -- field — non-streaming consumers (sub-graph node outputs, audit
    -- listeners) get the full trace alongside the content.
    local full = ""
    for _, chunk in ipairs(CANNED_REASONING_CHUNKS) do full = full .. chunk end
    output.reasoning = full
  end
  nefor.emit("chat.complete.result", {
    chat_id = chat_id,
    output  = output,
  })

  -- Echo the assistant turn into our local history so subsequent
  -- chat.complete calls (cycle re-fires) see it.
  if not chats[chat_id] then chats[chat_id] = {} end
  table.insert(chats[chat_id], {
    role       = "assistant",
    content    = resp.text or "",
    tool_calls = resp.tool_calls,
  })
end)

nefor.on(NAME .. ".chat.delete", function(body)
  local chat_id = body.chat_id
  if type(chat_id) ~= "string" then return end
  chats[chat_id] = nil
end)

nefor.on(NAME .. ".reset", function()
  chats = {}
end)

-- Tool-result accumulation is handled by rg_adapter's `adapter`
-- reasoner: it translates the tool-executor's ToolResults into
-- `{role="tool", content, tool_call_id}` messages that the wrap node
-- then appends via chat.append on its next firing. So mock receives
-- the tool message through the normal chat.append path and doesn't
-- need to subscribe to broadcast `tool.result` directly.

-- The auth dance — chat_orchestrator's openai_provider_adapter expects
-- to inject a static_token via `<name>.auth.set` after seeing
-- `<name>.ready`. Mock has no auth, but we acknowledge the set so the
-- adapter doesn't think auth failed.
nefor.on(NAME .. ".auth.set", function(_body)
  nefor.emit("auth.status", { state = "ready" })
end)
