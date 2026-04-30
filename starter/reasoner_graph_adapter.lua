-- starter/reasoner_graph_adapter.lua — type-driven adapter for the
-- reasoner-graph plugin (renamed from dag_adapter.lua per parent spec
-- §6.1 Stage 1 bullet 8).
--
-- Bridges the scheduler's native wire shape against the underlying
-- worker plugins (openai-provider for `dummy`/`provider-wrapper`,
-- tool-gate for `tool-executor`, in-Lua for `adapter`).
--
-- ### Wire shapes
--
-- Inbound from reasoner-graph (intercepted at egress via `from_plugin`):
--   `<reasoner>.run_node { run_id, node_id, firing_id, args, inputs, prev_state }`
--
-- The adapter must reply with two events per dispatch:
--   1. `<reasoner>.run_node.ack { run_id, firing_id }` — acknowledge
--      receipt within the scheduler's `ack_deadline_ms`. Emitted
--      synchronously the moment we intercept run_node so the scheduler
--      stops the watchdog. After that the work runs at its own pace.
--   2. `graph.node_result { run_id, node_id, firing_id, output|error,
--      next_state }` — terminal status; emitted when the underlying
--      worker (openai-provider, tool-gate) finishes.
--
-- ### Why a single shared module
--
-- Each "reasoner type" needs to react to events from BOTH directions
-- (reasoner-graph dispatches in, provider/tool-gate replies out). A
-- single module that holds a `pending` map keyed by an opaque
-- correlation token (`<run_id>:<firing_id>:<chat_id-or-tool-id>`) lets
-- us register at dispatch and resolve at reply without leaking state
-- across module boundaries.
--
-- The transforms exposed:
--   * `for_reasoner_graph()`        — attach to reasoner-graph's spawn.
--                                     Intercepts `<type>.run_node`
--                                     egress, drives workers, emits ack.
--   * `for_provider(name)`          — attach to an openai-provider
--                                     spawn. Intercepts
--                                     `<prefix>.chat.complete.result`
--                                     for chats we own, translates to
--                                     `graph.node_result`.
--   * `for_tool_gate()`             — attach to tool-gate. Intercepts
--                                     `tool.result` for invocations we
--                                     own, translates to
--                                     `graph.node_result` for
--                                     tool-executor firings.
--
-- ### Reasoner types registered (Stage 1)
--
-- * `dummy`            : ProviderIn → ProviderOut. Single-shot chat
--                        with no tool catalog. State: chat_id only.
-- * `provider-wrapper` : ProviderIn → ProviderOut (state: ChatHistory).
--                        Drives a long-lived provider chat across
--                        firings. `prev_state` carries `{ chat_id }`;
--                        first firing creates the chat, subsequent
--                        firings reuse it. The provider's `tool.result`
--                        plumbing is what carries tool outputs back
--                        into history; `inputs` (received at firing
--                        2+) is the adapter node's translated
--                        ProviderIn payload.
-- * `tool-executor`    : ToolCalls → ToolResults. Iterates the tool
--                        calls in `inputs`, dispatches each to
--                        `tool-gate.tool.invoke`, collects results,
--                        replies with `{ tool_results: [...] }`.
-- * `adapter`          : ToolResults → ProviderIn. Pure Lua: wraps the
--                        tool results into a ProviderIn shape that
--                        the wrapper node feeds back into the chat as
--                        the next user-side message.
--
-- ### State key
--
-- We key pending state by `run_id .. ":" .. firing_id` (no possibility
-- of collision because firing_ids are UUIDs minted by the scheduler).
-- For the chat-bound paths we ALSO maintain a reverse map from chat_id
-- → pending key so a `chat.complete.result` carrying only the chat_id
-- can find its run. For tool-gate paths we keep a tool_id → pending key
-- map analogously.

local M = {}

local json = nefor.json

-- spawn_graph module ref. Set by `M.set_spawn_graph_module(spawn_graph)`
-- from init.lua. provider_run_node releases queued sub-graph dispatches
-- right after emitting wrap's chat.complete so async-tool ack turns
-- aren't queued behind the sub-graph nodes at the Ollama HTTP boundary.
-- Module reference (rather than direct require) so tests that don't
-- need spawn_graph can leave it nil.
local spawn_graph_module = nil

-- ------------------------------------------------------------------
-- module state
-- ------------------------------------------------------------------

-- pending[<run_id>:<firing_id>] = {
--   type, run_id, node_id, firing_id, reasoner,
--   -- For provider-backed types:
--   provider_name,
--   chat_id,
--   -- For tool-executor:
--   tool_calls,           -- list still to dispatch
--   tool_results,         -- accumulated results
--   tool_id_to_call_idx,  -- tool_id → index in tool_calls
-- }
local pending = {}

-- Reverse maps for plugin-replies that don't carry firing_id.
local chat_id_to_key = {}
local tool_id_to_key = {}

-- chat_ids whose `<prefix>.stream.{delta,end}` and `<prefix>.session.stats`
-- should be visible to nefor-chat. True only for `provider-wrapper` (the
-- orchestrator's user-facing wrap node); false for `responder`, `dummy`
-- and any other reasoner type we drive through provider chats. Without
-- this gate, sub-graph nodes' token streams leak into the chat as if
-- they were the orchestrator's reply.
local chat_id_stream_visible = {}

-- Reasoner types whose streaming should reach nefor-chat. Anything else
-- runs silently (sub-graph internals, scratch chats, etc.).
local STREAM_VISIBLE_TYPES = { ["provider-wrapper"] = true }

-- Stable-but-unique id counters for chat_ids and tool_ids minted here.
local id_counter = 0
local function next_id(prefix)
  id_counter = id_counter + 1
  return prefix .. "-" .. tostring(id_counter)
end

-- Reasoner-type → handler-fn registry. Each handler receives the
-- (parsed) run_node body and a writer table; it must register its
-- pending entry and emit zero-or-more bus events. Returns nil on
-- success or a string error to surface as `graph.node_result.error`.
local handlers = {}

-- Default provider name used by `dummy` and `provider-wrapper` if a
-- node's `args` doesn't override. Configured via M.set_default_provider().
local default_provider = "ollama"

-- Default model used when minting `chat.create`. Args.model overrides
-- per-node.
local default_model = nil

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

local function pending_key(run_id, firing_id)
  return tostring(run_id) .. ":" .. tostring(firing_id)
end

-- Build an envelope and ship it via the engine's send binding. NCP §3
-- requires `from`+`ts` on every wire envelope; the engine forwards
-- our payloads verbatim, so we stamp them ourselves. Target nil =
-- broadcast (engine fans out); a string targets one peer.
local function emit(target, body)
  local payload = json.encode({
    type = "event",
    from = "engine",
    ts   = nefor.engine.now(),
    body = body,
  })
  if target ~= nil then
    nefor.engine.send(payload, target)
  else
    -- Broadcast: send to every connected plugin. The starter has no
    -- "fanout" send binding; iterate the plugin list. The handful of
    -- plugin-emitted broadcasts (graph.node_result, tool.result) all
    -- go through this path.
    for _, peer in ipairs(nefor.engine.plugins()) do
      nefor.engine.send(payload, peer)
    end
  end
end

-- Targeted-send helper. Engine-prefix routing in ncp.lua delivers
-- "<peer>.<rest>" to that peer only when broadcast through the bus,
-- but we're calling engine.send directly so we must specify the
-- target explicitly.
local function emit_to(target, body)
  emit(target, body)
end

-- Broadcast helper. Used for `<reasoner>.run_node.ack` and
-- `graph.node_result` — the scheduler treats those as broadcast.
local function emit_broadcast(body)
  emit(nil, body)
end

local function send_ack(reasoner, run_id, firing_id)
  emit_broadcast({
    kind = reasoner .. ".run_node.ack",
    run_id = run_id,
    firing_id = firing_id,
  })
end

-- Observers registered via `M.on_node_result(cb)`. Called synchronously
-- inside `send_node_result_ok` AFTER the broadcast, so siblings (e.g.
-- chat_orchestrator) can capture node-level state (next_state, output)
-- that a `to_plugin` transform would miss — engine.send bypasses Lua
-- transforms, so observation has to happen here.
local node_result_observers = {}

local function send_node_result_ok(run_id, node_id, firing_id, output, next_state)
  local body = {
    kind = "graph.node_result",
    run_id = run_id,
    node_id = node_id,
    firing_id = firing_id,
    output = output,
  }
  if next_state ~= nil then
    body.next_state = next_state
  end
  emit_broadcast(body)
  for _, cb in ipairs(node_result_observers) do
    pcall(cb, run_id, node_id, firing_id, output, next_state)
  end
end

local function send_node_result_err(run_id, node_id, firing_id, err)
  emit_broadcast({
    kind = "graph.node_result",
    run_id = run_id,
    node_id = node_id,
    firing_id = firing_id,
    error = tostring(err),
  })
end

-- ------------------------------------------------------------------
-- handler: dummy / provider-wrapper
-- ------------------------------------------------------------------
--
-- Both types drive openai-provider's chat.create / chat.append /
-- chat.complete sequence. The split:
--   * `dummy`            — first-firing flow only; no `prev_state`,
--                          one chat per node, completes once. The
--                          input prompt is supplied via `args.prompt`
--                          OR via `inputs` aggregated as a single
--                          string (rare for a one-shot dummy).
--   * `provider-wrapper` — stateful across firings. `prev_state.chat_id`
--                          (when present) reuses the existing chat;
--                          else create a new one. `inputs` (firing 2+)
--                          carries the ProviderIn shape from the
--                          adapter node — append it as user message
--                          before completing.
--
-- ChatHistory representation in next_state: an opaque table
-- `{ chat_id = "..." }`. The provider holds the actual history; we
-- only need to remember which chat to reuse.

local function provider_run_node(reasoner_type, body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local args = body.args or {}
  local inputs = body.inputs or {}
  local prev_state = body.prev_state

  local provider = (type(args) == "table" and args.provider) or default_provider
  if type(provider) ~= "string" or provider == "" then
    return "no provider configured (set args.provider or default_provider)"
  end

  -- Resolve chat_id with three-step precedence:
  --   1. prev_state.chat_id       — cyclic re-fire (within one run).
  --   2. args.seed_chat_id        — cross-run bootstrap (chat
  --                                 orchestrator carries chat continuity
  --                                 across submits). See
  --                                 chat_orchestrator.lua for why.
  --   3. mint a fresh id.
  local chat_id
  local need_create = false
  local chat_id_source
  if type(prev_state) == "table" and type(prev_state.chat_id) == "string" then
    chat_id = prev_state.chat_id
    chat_id_source = "prev_state"
  elseif type(args) == "table" and type(args.seed_chat_id) == "string" then
    chat_id = args.seed_chat_id
    chat_id_source = "seed"
  else
    chat_id = next_id("chat")
    need_create = true
    chat_id_source = "fresh"
  end

  nefor.log.info("rg_adapter.provider_run_node: chat_id resolved", {
    reasoner = reasoner_type,
    run_id = run_id,
    node_id = node_id,
    firing_id = firing_id,
    provider = provider,
    chat_id = chat_id,
    source = chat_id_source,
    need_create = need_create,
    has_args_prompt = type(args) == "table" and type(args.prompt) == "string" and #args.prompt or 0,
    has_args_system = type(args) == "table" and type(args.system) == "string" and #args.system or 0,
    input_count = (function() local n = 0; for _ in pairs(inputs) do n = n + 1 end; return n end)(),
  })

  -- Register pending firing BEFORE emitting events so the
  -- chat.complete.result-handler can resolve the firing as soon as
  -- the provider replies. (Provider replies are async; this code path
  -- runs synchronously inside `from_plugin`.)
  local key = pending_key(run_id, firing_id)
  pending[key] = {
    type          = reasoner_type,
    run_id        = run_id,
    node_id       = node_id,
    firing_id     = firing_id,
    reasoner      = reasoner_type,
    provider_name = provider,
    chat_id       = chat_id,
  }
  chat_id_to_key[chat_id] = key
  chat_id_stream_visible[chat_id] = STREAM_VISIBLE_TYPES[reasoner_type] == true

  if need_create then
    local create_body = { kind = provider .. ".chat.create", chat_id = chat_id }
    local model = (type(args) == "table" and args.model) or default_model
    if type(model) == "string" and #model > 0 then
      create_body.model = model
    end
    -- Sub-graph responder nodes must produce text, not tool calls. The
    -- provider's tool catalog is process-global, so without this the
    -- responder LLM sees the orchestrator's `spawn_graph` / `bash` / …
    -- and tool-calls instead of writing the requested summary. Other
    -- reasoner types (provider-wrapper) keep tools on.
    if reasoner_type == "responder" then
      create_body.tools = false
    end
    nefor.log.info("rg_adapter -> provider: chat.create", {
      provider = provider,
      chat_id = chat_id,
      model = create_body.model,
      tools = create_body.tools,
    })
    emit_to(provider, create_body)
  end

  -- Decide which message(s) to append. Rules:
  --   1. First firing AND args.system → append as system role.
  --   2. inputs.<id>.output is a ProviderIn-shaped table with
  --      `messages` list → append each (preferred path; what `adapter`
  --      emits on cycle re-fire).
  --   3. inputs.<id>.output is a ProviderOut-shaped table with `text`
  --      string → append text as `user` (what an upstream `responder`
  --      / `provider-wrapper` node emits when fanned into a combiner).
  --   4. inputs.<id>.output is a plain string → append as `user`.
  --   5. First firing → append args.prompt as `user` AFTER any inputs.
  --      Combiners (inputs + prompt) see upstream summaries first then
  --      the combine instruction.
  if need_create then
    if type(args) == "table" and type(args.system) == "string" and #args.system > 0 then
      nefor.log.info("rg_adapter -> provider: chat.append (system)", {
        provider = provider,
        chat_id = chat_id,
        content_len = #args.system,
        content_preview = string.sub(args.system, 1, 80),
      })
      emit_to(provider, {
        kind    = provider .. ".chat.append",
        chat_id = chat_id,
        message = { role = "system", content = args.system },
      })
    end
  end

  for dep_id, dep_entry in pairs(inputs) do
    if type(dep_entry) == "table" and dep_entry.output ~= nil then
      local out = dep_entry.output
      if type(out) == "table" and type(out.messages) == "table" then
        for _, msg in ipairs(out.messages) do
          nefor.log.info("rg_adapter -> provider: chat.append (from input)", {
            provider = provider,
            chat_id = chat_id,
            from_dep = tostring(dep_id),
            role = type(msg) == "table" and msg.role or nil,
            content_len = type(msg) == "table" and type(msg.content) == "string" and #msg.content or 0,
          })
          emit_to(provider, {
            kind    = provider .. ".chat.append",
            chat_id = chat_id,
            message = msg,
          })
        end
      elseif type(out) == "table" and type(out.text) == "string" then
        nefor.log.info("rg_adapter -> provider: chat.append (ProviderOut text as user)", {
          provider = provider,
          chat_id = chat_id,
          from_dep = tostring(dep_id),
          content_len = #out.text,
          content_preview = string.sub(out.text, 1, 80),
        })
        emit_to(provider, {
          kind    = provider .. ".chat.append",
          chat_id = chat_id,
          message = { role = "user", content = out.text },
        })
      elseif type(out) == "string" then
        nefor.log.info("rg_adapter -> provider: chat.append (string input as user)", {
          provider = provider,
          chat_id = chat_id,
          from_dep = tostring(dep_id),
          content_len = #out,
        })
        emit_to(provider, {
          kind    = provider .. ".chat.append",
          chat_id = chat_id,
          message = { role = "user", content = out },
        })
      end
    end
  end

  -- First-firing: append `args.prompt` as a user message AFTER any
  -- input-driven messages. Three scenarios this covers:
  --   * fresh single-shot (no inputs): prompt is the only user message.
  --   * cross-run resume via seed_chat_id (no inputs but existing chat):
  --     prompt is the new turn's user message; otherwise chat.complete
  --     would run on stale history.
  --   * combiner (inputs + prompt): upstream summaries land as user
  --     messages first, then the combine instruction follows. Without
  --     this, a fan-in combiner would silently drop its prompt.
  --
  -- prev_state on first firing arrives as serde_json `null`, which
  -- nefor.json decodes via mlua to a NULL sentinel (lightuserdata),
  -- NOT Lua nil — `prev_state == nil` is false in that case. Test the
  -- positive shape instead: cycle re-fires set `prev_state` to a
  -- table (`{chat_id=...}`); anything else (NULL sentinel, missing,
  -- or non-table) means we're firing for the first time.
  local first_firing = (type(prev_state) ~= "table")
  if first_firing then
    local prompt = (type(args) == "table" and type(args.prompt) == "string") and args.prompt or ""
    if #prompt > 0 then
      nefor.log.info("rg_adapter -> provider: chat.append (prompt as user)", {
        provider = provider,
        chat_id = chat_id,
        content_len = #prompt,
        content_preview = string.sub(prompt, 1, 80),
      })
      emit_to(provider, {
        kind    = provider .. ".chat.append",
        chat_id = chat_id,
        message = { role = "user", content = prompt },
      })
    end
  end

  nefor.log.info("rg_adapter -> provider: chat.complete", {
    provider = provider,
    chat_id = chat_id,
  })
  emit_to(provider, { kind = provider .. ".chat.complete", chat_id = chat_id })
  return nil
end

handlers["dummy"] = function(body) return provider_run_node("dummy", body) end
handlers["provider-wrapper"] = function(body) return provider_run_node("provider-wrapper", body) end
-- `responder` is the spawn_graph-facing alias for `dummy`: a one-shot
-- LLM completion node with no tools, no cycles, no fanout. The rename
-- exists so the system prompt can advertise an intuitive type name to
-- the model when it constructs sub-graphs.
handlers["responder"] = function(body) return provider_run_node("responder", body) end

-- ------------------------------------------------------------------
-- handler: tool-executor
-- ------------------------------------------------------------------
--
-- Receives `inputs.<upstream>.output` — either a ProviderOut shape
-- carrying `tool_calls` (when the wrap node fans out via tool_split)
-- or a bare list `[{name, arguments, id}]`. Dispatches each via
-- `tool-gate.tool.invoke`; collects `tool.result` replies; once all
-- have arrived emits `graph.node_result { output = { tool_results } }`.

handlers["tool-executor"] = function(body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local inputs = body.inputs or {}

  -- Find the tool calls in inputs.
  local calls
  for _, dep_entry in pairs(inputs) do
    if type(dep_entry) == "table" and dep_entry.output ~= nil then
      local out = dep_entry.output
      if type(out) == "table" then
        if type(out.tool_calls) == "table" then
          calls = out.tool_calls
          break
        elseif #out > 0 then
          -- Bare list shape (what tool_split emits on the ToolCalls slot).
          calls = out
          break
        end
      end
    end
  end

  if type(calls) ~= "table" or #calls == 0 then
    return "tool-executor received no tool calls in inputs"
  end

  local key = pending_key(run_id, firing_id)
  pending[key] = {
    type          = "tool-executor",
    run_id        = run_id,
    node_id       = node_id,
    firing_id     = firing_id,
    reasoner      = "tool-executor",
    tool_calls    = calls,
    tool_results  = {},
    tool_ids      = {},  -- minted gate-side ids for correlation
    pending_count = #calls,
  }

  for i, call in ipairs(calls) do
    local tool_id = next_id("tool")
    pending[key].tool_ids[i] = tool_id
    tool_id_to_key[tool_id] = { key = key, idx = i }
    local call_name = (type(call) == "table" and (call.name or call.tool)) or ""
    local call_args = (type(call) == "table" and (call.arguments or call.args)) or {}
    -- Surface the tool call to the chat UI as a collapsible row. Use
    -- the model's `call.id` (e.g. "call_abc123") rather than the
    -- gate-minted `tool_id` so the start/end pair correlates with the
    -- assistant's tool_calls in the rendered transcript. The legacy
    -- in-provider tool loop did this directly; the reasoner-graph
    -- path now lives here so the orchestrator's tool calls render
    -- the same way regardless of which loop drove them.
    local model_call_id = (type(call) == "table" and call.id) or tool_id
    emit_to("nefor-chat", {
      kind  = "chat.tool.start",
      id    = model_call_id,
      name  = call_name,
      input = call_args,
    })
    emit_to("tool-gate", {
      kind = "tool-gate.tool.invoke",
      id   = tool_id,
      name = call_name,
      args = call_args,
    })
  end
  return nil
end

-- ------------------------------------------------------------------
-- handler: adapter (pure Lua)
-- ------------------------------------------------------------------
--
-- Translates ToolResults → ProviderIn messages. Inputs come from
-- tool-executor as `{tool_results: [{id, name, output, error}, ...]}`
-- where `id` is the original assistant-emitted tool_call.id (preserved
-- through the gate hop by tool-executor). For each result we synthesise
-- a `{role = "tool", content, tool_call_id}` message — the OpenAI-style
-- tool-result shape. The wrapper node's next firing appends these via
-- chat.append so the provider's chat history reflects the round trip.
--
-- An earlier draft of this handler emitted `messages = {}` on the
-- (false) assumption that the provider auto-accumulates broadcast
-- `tool.result` events into its chat history. openai-provider's
-- `tool.result` arm only routes to its legacy in-flight tool broker,
-- not the chat-id-keyed history that the reasoner-graph flow uses —
-- so leaving the messages empty meant the wrapper never saw the tool
-- result and kept re-emitting the same tool call (infinite spawn
-- loop). Translating here is the canonical place for it: the adapter
-- node IS the protocol bridge between ToolResults and ProviderIn.

handlers["adapter"] = function(body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local inputs = body.inputs or {}

  -- Locate the ToolResults payload. tool-executor emits its output as
  -- `{tool_results: [...]}`; tolerate alternative shapes (string output,
  -- bare list) defensively but the canonical path is the named list.
  local results
  for _, dep_entry in pairs(inputs) do
    if type(dep_entry) == "table" and type(dep_entry.output) == "table" then
      if type(dep_entry.output.tool_results) == "table" then
        results = dep_entry.output.tool_results
        break
      end
    end
  end

  local messages = {}
  if type(results) == "table" then
    for _, r in ipairs(results) do
      local content
      if type(r.output) == "string" then
        content = r.output
      elseif r.output ~= nil then
        -- Non-string outputs (objects, arrays) get JSON-encoded so the
        -- model still has something readable; rare in practice — most
        -- tool plugins return strings.
        content = json.encode(r.output)
      elseif type(r.error) == "string" then
        content = "[tool error] " .. r.error
      else
        content = ""
      end
      messages[#messages + 1] = {
        role         = "tool",
        content      = content,
        tool_call_id = r.id,
      }
    end
  end

  send_ack("adapter", run_id, firing_id)
  send_node_result_ok(run_id, node_id, firing_id, { messages = messages }, nil)
  return "_already_replied"
end

-- ------------------------------------------------------------------
-- handler: terminal (orchestrator escape edge consumer)
-- ------------------------------------------------------------------
--
-- Receives input(s) from upstream nodes. Single upstream → echo as-is
-- so `graph.run_complete.results` carries the FinalAnswer verbatim.
-- Multiple upstreams → concatenate each upstream's `output.text` with
-- a `## <upstream_id>` header. Models routinely wire parallel branches
-- straight into terminal expecting it to merge; without this, only one
-- branch survived (Lua hash-table iteration order).

handlers["terminal"] = function(body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local inputs = body.inputs or {}

  -- Collect (upstream_id, output) pairs in stable (sorted) order so
  -- repeated runs of the same graph produce identical terminal text.
  local ordered_ids = {}
  for upstream_id, dep_entry in pairs(inputs) do
    if type(dep_entry) == "table" and dep_entry.output ~= nil then
      ordered_ids[#ordered_ids + 1] = upstream_id
    end
  end
  table.sort(ordered_ids)

  local final
  if #ordered_ids == 0 then
    final = { text = "" }
  elseif #ordered_ids == 1 then
    final = inputs[ordered_ids[1]].output
  else
    local parts = {}
    for _, uid in ipairs(ordered_ids) do
      local out = inputs[uid].output
      local txt = (type(out) == "table" and out.text) or ""
      parts[#parts + 1] = "## " .. tostring(uid) .. "\n" .. tostring(txt)
    end
    final = { text = table.concat(parts, "\n\n") }
  end

  send_ack("terminal", run_id, firing_id)
  send_node_result_ok(run_id, node_id, firing_id, final, nil)
  return "_already_replied"
end

-- ------------------------------------------------------------------
-- public API: register additional types
-- ------------------------------------------------------------------
--
-- Lua-driven type registry. `name` is the reasoner-type token (matches
-- the `<type>.run_node` kind prefix). `handler_fn(body)` runs once per
-- firing; returns nil on dispatch-success, a string error to surface
-- as `graph.node_result.error`, or the sentinel string
-- `"_already_replied"` if the handler emitted ack+result itself.

function M.register_type(name, handler_fn)
  assert(type(name) == "string" and #name > 0,
         "register_type: name must be non-empty string")
  assert(type(handler_fn) == "function",
         "register_type: handler must be a function")
  handlers[name] = handler_fn
end

-- Wire the spawn_graph module so for_provider's stream.delta hook can
-- release queued sub-graph dispatches at the right moment. Optional —
-- if unset, no flush happens and spawn_graph falls back to direct
-- dispatch. See `spawn_graph_module` declaration up top for the why.
function M.set_spawn_graph_module(spawn_graph)
  assert(type(spawn_graph) == "table"
           and type(spawn_graph.flush_pending_dispatches) == "function",
         "set_spawn_graph_module: spawn_graph must expose flush_pending_dispatches")
  spawn_graph_module = spawn_graph
end

-- List the reasoner-type names this adapter currently handles. Order is
-- arbitrary (set semantics on the receiving end). Used by
-- `for_starter()` to issue `reasoner-graph.register_reasoner` events.
function M.lua_resident_types()
  local names = {}
  for name, _ in pairs(handlers) do
    names[#names + 1] = name
  end
  return names
end

-- Subscribe to every successful `graph.node_result` emission. Called
-- synchronously after the broadcast with `(run_id, node_id, firing_id,
-- output, next_state)`. Multiple subscribers stack in registration
-- order. Errors in callbacks are swallowed (a bad observer cannot
-- corrupt the run).
--
-- Why this exists: rg_adapter emits `graph.node_result` via
-- `nefor.engine.send`, which writes directly to peer connections —
-- bypassing Lua's `to_plugin`/`from_plugin` transforms. Sibling glue
-- (chat_orchestrator persisting `next_state`) needs an in-process hook
-- because the bus path doesn't visit it.
function M.on_node_result(callback)
  assert(type(callback) == "function",
         "on_node_result: callback must be a function")
  node_result_observers[#node_result_observers + 1] = callback
end

function M.set_default_provider(name, model)
  assert(type(name) == "string" and #name > 0,
         "set_default_provider: name required")
  default_provider = name
  if model ~= nil then
    assert(type(model) == "string", "set_default_provider: model must be string")
    if #model > 0 then default_model = model end
  end
end

-- ------------------------------------------------------------------
-- transform factories
-- ------------------------------------------------------------------

-- Attach to the reasoner-graph spawn. Watches for the plugin's own
-- `reasoner-graph.ready` egress and, on first sight, emits one
-- `reasoner-graph.register_reasoner { name = <type> }` per Lua-resident
-- reasoner type so the scheduler treats them as connected peers.
--
-- Lua-resident types (provider-wrapper, tool-executor, adapter,
-- terminal, dummy) never appear as wire `from` fields — they're
-- handled by `for_reasoner_graph()`'s `*.run_node` interception. The
-- scheduler's `track_peer` mechanism therefore never sees them on its
-- own; without this transform, the first dispatch synthesises a
-- "reasoner '<name>' not connected" failure.
--
-- The trigger is reasoner-graph's `ready` rather than `hello` so the
-- registrations land after the plugin has finished its handshake.
-- Idempotent on repeat: the Rust side uses a HashSet, so even if the
-- transform fires twice we don't double-register.
local function emit_register_reasoner(name)
  local payload = json.encode({
    type = "event",
    from = "engine",
    ts   = nefor.engine.now(),
    body = {
      kind = "reasoner-graph.register_reasoner",
      name = name,
    },
  })
  nefor.engine.send(payload, "reasoner-graph")
end

function M.for_starter()
  local registered = false
  local function from_plugin(env)
    if registered then return env end
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    if env.body.kind ~= "reasoner-graph.ready" then return env end

    for _, name in ipairs(M.lua_resident_types()) do
      emit_register_reasoner(name)
    end
    registered = true
    return env
  end
  return { from_plugin = from_plugin }
end

-- Attach to the reasoner-graph spawn. Intercepts `<reasoner>.run_node`
-- on egress: looks up the type's handler, dispatches it, emits ack,
-- and drops the original envelope (no other plugin needs to see the
-- run_node — it was for us). Non-`*.run_node` events pass through.
function M.for_reasoner_graph()
  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local kind = env.body.kind
    if type(kind) ~= "string" then return env end
    -- Match `<token>.run_node` exactly. The ack and result kinds use
    -- different spellings (`<token>.run_node.ack`, `graph.node_result`)
    -- so they don't match here.
    local token = kind:match("^([^.]+)%.run_node$")
    if not token then return env end

    local handler = handlers[token]
    if not handler then
      -- Unknown reasoner type. Surface a clean failure as
      -- graph.node_result and drop the run_node so the scheduler
      -- isn't waiting on a phantom ack.
      send_ack(token, env.body.run_id, env.body.firing_id)
      send_node_result_err(
        env.body.run_id,
        env.body.node_id,
        env.body.firing_id,
        "no Lua adapter for reasoner type `" .. token .. "`"
      )
      return nil
    end

    local err = handler(env.body)
    if err == "_already_replied" then
      -- Pure-Lua synchronous handler emitted both ack and result.
      return nil
    end
    if err ~= nil then
      send_ack(token, env.body.run_id, env.body.firing_id)
      send_node_result_err(
        env.body.run_id,
        env.body.node_id,
        env.body.firing_id,
        err
      )
      return nil
    end
    -- Async handler started work; emit ack so the scheduler stops
    -- the watchdog. The eventual result arrives via the provider /
    -- tool-gate transforms.
    send_ack(token, env.body.run_id, env.body.firing_id)
    return nil
  end
  return { from_plugin = from_plugin }
end

-- Attach to an openai-provider spawn (composed alongside the chat
-- adapter). Intercepts `<prefix>.chat.complete.result` and translates
-- to `graph.node_result` when the chat_id maps to a pending firing.
function M.for_provider(name)
  assert(type(name) == "string" and #name > 0,
         "for_provider: name must be non-empty string")
  local prefix = name .. "."
  local result_kind = prefix .. "chat.complete.result"
  -- Streaming-side kinds we suppress for sub-graph chats so their tokens
  -- don't leak into nefor-chat as if the orchestrator were producing them.
  -- Reasoning streams ride the same gate: a sub-graph responder's
  -- thinking trace must not appear in the orchestrator's chat.
  local stream_delta_kind = prefix .. "stream.delta"
  local stream_end_kind   = prefix .. "stream.end"
  local stream_reasoning_delta_kind = prefix .. "stream.reasoning_delta"
  local stream_reasoning_end_kind   = prefix .. "stream.reasoning_end"
  local session_stats_kind = prefix .. "session.stats"

  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local kind = env.body.kind

    -- Sub-graph stream gating: stream.delta / stream.end / session.stats
    -- AND the reasoning siblings carry chat_id; if it's one of our
    -- internal (non-wrap) chats, drop. Pass through unconditionally for
    -- chats we don't manage (someone else's chat) or for chats marked
    -- stream-visible (the orchestrator's wrap node).
    if kind == stream_delta_kind
        or kind == stream_end_kind
        or kind == stream_reasoning_delta_kind
        or kind == stream_reasoning_end_kind
        or kind == session_stats_kind then
      local chat_id = env.body.chat_id
      if type(chat_id) == "string" and chat_id_to_key[chat_id] ~= nil
          and chat_id_stream_visible[chat_id] == false then
        return nil
      end
      -- First sub-token from a stream-visible chat (the orchestrator's
      -- wrap node) means Ollama has committed to processing this
      -- request. THIS is the right moment to release any sub-graph
      -- dispatches spawn_graph queued during the prior tool firing —
      -- chat.complete just enqueues at the openai-provider boundary,
      -- but a stream.delta means the HTTP request is in flight and
      -- whatever comes next at Ollama queues behind it. Result: a
      -- 50-token ack lands in ~3s instead of ~60s. Idempotent: when
      -- the queue is empty (every delta after the first, all sub-
      -- graph deltas, …) it's a no-op.
      if (kind == stream_delta_kind or kind == stream_reasoning_delta_kind)
          and type(chat_id) == "string"
          and chat_id_stream_visible[chat_id] == true
          and type(spawn_graph_module) == "table"
          and type(spawn_graph_module.flush_pending_dispatches) == "function" then
        spawn_graph_module.flush_pending_dispatches()
      end
      return env
    end

    if kind ~= result_kind then return env end
    local chat_id = env.body.chat_id
    if type(chat_id) ~= "string" then return env end
    local key = chat_id_to_key[chat_id]
    if not key then return env end  -- not ours; pass through to the chat adapter
    local entry = pending[key]
    if not entry then return env end

    -- Build graph.node_result. On success: output = ProviderOut shape
    -- (passthrough from provider). next_state captures the chat_id so
    -- a cyclic provider-wrapper firing can reuse it.
    local out = env.body.output
    pending[key] = nil
    chat_id_to_key[chat_id] = nil
    chat_id_stream_visible[chat_id] = nil

    if type(out) == "table" then
      nefor.log.info("rg_adapter <- provider: chat.complete.result", {
        provider = name,
        chat_id = chat_id,
        run_id = entry.run_id,
        node_id = entry.node_id,
        text_len = type(out.text) == "string" and #out.text or 0,
        text_preview = type(out.text) == "string" and string.sub(out.text, 1, 80) or nil,
        finish_reason = out.finish_reason,
        prompt_tokens = type(out.usage) == "table" and out.usage.prompt_tokens or nil,
        completion_tokens = type(out.usage) == "table" and out.usage.completion_tokens or nil,
      })
      send_node_result_ok(
        entry.run_id, entry.node_id, entry.firing_id,
        out,
        { chat_id = chat_id }
      )
    else
      nefor.log.warn("rg_adapter <- provider: chat.complete.result with non-object output", {
        provider = name,
        chat_id = chat_id,
        out_type = type(out),
      })
      send_node_result_err(
        entry.run_id, entry.node_id, entry.firing_id,
        "provider returned non-object output"
      )
    end
    -- Drop the original chat.complete.result so it doesn't surface
    -- as a chat-stream artifact. (The chat plugin sees the streaming
    -- deltas separately — this terminal envelope is a control event.)
    return nil
  end

  return { from_plugin = from_plugin }
end

-- Attach to tool-gate. Intercepts `tool.result` for invocations we
-- own (matched by tool_id). When all calls in a tool-executor firing
-- have replied, emit `graph.node_result`.
function M.for_tool_gate()
  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    if env.body.kind ~= "tool.result" then return env end
    local tool_id = env.body.id
    if type(tool_id) ~= "string" then return env end
    local ref = tool_id_to_key[tool_id]
    if not ref then return env end  -- not ours
    local entry = pending[ref.key]
    if not entry then
      tool_id_to_key[tool_id] = nil
      return env
    end

    -- Record this call's result.
    local model_call_id = (entry.tool_calls[ref.idx] and entry.tool_calls[ref.idx].id) or tool_id
    entry.tool_results[ref.idx] = {
      id     = model_call_id,
      name   = (entry.tool_calls[ref.idx] and entry.tool_calls[ref.idx].name) or "",
      output = env.body.output,
      error  = env.body.error,
    }
    -- Close out the chat-UI row paired with the start emitted at
    -- invoke time. `error` is a bool (chat-contract); coerce
    -- truthy/non-nil values defensively in case the gate sends
    -- something else.
    emit_to("nefor-chat", {
      kind   = "chat.tool.end",
      id     = model_call_id,
      output = type(env.body.output) == "string" and env.body.output or "",
      error  = env.body.error == true,
    })
    entry.pending_count = entry.pending_count - 1
    tool_id_to_key[tool_id] = nil

    if entry.pending_count == 0 then
      -- All results in. Pack into ToolResults shape and reply.
      pending[ref.key] = nil
      send_node_result_ok(
        entry.run_id, entry.node_id, entry.firing_id,
        { tool_results = entry.tool_results },
        nil
      )
    end
    -- Pass through the tool.result so other listeners (the provider's
    -- chat-history accumulator) still see it. Critical: the provider's
    -- chat history must include tool.result events for the next
    -- chat.complete to reference them.
    return env
  end
  return { from_plugin = from_plugin }
end

-- Test-only state reset.
function M._reset()
  pending = {}
  chat_id_to_key = {}
  chat_id_stream_visible = {}
  tool_id_to_key = {}
  id_counter = 0
  node_result_observers = {}
end

-- Test-only inspection.
function M._pending_count()
  local n = 0
  for _ in pairs(pending) do n = n + 1 end
  return n
end

return M
