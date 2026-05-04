-- starter/agentic_workflow.lua — consolidated orchestration module.
--
-- Replaces the four split files (`chat_orchestrator.lua`,
-- `reasoner_graph_adapter.lua`, `spawn_graph.lua`,
-- `openai_provider_adapter.lua`) with a single cohesive module.
--
-- ### Public API (see ./agentic-workflow-spec.md for the full contract)
--
--   setup(opts)               — one-shot configuration; wires internal
--                               observers; replaces the prior chain of
--                               configure/attach_state_capture/
--                               attach_spawn_graph_listener/
--                               set_default_provider/set_spawn_graph_module.
--   submit(text, opts?)       — programmatic equivalent of typing into chat.
--   set_model(provider, model)— runtime model switch.
--   set_yolo(enabled)         — placeholder; emits a tool-gate.policy.set
--                               event so the surface is locked in for Phase 2
--                               while flagging that the wire-up isn't done.
--   cancel()                  — single-Esc behaviour (cancel current chat
--                               turn at the provider).
--   cancel_all()              — double-Esc / nuclear cancel; preserves D-32
--                               fan-out order.
--   new_chat()                — /new handler; broadcasts chat.reset.
--   on_stream(fn)             — observe stream-visible chat.stream.delta.
--   on_reasoning(fn)          — observe stream-visible reasoning_delta.
--   on_tool_start(fn)         — observe chat.tool.start emission.
--   on_tool_end(fn)            — observe chat.tool.end emission.
--   on_complete(fn)           — observe orchestrator graph.run_complete.
--   on_popup(fn)              — observe chat.popup emissions (placeholder).
--
-- ### Transform factories (called from init.lua's ncp.spawn blocks)
--
--   for_provider(name, opts)      → { from_plugin, to_plugin }
--   for_reasoner_graph()          → { from_plugin }
--   for_tool_gate(gate_name?)     → { from_plugin }
--   for_chat()                    → { from_plugin }
--
-- ### Wire-contract preservation
--
-- This module emits and consumes the SAME bus events the four prior files
-- did. Every external plugin (nefor-chat, openai-provider, reasoner-graph,
-- tool-gate, basic-tools) sees byte-identical traffic. The reasoner-graph
-- protocol (D-21a) — `<type>.run_node` ↔ `<type>.run_node.ack` /
-- `graph.node_result` — and the spawn-graph virtual-source pattern (D-22)
-- are preserved verbatim. D-25 (per-plugin transform), D-26 (sub-graph
-- stream gating), D-27 (fire-and-forget spawn_graph), D-28 (reasoning
-- stream channel), D-29 (responder tools=false), D-30 (terminal sorted-id
-- concat), D-31 (spawn-dispatch deferral on first stream.delta), and D-32
-- (double-Esc fan-out order) all live in this file with the spec naming
-- the function that owns each.

local M = {}

local json = nefor.json

-- Seed math.random at module load so uuid_lite / mint_chat_run_id
-- don't draw from the deterministic Lua-default sequence. os.time() is
-- whole-seconds; mix in os.clock() (sub-second CPU time) and the
-- address of a fresh table for additional entropy across processes
-- spawned in the same wall-clock second. A monotonic counter
-- (`id_seq` below) is folded into the minted ids for tighter collision
-- resistance — two spawn_graph calls in the same second can't collide.
do
  local addr_byte = string.byte(tostring({}):sub(-2, -2)) or 0
  math.randomseed((os.time() * 1000) + math.floor((os.clock() or 0) * 1e6) + addr_byte)
end

-- Monotonic counter used by uuid_lite + mint_chat_run_id to make ids
-- unique even when two calls land in the same os.time() second.
local id_seq = 0

-- ------------------------------------------------------------------
-- configuration & runtime state
-- ------------------------------------------------------------------

-- Orchestrator config — mutated by setup()/set_model().
local config = {
  provider = "ollama",
  model    = nil,
  system   = nil,
}

-- in-flight orchestrator chat run (single-flight for v1).
local current_run_id = nil

-- next_state from wrap node's last firing — seeds the next submit.
local current_state = nil

-- Queued deferred spawn_graph result messages pending flush into a
-- new chat run. Each entry: { text = "<formatted>" }.
local deferred_queue = {}

-- pending firings keyed by run_id:firing_id (provider/tool-gate paths).
local pending = {}

-- Reverse maps for plugin-replies that don't carry firing_id.
local chat_id_to_key = {}
local tool_id_to_key = {}

-- chat_ids whose `<prefix>.stream.{delta,end}` and reasoning siblings
-- should be visible to nefor-chat. True only for `provider-wrapper`
-- (the orchestrator's user-facing wrap node). False for sub-graph
-- responders/dummies.
local chat_id_stream_visible = {}

-- Reasoner types whose streaming should reach nefor-chat.
local STREAM_VISIBLE_TYPES = { ["provider-wrapper"] = true }

-- Monotonic counter for chat/tool ids minted here.
local id_counter = 0

-- Reasoner-type → handler-fn registry. Wired below.
local handlers = {}

-- Sub-graph dispatches captured from `tool-gate.tool.invoke` but not
-- yet emitted as `reasoner-graph.run`. Released on first wrap-stream
-- delta — see D-31.
local pending_dispatches = {}

-- Sub-graph runs keyed by run_id. Each entry: { gate_inner_id }.
local pending_runs = {}

-- Session-replay flag. Flipped on by `sessions.session_start` (which fires
-- for both fresh-boot and resume-target swaps) and back off by
-- `sessions.resume_done`. While true, the reasoner-graph from_plugin
-- transform short-circuits run_node dispatch — the graph already ran in
-- the original session and re-running on replay would duplicate every
-- side effect (provider chat.create, tool.invoke, etc.). The flag stays
-- false during normal live operation; sessions module owns the lifecycle.
local replay_mode = false

-- In-process observer registries. These exist because the events they
-- observe are emitted via `nefor.engine.send` (which bypasses ncp.lua's
-- `from_plugin` / `to_plugin` chains), so a Lua-level consumer cannot
-- subscribe at the bus layer.
local node_result_observers = {}     -- (run_id, node_id, firing_id, output, next_state)
local completed_subscribers = {}     -- (completion_table)
local stream_observers = {}          -- (text)
local reasoning_observers = {}       -- (text)
local tool_start_observers = {}      -- (id, name, input)
local tool_end_observers = {}        -- (id, output, error_bool)
local complete_observers = {}        -- (run_id, status)
local popup_observers = {}           -- (popup_table)

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

local function next_id(prefix)
  id_counter = id_counter + 1
  return prefix .. "-" .. tostring(id_counter)
end

local function pending_key(run_id, firing_id)
  return tostring(run_id) .. ":" .. tostring(firing_id)
end

-- Build an envelope and ship it via the engine's send binding. NCP §3
-- requires `from`+`ts` on every wire envelope; the engine forwards our
-- payloads verbatim, so we stamp them ourselves. Target nil = broadcast
-- (engine fans out); a string targets one peer.
--
-- json.encode is wrapped in pcall: a payload that contains non-UTF-8
-- bytes (e.g. binary file content reaching the bus, or a UTF-8-truncated
-- multibyte boundary from a buggy producer) would otherwise raise from
-- the bus dispatcher with no path back to the run — engine logs the
-- error but the broker keeps idling, hanging the user. On encode
-- failure: emit a synthesised chat.popup so the user sees something
-- failed, log loudly, and request engine exit so the run terminates
-- cleanly instead of hanging.
local function emit(target, body)
  local ok, payload = pcall(json.encode, {
    type = "event",
    from = "engine",
    ts   = nefor.engine.now(),
    body = body,
  })
  if not ok then
    local kind = (type(body) == "table" and tostring(body.kind)) or "(unknown)"
    nefor.log.error("agentic_workflow: json.encode failed — payload not emitted", {
      kind  = kind,
      error = tostring(payload),
    })
    -- Best-effort user-visible popup. Build the popup envelope from
    -- string primitives only so the popup itself can't repeat the
    -- failure. If even this re-encode fails, we've at least logged.
    local popup_ok, popup_payload = pcall(json.encode, {
      type = "event",
      from = "engine",
      ts   = nefor.engine.now(),
      body = {
        kind  = "chat.popup",
        level = "error",
        text  = "internal error: failed to encode bus event (kind="
                .. kind .. "); see engine log",
      },
    })
    if popup_ok and nefor.engine and nefor.engine.send then
      for _, peer in ipairs(nefor.engine.plugins()) do
        nefor.engine.send(popup_payload, peer)
      end
    end
    -- Cleanly tear down the run. The CLI driver waits for engine.exit;
    -- TUI driver treats this as a fatal — better to surface the bug
    -- than silently hang. Guarded so a missing binding (test harness)
    -- doesn't compound the failure.
    if nefor.engine and type(nefor.engine.exit) == "function" then
      nefor.engine.exit(1)
    end
    return
  end
  if target ~= nil then
    nefor.engine.send(payload, target)
  else
    for _, peer in ipairs(nefor.engine.plugins()) do
      nefor.engine.send(payload, peer)
    end
  end
end

local function emit_to(target, body) emit(target, body) end
local function emit_broadcast(body) emit(nil, body) end

local function send_ack(reasoner, run_id, firing_id)
  emit_broadcast({
    kind = reasoner .. ".run_node.ack",
    run_id = run_id,
    firing_id = firing_id,
  })
end

local function send_node_result_ok(run_id, node_id, firing_id, output, next_state)
  local body = {
    kind = "graph.node_result",
    run_id = run_id,
    node_id = node_id,
    firing_id = firing_id,
    output = output,
  }
  if next_state ~= nil then body.next_state = next_state end
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

-- Fire each observer in order; pcall so a bad observer doesn't break
-- the chain. Used by all on_* registries.
local function fire_observers(list, ...)
  for _, cb in ipairs(list) do pcall(cb, ...) end
end

-- ------------------------------------------------------------------
-- spawn_graph internals (D-22: virtual source name)
-- ------------------------------------------------------------------

-- Virtual source name we register `spawn_graph` under. There is no
-- plugin by this name on the bus — `for_tool_gate` intercepts the
-- gate-forwarded `spawn-graph-tool.tool.invoke` and drops it before
-- targeting tries to deliver to a non-existent peer.
local SPAWN_GRAPH_SOURCE = "spawn-graph-tool"

local function spawn_graph_schema()
  return {
    type = "object",
    description = "Submit a reasoner-graph run and return its results.",
    properties = {
      graph = {
        type = "object",
        description = "The graph topology: { nodes: [...], edges: [...] }.",
      },
      on_node_failure = {
        type = "string",
        enum = { "abort", "continue" },
        description = "Failure policy. Defaults to abort.",
      },
    },
    required = { "graph" },
  }
end

local function advertise_body(gate_name)
  return {
    kind   = gate_name .. ".tools.advertise",
    source = SPAWN_GRAPH_SOURCE,
    tools  = {
      {
        name        = "spawn_graph",
        description = "Submit a reasoner-graph run and return its terminal results.",
        parameters  = spawn_graph_schema(),
      },
    },
  }
end

local function uuid_lite()
  id_seq = id_seq + 1
  return string.format(
    "rg-%d-%d-%d",
    os.time(),
    id_seq,
    math.random(0, 2 ^ 31 - 1)
  )
end

-- Serialise sub-graph results into a tool-friendly string. Preference:
--   1. results.terminal.output.text — canonical agent-style exit.
--   2. Any node whose key contains "terminal", "out", or "final".
--   3. Any node's output.text (Lua pairs() order; last-resort).
--   4. JSON-encoded results map.
local function extract_text(entry)
  if type(entry) ~= "table" or type(entry.output) ~= "table" then return nil end
  local out = entry.output
  return out.text or (out.final_answer and out.final_answer.text) or nil
end

local function serialise_results(results)
  if type(results) ~= "table" then return tostring(results) end

  local terminal_text = extract_text(results.terminal)
  if type(terminal_text) == "string" then return terminal_text end

  for nid, entry in pairs(results) do
    if type(nid) == "string"
        and (string.find(nid, "terminal") or string.find(nid, "out") or string.find(nid, "final")) then
      local txt = extract_text(entry)
      if type(txt) == "string" then return txt end
    end
  end

  for _, entry in pairs(results) do
    local txt = extract_text(entry)
    if type(txt) == "string" then return txt end
  end

  return json.encode(results)
end

-- Release every queued sub-graph dispatch. Called from for_provider's
-- stream.delta / stream.reasoning_delta handler on the orchestrator's
-- wrap chat. The moment Ollama starts streaming wrap-firing-2 is the
-- moment we know wrap's HTTP request is committed and any subsequent
-- chat.complete queues behind it. Idempotent.
--
-- Owns invariant D-31.
local function flush_pending_dispatches()
  if #pending_dispatches == 0 then return 0 end
  local n = #pending_dispatches
  local snapshot = pending_dispatches
  pending_dispatches = {}
  for _, entry in ipairs(snapshot) do
    nefor.log.info("agentic_workflow: dispatching queued sub-graph", {
      run_id = entry.run_id,
    })
    emit("reasoner-graph", {
      kind            = "reasoner-graph.run",
      run_id          = entry.run_id,
      graph           = entry.graph,
      on_node_failure = entry.on_node_failure,
    })
  end
  return n
end

-- Cancel every in-flight sub-graph run we minted plus drop the
-- queued-but-not-yet-dispatched runs. Returns the count cancelled.
-- Called only by cancel_all().
local function cancel_all_pending_runs()
  local n = 0
  for run_id, _ in pairs(pending_runs) do
    emit("reasoner-graph", { kind = "reasoner-graph.graph.cancel", run_id = run_id })
    n = n + 1
  end
  pending_runs = {}
  pending_dispatches = {}
  return n
end

-- ------------------------------------------------------------------
-- handler: dummy / provider-wrapper / responder
-- ------------------------------------------------------------------
--
-- Drives openai-provider's chat.create / chat.append / chat.complete
-- sequence. Owns invariants:
--   * D-26 (sub-graph stream gating) — chat_id_stream_visible flag.
--   * D-29 (responder tools=false)   — tools off for sub-graph responders.
--
-- Three-step chat_id precedence (preserved from rg_adapter:271–286):
--   1. prev_state.chat_id (cyclic re-fire within one run).
--   2. args.seed_chat_id (cross-run bootstrap from chat orchestrator).
--   3. mint a fresh id.

local function provider_run_node(reasoner_type, body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local args = body.args or {}
  local inputs = body.inputs or {}
  local prev_state = body.prev_state

  local provider = (type(args) == "table" and args.provider) or config.provider
  if type(provider) ~= "string" or provider == "" then
    return "no provider configured (set args.provider or config.provider)"
  end

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

  nefor.log.info("agentic_workflow.provider_run_node: chat_id resolved", {
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
    local model = (type(args) == "table" and args.model) or config.model
    if type(model) == "string" and #model > 0 then
      create_body.model = model
    end
    -- D-29: sub-graph responder nodes must produce text, not tool calls.
    if reasoner_type == "responder" then
      create_body.tools = false
    end
    nefor.log.info("agentic_workflow -> provider: chat.create", {
      provider = provider,
      chat_id = chat_id,
      model = create_body.model,
      tools = create_body.tools,
    })
    emit_to(provider, create_body)
  end

  if need_create then
    if type(args) == "table" and type(args.system) == "string" and #args.system > 0 then
      nefor.log.info("agentic_workflow -> provider: chat.append (system)", {
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
          nefor.log.info("agentic_workflow -> provider: chat.append (from input)", {
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
        nefor.log.info("agentic_workflow -> provider: chat.append (ProviderOut text as user)", {
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
        nefor.log.info("agentic_workflow -> provider: chat.append (string input as user)", {
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

  -- prev_state on first firing arrives as serde_json `null`, which
  -- nefor.json decodes via mlua to a NULL sentinel (lightuserdata),
  -- NOT Lua nil. Test the positive shape: cycle re-fires set
  -- `prev_state` to a table; anything else means first firing.
  local first_firing = (type(prev_state) ~= "table")
  if first_firing then
    local prompt = (type(args) == "table" and type(args.prompt) == "string") and args.prompt or ""
    if #prompt > 0 then
      nefor.log.info("agentic_workflow -> provider: chat.append (prompt as user)", {
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

  nefor.log.info("agentic_workflow -> provider: chat.complete", {
    provider = provider,
    chat_id = chat_id,
  })
  emit_to(provider, { kind = provider .. ".chat.complete", chat_id = chat_id })
  return nil
end

handlers["dummy"] = function(body) return provider_run_node("dummy", body) end
handlers["provider-wrapper"] = function(body) return provider_run_node("provider-wrapper", body) end
handlers["responder"] = function(body) return provider_run_node("responder", body) end

-- ------------------------------------------------------------------
-- handler: tool-executor
-- ------------------------------------------------------------------

handlers["tool-executor"] = function(body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local inputs = body.inputs or {}

  local calls
  for _, dep_entry in pairs(inputs) do
    if type(dep_entry) == "table" and dep_entry.output ~= nil then
      local out = dep_entry.output
      if type(out) == "table" then
        if type(out.tool_calls) == "table" then
          calls = out.tool_calls
          break
        elseif #out > 0 then
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
    tool_ids      = {},
    pending_count = #calls,
  }

  for i, call in ipairs(calls) do
    local tool_id = next_id("tool")
    pending[key].tool_ids[i] = tool_id
    tool_id_to_key[tool_id] = { key = key, idx = i }
    local call_name = (type(call) == "table" and (call.name or call.tool)) or ""
    local call_args = (type(call) == "table" and (call.arguments or call.args)) or {}
    local model_call_id = (type(call) == "table" and call.id) or tool_id
    fire_observers(tool_start_observers, model_call_id, call_name, call_args)
    emit_to("nefor-tui", {
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
-- handler: adapter (pure Lua; ToolResults → ProviderIn)
-- ------------------------------------------------------------------

handlers["adapter"] = function(body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local inputs = body.inputs or {}

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
-- handler: terminal (D-30 sorted-id concat)
-- ------------------------------------------------------------------
--
-- Single upstream → echo as-is. Multiple upstreams → concatenate each
-- upstream's `output.text` with a `## <upstream_id>` header, ordered by
-- sorted upstream id (Lua pairs() iteration is undefined; sorting makes
-- runs deterministic).

handlers["terminal"] = function(body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local inputs = body.inputs or {}

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
-- orchestrator template + submit
-- ------------------------------------------------------------------

local function mint_chat_run_id()
  id_seq = id_seq + 1
  return string.format(
    "chat-run-%d-%d-%d",
    os.time(),
    id_seq,
    math.random(0, 2 ^ 31 - 1)
  )
end

local function build_orchestrator_graph(opts)
  opts = opts or {}
  local provider = opts.provider or "ollama"
  local model = opts.model
  local system = opts.system or ""
  local user_text = opts.user_text or ""

  local wrap_args = {
    provider = provider,
    prompt   = user_text,
  }
  if type(system) == "string" and #system > 0 then
    wrap_args.system = system
  end
  if type(model) == "string" and #model > 0 then
    wrap_args.model = model
  end

  return {
    nodes = {
      {
        id       = "wrap",
        reasoner = "provider-wrapper",
        args     = wrap_args,
        fanout   = {
          ["in"] = "generic-provider.ProviderOut",
          out    = {
            "generic-tool.ToolCalls",
            "generic-provider.FinalAnswer",
          },
        },
      },
      { id = "tools",    reasoner = "tool-executor", args = {} },
      { id = "adapt",    reasoner = "adapter",       args = {} },
      { id = "terminal", reasoner = "terminal",      args = {} },
    },
    edges = {
      { from = "wrap",  to = "tools",    type = "generic-tool.ToolCalls" },
      { from = "wrap",  to = "terminal", type = "generic-provider.FinalAnswer" },
      { from = "tools", to = "adapt" },
      { from = "adapt", to = "wrap" },
    },
  }
end

-- Submit the orchestrator template graph as a fresh chat run. Used by
-- both `chat.input.submit` (the user typed something) and the deferred
-- spawn_graph delivery path.
--
-- Returns the freshly-minted run_id, or nil if a run was already in
-- flight (caller is responsible for queueing in that case).
local function submit_orchestrator_run(user_text)
  if current_run_id ~= nil then return nil end

  local graph = build_orchestrator_graph({
    provider  = config.provider,
    model     = config.model,
    system    = config.system,
    user_text = user_text or "",
  })

  if type(current_state) == "table" and type(current_state.chat_id) == "string" then
    graph.nodes[1].args.seed_chat_id = current_state.chat_id
  end

  current_run_id = mint_chat_run_id()
  emit("reasoner-graph", {
    kind            = "reasoner-graph.run",
    run_id          = current_run_id,
    graph           = graph,
    on_node_failure = "abort",
  })
  return current_run_id
end

-- Format a deferred spawn_graph completion into a user-role message.
local function format_deferred(completion)
  local run_id = completion.run_id or "?"
  if completion.status == "success" then
    return "[spawn_graph(run_id=" .. tostring(run_id) .. ") result]\n" ..
           "The sub-graph you submitted earlier has finished. " ..
           "Present the output below to the user as your reply to their " ..
           "original prompt. You may lightly reformat for readability; " ..
           "do not re-spawn the graph, do not fabricate missing content, " ..
           "do not re-analyse whether the result is complete — the " ..
           "sub-graph is the source of truth.\n\n" ..
           "--- output ---\n" ..
           tostring(completion.output or "")
  else
    return "[spawn_graph(run_id=" .. tostring(run_id) .. ") FAILED]\n" ..
           "The sub-graph you submitted earlier failed. Tell the user " ..
           "the sub-graph errored and offer to retry; do not silently " ..
           "re-spawn or fabricate a result.\n\n" ..
           "--- error ---\n" ..
           tostring(completion.error or completion.status or "unknown error")
  end
end

local function flush_deferred()
  if current_run_id ~= nil then return end
  if #deferred_queue == 0 then return end
  local entry = table.remove(deferred_queue, 1)
  nefor.log.info("agentic_workflow: flushing deferred spawn_graph result", {
    text_preview = string.sub(entry.text, 1, 80),
    queue_remaining = #deferred_queue,
  })
  submit_orchestrator_run(entry.text)
end

-- ------------------------------------------------------------------
-- transform factories
-- ------------------------------------------------------------------

-- Compose two `{from_plugin, to_plugin}` transform pairs into one. The
-- inner adapter runs first; if it drops or rewrites the envelope the
-- outer adapter sees the result. Used internally by for_provider.
local function compose_adapters(inner, outer)
  local function chain_from(env)
    local e = env
    if inner.from_plugin then e = inner.from_plugin(e) end
    if e == nil then return nil end
    if outer.from_plugin then e = outer.from_plugin(e) end
    return e
  end
  local function chain_to(env)
    local e = env
    if outer.to_plugin then e = outer.to_plugin(e) end
    if e == nil then return nil end
    if inner.to_plugin then e = inner.to_plugin(e) end
    return e
  end
  return { from_plugin = chain_from, to_plugin = chain_to }
end

-- emit_register_reasoner used by for_starter (folded into for_reasoner_graph below)
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

local function lua_resident_types()
  local names = {}
  for name, _ in pairs(handlers) do
    names[#names + 1] = name
  end
  return names
end

-- ------------------------------------------------------------------
-- for_reasoner_graph — composed transform attached to reasoner-graph
-- ------------------------------------------------------------------
--
-- Combines four behaviours that previously lived in separate factories:
--
--   1. for_starter — on first `reasoner-graph.ready`, register every
--      Lua-resident reasoner type so the scheduler treats them as
--      connected peers. Without this, the first dispatch synthesises
--      "reasoner '<name>' not connected".
--
--   2. for_reasoner_graph — intercept `<token>.run_node`, dispatch the
--      handler, emit ack + (eventually) graph.node_result.
--
--   3. spawn_graph.for_reasoner_graph — intercept `graph.run_complete`
--      for sub-graph runs we minted; broadcast `spawn_graph.completed`
--      and notify in-process subscribers.
--
--   4. chat_orchestrator.for_reasoner_graph — intercept
--      `graph.run_complete` for the orchestrator's own run; clear
--      current_run_id, surface failures, kick off deferred flush.
--
-- The order matters: starter registration (1) only triggers on `ready`;
-- the run_node interception (2) consumes its envelope (returns nil);
-- the graph.run_complete handlers (3) and (4) both observe the same
-- envelope but key off run_id (sub-graph-minted vs chat-minted).

function M.for_reasoner_graph()
  local registered = false

  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local kind = env.body.kind
    if type(kind) ~= "string" then return env end

    -- (1) for_starter: register Lua-resident reasoner types on ready.
    if not registered and kind == "reasoner-graph.ready" then
      for _, name in ipairs(lua_resident_types()) do
        emit_register_reasoner(name)
      end
      registered = true
      return env
    end

    -- During replay, the graph already ran in the original session — we
    -- don't want to re-dispatch run_node handlers (which would emit
    -- fresh chat.create / tool.invoke calls and double every side
    -- effect). Drop graph emissions while replay_mode is true. The flag
    -- is owned by the sessions handlers above and lifted on resume_done.
    if replay_mode then
      return nil
    end

    -- (2) for_reasoner_graph: dispatch <token>.run_node.
    local token = kind:match("^([^.]+)%.run_node$")
    if token then
      local handler = handlers[token]
      if not handler then
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
      if err == "_already_replied" then return nil end
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
      send_ack(token, env.body.run_id, env.body.firing_id)
      return nil
    end

    -- (3) spawn_graph: graph.run_complete for runs we minted.
    if kind == "graph.run_complete" then
      local run_id = env.body.run_id

      local sub_pending = pending_runs[run_id]
      if sub_pending ~= nil then
        pending_runs[run_id] = nil
        local status = env.body.status or "unknown"
        local completed = {
          kind   = "spawn_graph.completed",
          run_id = run_id,
          status = status,
        }
        if status == "success" then
          completed.output = serialise_results(env.body.results)
        else
          completed.error = "spawn_graph run completed with status `" .. status .. "`: " ..
                            json.encode(env.body.results or {})
        end
        nefor.log.info("agentic_workflow: sub-graph completed", {
          run_id = run_id,
          status = status,
          subscribers = #completed_subscribers,
        })
        for _, cb in ipairs(completed_subscribers) do
          pcall(cb, completed)
        end
        emit(nil, completed)
        -- Note: do NOT return here. (4) below also wants to see this
        -- envelope (chat-orchestrator may match on run_id). Falls through.
      end

      -- (4) chat_orchestrator: graph.run_complete for our run.
      if run_id == current_run_id then
        nefor.log.info("agentic_workflow: graph.run_complete for our run", {
          run_id = run_id,
          status = env.body.status,
          had_state = current_state ~= nil,
          chat_id = type(current_state) == "table" and current_state.chat_id or nil,
          deferred_queued = #deferred_queue,
        })
        current_run_id = nil

        local status = env.body.status
        local results = env.body.results or {}

        fire_observers(complete_observers, run_id, tostring(status))

        if status == "success" then
          flush_deferred()
          return env
        end

        local err_text
        for _, key in ipairs({ "_typecheck", "_missing_combinators", "_error", "_cycle" }) do
          local entry = results[key]
          if type(entry) == "table" and type(entry.error) == "string" then
            err_text = "[" .. key .. "] " .. entry.error
            break
          end
        end
        if err_text == nil then
          for nid, entry in pairs(results) do
            if type(entry) == "table" and type(entry.error) == "string" then
              err_text = "[" .. tostring(nid) .. " errored] " .. entry.error
              break
            end
          end
        end
        if type(err_text) ~= "string" or #err_text == 0 then
          err_text = "[orchestrator finished with status: " .. tostring(status) .. "]"
        end

        emit("nefor-tui", {
          kind = "chat.message.append",
          role = "system",
          text = err_text,
        })
        flush_deferred()
        return env
      end

      return env
    end

    return env
  end

  return { from_plugin = from_plugin }
end

-- ------------------------------------------------------------------
-- for_provider — composed transform attached to a provider plugin
-- ------------------------------------------------------------------
--
-- Combines openai_provider_adapter.make + rg_adapter.for_provider via
-- compose_adapters. The inner adapter (rg_adapter) runs first on
-- ingress (it sees raw `<prefix>.*` events and intercepts
-- `<prefix>.chat.complete.result` for chats we own); if it returns nil
-- the outer adapter (openai_provider_adapter, which renames to
-- `chat.*`) doesn't see the event. Stream gating + spawn-dispatch
-- flush also live in the inner adapter.
--
-- Owns invariants:
--   * D-26 (sub-graph stream gating)
--   * D-28 (reasoning stream gating)
--   * D-31 (spawn-dispatch deferral on first stream.delta)
--
-- `opts` (optional table):
--   * `static_token` — string to push back as `<prefix>.auth.set` once
--     the first `<prefix>.ready` is observed.

function M.for_provider(name, opts)
  assert(type(name) == "string" and #name > 0,
         "for_provider: name must be non-empty string")
  opts = opts or {}
  local static_token = opts.static_token
  if static_token ~= nil then
    assert(type(static_token) == "string" and #static_token > 0,
           "for_provider: opts.static_token must be a non-empty string")
  end
  local prefix = name .. "."
  local result_kind = prefix .. "chat.complete.result"
  local stream_delta_kind = prefix .. "stream.delta"
  local stream_end_kind   = prefix .. "stream.end"
  local stream_reasoning_delta_kind = prefix .. "stream.reasoning_delta"
  local stream_reasoning_end_kind   = prefix .. "stream.reasoning_end"
  local session_stats_kind = prefix .. "session.stats"

  -- Inner adapter: rg-style (chat ownership + stream gating + spawn flush).
  local function inner_from(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local kind = env.body.kind

    -- Stream-side kinds carry chat_id; gate sub-graph streams from
    -- reaching nefor-chat. D-26, D-28.
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

      -- D-31: first stream.delta / stream.reasoning_delta from a
      -- stream-visible chat releases queued sub-graph dispatches.
      if (kind == stream_delta_kind or kind == stream_reasoning_delta_kind)
          and type(chat_id) == "string"
          and chat_id_stream_visible[chat_id] == true then
        flush_pending_dispatches()
      end

      -- Fire stream/reasoning observers AFTER the gate check so
      -- callers only see stream-visible deltas. (D-26 + D-28 propagate
      -- to observers too.)
      if type(chat_id) == "string"
          and chat_id_stream_visible[chat_id] == true then
        if kind == stream_delta_kind then
          local txt = env.body.text or env.body.delta or ""
          if type(txt) == "string" then fire_observers(stream_observers, txt) end
        elseif kind == stream_reasoning_delta_kind then
          local txt = env.body.text or env.body.delta or ""
          if type(txt) == "string" then fire_observers(reasoning_observers, txt) end
        end
      end

      return env
    end

    if kind ~= result_kind then return env end
    local chat_id = env.body.chat_id
    if type(chat_id) ~= "string" then return env end
    local key = chat_id_to_key[chat_id]
    if not key then return env end
    local entry = pending[key]
    if not entry then return env end

    local out = env.body.output
    local was_stream_visible = chat_id_stream_visible[chat_id] == true
    pending[key] = nil
    chat_id_to_key[chat_id] = nil
    chat_id_stream_visible[chat_id] = nil

    -- D-31 backup flush: the primary trigger is the first stream.delta
    -- or stream.reasoning_delta on a stream-visible chat (see ~line
    -- 1095). That covers ~all real provider turns because qwen/ollama
    -- always emit at least one intermediate text or reasoning delta.
    -- The rare gap: a wrap firing that goes straight from chat.complete
    -- to a tool-calls result with zero deltas — possible with some
    -- providers' compact responses. Without this backup the queued
    -- sub-graph dispatch leaks until cancel_all or the next user turn.
    -- chat.complete.result arrives only AFTER the provider's response
    -- is fully processed, so the dispatch-overtake race that ruled out
    -- a chat.complete trigger doesn't apply here. flush is idempotent
    -- (no-op when queue is empty) so the redundancy with the primary
    -- trigger is harmless.
    if was_stream_visible then
      flush_pending_dispatches()
    end

    if type(out) == "table" then
      nefor.log.info("agentic_workflow <- provider: chat.complete.result", {
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
      nefor.log.warn("agentic_workflow <- provider: chat.complete.result with non-object output", {
        provider = name,
        chat_id = chat_id,
        out_type = type(out),
      })
      send_node_result_err(
        entry.run_id, entry.node_id, entry.firing_id,
        "provider returned non-object output"
      )
    end
    return nil
  end

  -- Outer adapter: chat-contract translation.
  local injected_static = false

  local function outer_from(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local k = env.body.kind
    if type(k) ~= "string" then return env end

    if k == prefix .. "stream.delta" then
      env.body.kind = "chat.stream.delta"
    elseif k == prefix .. "stream.reasoning_delta" then
      env.body.kind = "chat.stream.reasoning_delta"
    elseif k == prefix .. "stream.reasoning_end" then
      env.body.kind = "chat.stream.reasoning_end"
    elseif k == prefix .. "stream.end" then
      env.body.kind = "chat.stream.end"
      env.body.finish_reason = nil
    elseif k == prefix .. "session.stats" then
      env.body.kind = "chat.session.stats"
    elseif k == prefix .. "auth.status" then
      env.body.kind = "chat.auth.status"
      env.body.provider = name
    elseif k == prefix .. "models.listed" then
      env.body.kind = "chat.models.listed"
      env.body.provider = name
    elseif k == prefix .. "model.set_ack" then
      env.body.kind = "chat.model.set_ack"
      env.body.provider = name
    elseif k == prefix .. "turn.error" then
      local msg = tostring(env.body.message or "(unknown)")
      if msg == "interrupted" then
        env.body = {
          kind = "chat.message.append",
          role = "system",
          text = "[interrupted]",
        }
      else
        env.body = {
          kind = "chat.message.append",
          role = "system",
          text = "Error: " .. msg,
        }
      end
    elseif k == prefix .. "hello" then
      local model = env.body.model
      if type(model) == "string" and #model > 0 then
        env.body = {
          kind     = "chat.model.set_ack",
          provider = name,
          model    = model,
        }
        return env
      end
      return nil
    elseif k == prefix .. "ready"
        or k == prefix .. "goodbye" then
      if k == prefix .. "ready"
          and static_token ~= nil
          and not injected_static
          and nefor and nefor.engine and nefor.engine.send and nefor.json then
        injected_static = true
        local payload = nefor.json.encode({
          type = "event",
          from = "engine",
          ts   = nefor.engine.now(),
          body = {
            kind  = prefix .. "auth.set",
            token = static_token,
          },
        })
        nefor.engine.send(payload, name)
      end
      return nil
    end
    return env
  end

  local function outer_to(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local k = env.body.kind
    if type(k) ~= "string" then return env end

    if k == "chat.input.submit" then
      env.body.kind = prefix .. "prompt"
    elseif k == "chat.interrupt" then
      env.body.kind = prefix .. "interrupt"
    elseif k == "chat.reset" then
      env.body.kind = prefix .. "reset"
    elseif k == "chat.auth.set" then
      if env.body.provider ~= name then return nil end
      local token = env.body.token
      env.body = {
        kind = prefix .. "auth.set",
        token = token,
      }
    elseif k == "chat.login_requested" then
      if env.body.provider ~= name then return nil end
      env.body = { kind = prefix .. "login_requested" }
    elseif k == "chat.logout_requested" then
      if env.body.provider ~= name then return nil end
      env.body = { kind = prefix .. "logout_requested" }
    elseif k == "chat.model.list_requested" then
      if env.body.provider ~= name then return nil end
      env.body = { kind = prefix .. "models.list_requested" }
    elseif k == "chat.model.set" then
      if env.body.provider ~= name then return nil end
      local model = env.body.model
      env.body = { kind = prefix .. "model.set", model = model }
    end
    return env
  end

  -- Compose: inner runs first on ingress, last on egress.
  local inner = { from_plugin = inner_from }
  local outer = { from_plugin = outer_from, to_plugin = outer_to }
  return compose_adapters(inner, outer)
end

-- ------------------------------------------------------------------
-- for_tool_gate — composed transform attached to tool-gate
-- ------------------------------------------------------------------
--
-- Combines rg_adapter.for_tool_gate (intercepts tool.result for
-- tool-executor firings) and spawn_graph.for_tool_gate (advertises
-- spawn_graph + intercepts the gate-forwarded
-- spawn-graph-tool.tool.invoke).

function M.for_tool_gate(gate_name)
  gate_name = gate_name or "tool-gate"
  local advertised = false
  local hello_kind = gate_name .. ".hello"
  local invoke_kind = SPAWN_GRAPH_SOURCE .. ".tool.invoke"

  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end

    -- Spawn-graph: advertise on first hello.
    if not advertised and env.body.kind == hello_kind then
      advertised = true
      emit(gate_name, advertise_body(gate_name))
      return env
    end

    -- Spawn-graph: intercept gate-forwarded tool invoke.
    if env.body.kind == invoke_kind then
      local name = env.body.name
      local invoke_id = env.body.id
      local args = env.body.args or {}
      if name ~= "spawn_graph" or type(invoke_id) ~= "string" then
        return nil
      end

      local graph = args.graph
      local on_failure = args.on_node_failure or "abort"
      if type(graph) ~= "table" then
        emit(nil, {
          kind  = "tool.result",
          id    = invoke_id,
          error = "spawn_graph: missing or non-object `graph` argument",
        })
        return nil
      end

      local run_id = uuid_lite()
      pending_runs[run_id] = { gate_inner_id = invoke_id }

      -- D-27: immediate ack so the model's wrap-firing-2 can start
      -- right away.
      emit(nil, {
        kind   = "tool.result",
        id     = invoke_id,
        output = "Submitted sub-graph run_id=" .. run_id ..
                 ". Acknowledge briefly to the user, or chain another " ..
                 "tool call. The real result will arrive later as a " ..
                 "user message tagged `[spawn_graph(run_id=" .. run_id ..
                 ") result]`.",
      })

      -- D-31: queue the dispatch instead of emitting now.
      pending_dispatches[#pending_dispatches + 1] = {
        run_id          = run_id,
        graph           = graph,
        on_node_failure = on_failure,
      }

      nefor.log.info("agentic_workflow: queued sub-graph dispatch (will flush on wrap stream.delta)", {
        run_id = run_id,
        gate_inner_id = invoke_id,
        queue_depth = #pending_dispatches,
      })
      return nil
    end

    -- rg_adapter: tool.result correlation for tool-executor firings.
    if env.body.kind == "tool.result" then
      local tool_id = env.body.id
      if type(tool_id) ~= "string" then return env end
      local ref = tool_id_to_key[tool_id]
      if not ref then return env end
      local entry = pending[ref.key]
      if not entry then
        tool_id_to_key[tool_id] = nil
        return env
      end

      local model_call_id = (entry.tool_calls[ref.idx] and entry.tool_calls[ref.idx].id) or tool_id
      entry.tool_results[ref.idx] = {
        id     = model_call_id,
        name   = (entry.tool_calls[ref.idx] and entry.tool_calls[ref.idx].name) or "",
        output = env.body.output,
        error  = env.body.error,
      }
      -- An error indicator is either `true` or a non-empty string error
      -- message; either should drive the chat-tool-end `error` flag so
      -- the UI can highlight the failed call. Tool implementations vary
      -- between the two shapes.
      local raw_err = env.body.error
      local err_bool = raw_err == true
          or (type(raw_err) == "string" and raw_err ~= "")
      local out_str = type(env.body.output) == "string" and env.body.output or ""
      fire_observers(tool_end_observers, model_call_id, out_str, err_bool)
      emit_to("nefor-tui", {
        kind   = "chat.tool.end",
        id     = model_call_id,
        output = out_str,
        error  = err_bool,
      })
      entry.pending_count = entry.pending_count - 1
      tool_id_to_key[tool_id] = nil

      if entry.pending_count == 0 then
        pending[ref.key] = nil
        send_node_result_ok(
          entry.run_id, entry.node_id, entry.firing_id,
          { tool_results = entry.tool_results },
          nil
        )
      end
      return env
    end

    return env
  end

  return { from_plugin = from_plugin }
end

-- ------------------------------------------------------------------
-- for_chat — transform attached to nefor-chat
-- ------------------------------------------------------------------

function M.for_chat()
  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local kind = env.body.kind

    -- /new handler: clear current_state + drop deferred queue. Also
    -- clear current_run_id so a `/new` mid-run doesn't wedge the next
    -- submit into the [orchestrator busy] path; the in-flight run is
    -- the one being discarded so blocking on it is the wrong move.
    if kind == "chat.reset" then
      nefor.log.info("agentic_workflow: chat.reset received, clearing current_state", {
        had_state = current_state ~= nil,
        prior_chat_id = type(current_state) == "table" and current_state.chat_id or nil,
        dropped_deferred = #deferred_queue,
        had_run = current_run_id ~= nil,
      })
      current_state = nil
      current_run_id = nil
      deferred_queue = {}
      return env
    end

    -- D-32: double-Esc fan-out.
    if kind == "chat.interrupt_all" then
      M.cancel_all()
      return nil
    end

    -- Runtime model switch. Both `/model <name>` (direct) and the
    -- model-picker popup (Enter on a row) emit the same envelope:
    --   { kind = "chat.model.set", provider = "<name>"?, model = "<id>" }
    -- The orchestrator owns `config.model` (used for fresh chat.create
    -- calls inside `provider-wrapper` / `responder` / `tool-executor`
    -- handlers), so it has to update local state here. Without this,
    -- the picker only updated the provider's default-chat seed —
    -- new chats minted by the orchestrator kept the original model
    -- and per-turn footers stayed stamped with it. Pass the envelope
    -- through (return env) so the egress transform on the provider
    -- still translates it into `<prefix>.model.set` and the provider
    -- emits its `model.set_ack`.
    if kind == "chat.model.set" then
      local model = env.body.model
      local provider = env.body.provider
      if type(model) == "string" and #model > 0 then
        nefor.log.info("agentic_workflow: chat.model.set received", {
          provider = provider,
          model    = model,
          previous = config.model,
        })
        M.set_model(provider, model)
      end
      return env
    end

    if kind ~= "chat.input.submit" then return env end

    local text = env.body.text or ""
    if type(text) ~= "string" or #text == 0 then return nil end

    nefor.log.info("agentic_workflow: chat.input.submit received", {
      text_len = #text,
      text_preview = string.sub(text, 1, 80),
      had_state = current_state ~= nil,
      seed_chat_id = type(current_state) == "table" and current_state.chat_id or nil,
      busy = current_run_id ~= nil,
      deferred_queued = #deferred_queue,
    })

    if current_run_id ~= nil then
      emit("nefor-tui", {
        kind = "chat.message.append",
        role = "system",
        text = "[orchestrator busy — wait for the current turn to finish]",
      })
      return nil
    end

    local run_id = submit_orchestrator_run(text)
    nefor.log.info("agentic_workflow: emitting reasoner-graph.run", {
      run_id = run_id,
      seed_chat_id = type(current_state) == "table" and current_state.chat_id or nil,
      prompt_preview = string.sub(text, 1, 80),
    })
    return nil
  end

  return { from_plugin = from_plugin }
end

-- ------------------------------------------------------------------
-- public API
-- ------------------------------------------------------------------

-- One-shot configuration. Wires the in-process observers (state capture
-- + spawn_graph completion listener) before returning so the first
-- possible event is already covered.
--
-- Idempotent: the observer registries are append-only, so a second
-- setup() would double-fire the next_state capture and the
-- spawn_graph completion handler. Config rebinds (provider/model/
-- system) are still honoured on every call — only the observer
-- wire-up is gated.
function M.setup(opts)
  if type(opts) == "table" then
    if type(opts.provider) == "string" and #opts.provider > 0 then
      config.provider = opts.provider
    end
    if type(opts.model) == "string" and #opts.model > 0 then
      config.model = opts.model
    end
    if type(opts.system) == "string" and #opts.system > 0 then
      config.system = opts.system
    end
  end

  if M._setup_done then return end
  M._setup_done = true

  -- next_state capture: when the wrap node's firing completes, persist
  -- next_state so the next submit can seed_chat_id.
  node_result_observers[#node_result_observers + 1] = function(run_id, node_id, _firing_id, _output, next_state)
    if run_id ~= current_run_id then return end
    if node_id ~= "wrap" then return end
    if type(next_state) ~= "table" then return end
    current_state = next_state
    nefor.log.info("agentic_workflow: captured next_state from wrap", {
      run_id = run_id,
      chat_id = next_state.chat_id,
    })
  end

  -- Spawn_graph completion: queue + flush.
  completed_subscribers[#completed_subscribers + 1] = function(completion)
    if type(current_state) ~= "table" or type(current_state.chat_id) ~= "string" then
      nefor.log.info("agentic_workflow: dropping spawn_graph completion (no current chat)", {
        run_id = completion.run_id,
        status = completion.status,
      })
      return
    end
    local text = format_deferred(completion)
    nefor.log.info("agentic_workflow: queueing deferred spawn_graph result", {
      sub_run_id = completion.run_id,
      status = completion.status,
      text_len = #text,
      busy = current_run_id ~= nil,
    })
    deferred_queue[#deferred_queue + 1] = { text = text }
    flush_deferred()
  end

  -- ------------------------------------------------------------------
  -- session lifecycle handlers
  -- ------------------------------------------------------------------
  --
  -- The starter's `sessions` module emits four control events on the
  -- bus: session_start, session_end, resume_done, resume_request. We
  -- subscribe via `nefor.bus.on_event` (which fires on every routed
  -- envelope, regardless of origin — distinct from ncp.step which skips
  -- engine-/step-origin entries) so per-plugin orchestration state can
  -- track session boundaries.
  --
  --   session_end  → tear down in-flight orchestrator run, drop chat-id
  --                   bookkeeping, broadcast `chat.reset` so the
  --                   provider drops its `Chats` map and the TUI clears
  --                   the transcript.
  --   session_start → ensure state is empty (idempotent), set
  --                   `replay_mode = true` so the reasoner-graph
  --                   from_plugin transform stops dispatching run_node
  --                   handlers (the graph already ran in the original
  --                   session — re-running would duplicate every side
  --                   effect).
  --   resume_done  → flip `replay_mode = false`; the orchestrator is
  --                   live again and ready to accept the next submit.
  if nefor.bus and nefor.bus.on_event then
    -- session_end fires only on a resume swap (and on shutdown — which
    -- we ignore here, the process is going away). It's the signal that
    -- the next session_start belongs to a different session id and any
    -- subsequent envelopes through resume_done are replays. We use it
    -- as the "expect replay" gate: the next session_start that follows
    -- enables replay_mode.
    local expecting_replay = false

    nefor.bus.on_event("sessions.session_end", function(_entry)
      -- Cancel any in-flight orchestrator run on the reasoner-graph side
      -- and drop sub-graph bookkeeping. Mirrors cancel_all() but without
      -- the user-visible "[interrupted: ...]" message — session swaps
      -- are silent at the UX layer.
      if current_run_id ~= nil then
        emit("reasoner-graph", { kind = "reasoner-graph.graph.cancel", run_id = current_run_id })
        current_run_id = nil
      end
      for run_id, _ in pairs(pending_runs) do
        emit("reasoner-graph", { kind = "reasoner-graph.graph.cancel", run_id = run_id })
      end
      pending_runs       = {}
      pending_dispatches = {}
      pending            = {}
      chat_id_to_key     = {}
      chat_id_stream_visible = {}
      tool_id_to_key     = {}
      current_state      = nil
      deferred_queue     = {}
      -- Broadcast chat.reset so the provider's Chats map clears and the
      -- TUI transcript empties. Provider's `<prefix>.reset` translation
      -- already exists in for_provider's outer_to.
      emit(nil, { kind = "chat.reset" })
      expecting_replay = true
      nefor.log.info("agentic_workflow: sessions.session_end → state cleared", {})
    end)

    nefor.bus.on_event("sessions.session_start", function(_entry)
      -- session_start fires twice in a session's lifetime: once at boot
      -- (fresh, no replay coming) and once after a resume swap (replay
      -- about to begin). Boot has no preceding session_end, so
      -- expecting_replay is false and we leave replay_mode off.
      -- Resume's session_start follows session_end, so we enter replay
      -- mode and wait for resume_done to lift it.
      if expecting_replay then
        replay_mode = true
        expecting_replay = false
        nefor.log.info("agentic_workflow: sessions.session_start (resume) → replay_mode=true", {})
      else
        nefor.log.info("agentic_workflow: sessions.session_start (boot) → live", {})
      end
    end)

    nefor.bus.on_event("sessions.resume_done", function(_entry)
      replay_mode = false
      nefor.log.info("agentic_workflow: sessions.resume_done → replay_mode=false", {})
    end)
  end
end

-- Programmatic submit. Returns the minted run_id, or nil if a run was
-- already in flight. The opts table reserved for future use.
function M.submit(text, _opts)
  return submit_orchestrator_run(text)
end

-- Runtime model switch. The next submit will pick up the new values.
function M.set_model(provider, model)
  if type(provider) == "string" and #provider > 0 then
    config.provider = provider
  end
  if type(model) == "string" and #model > 0 then
    config.model = model
  end
end

-- Yolo-mode placeholder. Emits a tool-gate.policy.set event so the API
-- surface is locked in for Phase 2's CLI --yolo flag, and logs that the
-- wire-up isn't done yet.
function M.set_yolo(enabled)
  local default = enabled and "auto" or "prompt"
  emit("tool-gate", {
    kind    = "tool-gate.policy.set",
    default = default,
  })
  nefor.log.info("agentic_workflow.set_yolo: placeholder event emitted; tool-gate not yet wired", {
    enabled = enabled,
    default = default,
  })
end

-- Single-Esc behaviour: cancel the current chat turn at the provider.
function M.cancel()
  if current_run_id == nil then return end
  emit(config.provider, {
    kind = config.provider .. ".interrupt",
  })
end

-- Double-Esc / nuclear cancel. Owns invariant D-32: fan-out order is
-- (1) cancel current orchestrator run, (2) cancel pending sub-graph
-- runs + clear queued dispatches, (3) drop deferred queue, (4) post a
-- system message with counts.
function M.cancel_all()
  local cancelled_chat = current_run_id ~= nil
  if cancelled_chat then
    emit("reasoner-graph", { kind = "reasoner-graph.graph.cancel", run_id = current_run_id })
    current_run_id = nil
  end
  local sub_n = cancel_all_pending_runs()
  local dropped = #deferred_queue
  deferred_queue = {}
  -- Drop the in-flight provider/tool-gate bookkeeping too. A late
  -- chat.complete.result for a chat we just cancelled would otherwise
  -- emit a stale graph.node_result for a run_id reasoner-graph already
  -- discarded. Reasoner-graph would silently drop it but the Lua-side
  -- state bleed is a robustness wart. The registries get rebuilt as
  -- new chats are created.
  pending = {}
  chat_id_to_key = {}
  chat_id_stream_visible = {}
  tool_id_to_key = {}
  nefor.log.info("agentic_workflow: cancel_all", {
    cancelled_chat_run = cancelled_chat,
    cancelled_sub_runs = sub_n,
    dropped_deferred = dropped,
  })
  emit("nefor-tui", {
    kind = "chat.message.append",
    role = "system",
    text = string.format(
      "[interrupted: chat=%s sub-graphs=%d deferred=%d]",
      cancelled_chat and "1" or "0", sub_n, dropped),
  })
  return { chat = cancelled_chat, sub_graphs = sub_n, deferred = dropped }
end

-- /new handler: broadcast chat.reset (openai-provider clears its chat
-- histories, nefor-chat clears its transcript), and locally clear our
-- current_state + deferred queue. Also clear current_run_id — same
-- reasoning as the chat.reset arm in for_chat.
function M.new_chat()
  current_state = nil
  current_run_id = nil
  deferred_queue = {}
  emit(nil, { kind = "chat.reset" })
end

-- Observer registration. Each on_* function appends to a private list;
-- callbacks fire in registration order, errors swallowed via pcall.
function M.on_stream(fn)
  assert(type(fn) == "function", "on_stream: callback must be a function")
  stream_observers[#stream_observers + 1] = fn
end

function M.on_reasoning(fn)
  assert(type(fn) == "function", "on_reasoning: callback must be a function")
  reasoning_observers[#reasoning_observers + 1] = fn
end

function M.on_tool_start(fn)
  assert(type(fn) == "function", "on_tool_start: callback must be a function")
  tool_start_observers[#tool_start_observers + 1] = fn
end

function M.on_tool_end(fn)
  assert(type(fn) == "function", "on_tool_end: callback must be a function")
  tool_end_observers[#tool_end_observers + 1] = fn
end

function M.on_complete(fn)
  assert(type(fn) == "function", "on_complete: callback must be a function")
  complete_observers[#complete_observers + 1] = fn
end

function M.on_popup(fn)
  assert(type(fn) == "function", "on_popup: callback must be a function")
  popup_observers[#popup_observers + 1] = fn
end

-- ------------------------------------------------------------------
-- internal hooks (used by tests + dev tooling)
-- ------------------------------------------------------------------

-- Register a custom reasoner type. `name` is the reasoner-type token;
-- `handler_fn(body)` runs once per firing and returns nil on dispatch
-- success, a string error to surface as graph.node_result.error, or
-- the sentinel "_already_replied" if it emitted ack+result itself.
function M.register_type(name, handler_fn)
  assert(type(name) == "string" and #name > 0,
         "register_type: name must be non-empty string")
  assert(type(handler_fn) == "function",
         "register_type: handler must be a function")
  handlers[name] = handler_fn
end

-- Subscribe to every successful graph.node_result emission. Used
-- internally by setup() and exposed for tests.
function M.on_node_result(callback)
  assert(type(callback) == "function",
         "on_node_result: callback must be a function")
  node_result_observers[#node_result_observers + 1] = callback
end

-- Subscribe to spawn_graph completion events. Used internally by
-- setup() and exposed for tests.
function M.on_completed(callback)
  assert(type(callback) == "function",
         "on_completed: callback must be a function")
  completed_subscribers[#completed_subscribers + 1] = callback
end

-- Test/dev only: build the orchestrator template graph for inspection.
function M.build_template(user_text, opts)
  opts = opts or {}
  return build_orchestrator_graph({
    provider  = opts.provider or config.provider,
    model     = opts.model    or config.model,
    system    = opts.system   or config.system,
    user_text = user_text or "",
  })
end

-- Test-only state reset. Clears every module-level table back to its
-- initial value. Observer lists are emptied — the test must re-call
-- setup() if it relies on the wired-in observers.
function M._reset()
  config = { provider = "ollama", model = nil, system = nil }
  current_run_id = nil
  current_state = nil
  deferred_queue = {}
  pending = {}
  chat_id_to_key = {}
  tool_id_to_key = {}
  chat_id_stream_visible = {}
  id_counter = 0
  pending_dispatches = {}
  pending_runs = {}
  node_result_observers = {}
  completed_subscribers = {}
  stream_observers = {}
  reasoning_observers = {}
  tool_start_observers = {}
  tool_end_observers = {}
  complete_observers = {}
  popup_observers = {}
  M._setup_done = nil
end

-- Test-only inspection.
function M._pending_count()
  local n = 0
  for _ in pairs(pending) do n = n + 1 end
  return n
end

-- Test-only: flush the dispatch queue manually. Mirrors the inner
-- spawn_graph flush (called by for_provider's stream.delta hook in
-- production); useful for tests that want to verify the flush behaviour
-- without driving a full provider stream.
function M._flush_pending_dispatches() return flush_pending_dispatches() end

return M
