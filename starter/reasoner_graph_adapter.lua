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

  if need_create then
    local create_body = { kind = provider .. ".chat.create", chat_id = chat_id }
    local model = (type(args) == "table" and args.model) or default_model
    if type(model) == "string" and #model > 0 then
      create_body.model = model
    end
    nefor.log.info("rg_adapter -> provider: chat.create", {
      provider = provider,
      chat_id = chat_id,
      model = create_body.model,
    })
    emit_to(provider, create_body)
  end

  -- Decide which message(s) to append. Rules:
  --   1. First firing AND args.system → append as system role.
  --   2. inputs.<id>.output is a ProviderIn-shaped table with
  --      `messages` list → append each (preferred path; what `adapter`
  --      emits on cycle re-fire).
  --   3. inputs.<id>.output is a plain string → append as `user`.
  --   4. else: args.prompt as `user` (first-firing convenience for
  --      `dummy`).
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

  local appended_any = false
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
          appended_any = true
        end
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
        appended_any = true
      end
    end
  end

  -- First-firing convenience: append `args.prompt` as a user message
  -- whenever no input-driven messages were appended. This covers BOTH
  -- the fresh-chat case (need_create=true, system already added above)
  -- AND the cross-run resume case (need_create=false, seed_chat_id
  -- carries the existing chat — its system message survives, but the
  -- new turn's user prompt still has to be appended). Without this,
  -- a seeded chat gets `chat.complete` with no new user message and
  -- the model either re-runs on stale history or the provider rejects
  -- the call.
  --
  -- prev_state on first firing arrives as serde_json `null`, which
  -- nefor.json decodes via mlua to a NULL sentinel (lightuserdata),
  -- NOT Lua nil — `prev_state == nil` is false in that case. Test the
  -- positive shape instead: cycle re-fires set `prev_state` to a
  -- table (`{chat_id=...}`); anything else (NULL sentinel, missing,
  -- or non-table) means we're firing for the first time.
  local first_firing = (type(prev_state) ~= "table")
  if not appended_any and first_firing then
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
    emit_to("tool-gate", {
      kind = "tool-gate.tool.invoke",
      id   = tool_id,
      name = (type(call) == "table" and (call.name or call.tool)) or "",
      args = (type(call) == "table" and (call.arguments or call.args)) or {},
    })
  end
  return nil
end

-- ------------------------------------------------------------------
-- handler: adapter (pure Lua)
-- ------------------------------------------------------------------
--
-- Translates ToolResults → ProviderIn. The provider already accumulates
-- `tool.result` events into its chat history (see openai-provider's
-- main.rs `tool.result` arm). So the adapter node's job here is
-- minimal: produce a ProviderIn shape with no new messages, signaling
-- "continue the chat using whatever the provider already has". The
-- wrapper node sees this on its next firing and re-issues
-- chat.complete with no new appends.
--
-- The `messages = {}` shape tells the wrapper "no extra append, just
-- complete again". ToolResults already landed in the provider's
-- chat history via the broadcast `tool.result` events.

handlers["adapter"] = function(body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id

  -- Synchronously reply — this handler does no I/O. We still emit ack
  -- first to keep the lifecycle uniform. The result follows
  -- immediately.
  send_ack("adapter", run_id, firing_id)
  send_node_result_ok(run_id, node_id, firing_id, { messages = {} }, nil)
  return "_already_replied"
end

-- ------------------------------------------------------------------
-- handler: terminal (orchestrator escape edge consumer)
-- ------------------------------------------------------------------
--
-- Receives FinalAnswer-shaped input on the orchestrator's escape edge.
-- Echoes the input as output verbatim so `graph.run_complete.results`
-- can carry the terminal text. Pure Lua, single firing per run.

handlers["terminal"] = function(body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local inputs = body.inputs or {}

  -- Find the FinalAnswer payload — first non-nil input.output wins.
  local final
  for _, dep_entry in pairs(inputs) do
    if type(dep_entry) == "table" and dep_entry.output ~= nil then
      final = dep_entry.output
      break
    end
  end
  if final == nil then final = { text = "" } end

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

  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    if env.body.kind ~= result_kind then return env end
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
    entry.tool_results[ref.idx] = {
      id     = (entry.tool_calls[ref.idx] and entry.tool_calls[ref.idx].id) or tool_id,
      name   = (entry.tool_calls[ref.idx] and entry.tool_calls[ref.idx].name) or "",
      output = env.body.output,
      error  = env.body.error,
    }
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
