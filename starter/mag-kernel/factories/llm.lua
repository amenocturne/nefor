-- starter/mag-kernel/factories/llm.lua — the provider-boundary primitive.
--
-- The one factory that crosses a plugin boundary (a language model provider),
-- which is exactly why it earns a Lua-level place: everything composable ABOVE
-- it (turn loops, tool execution, adapters) lives in the MAG stdlib, not here.
-- This factory does one thing — a single provider round-trip per activation —
-- and classifies the result into a typed exit. It never ports the inline
-- agentic turn-loop from starter/reasoners (that becomes stdlib); it is the
-- contract-declared replacement for the old `provider-wrapper` shim.
--
-- Flow (async factory — routing.lua, the kernel⇄factory contract):
--   * A `generic-provider.ProviderOut` arrives as a graph activation on the
--     single declared input. Build a provider request from params + that
--     message, mint a provider-domain request handle (chat_id), emit a
--     `capability.invoke` (the kernel correlates it and forwards a tool.invoke
--     onto the bus — routing.lua on_capability_invoke), and return
--     `{ status = "pending" }`: completion is deferred until the provider
--     answers.
--   * The provider answer arrives later as a `{ kind = "reply", ref, result,
--     error }` activation through the correlation channel (routing.lua
--     bus_response). Classify it into a typed exit:
--       - tool calls present  → `generic-tool.ToolCalls`
--       - otherwise           → `generic-provider.FinalAnswer`
--     then signal deferred success with a `mag.complete` emit (the kernel emits
--     mag.Unit along dependency edges). A provider ERROR in the reply is a
--     suffered failure: emit `mag.failed` so the kernel routes the failure tag.
--
-- The request handle / cancel mechanics (FLAGGED — the reconciliation the task
-- asked for):
--   * The kernel mints its OWN correlation request id inside
--     on_capability_invoke and never exposes it to the factory; that id only
--     routes the reply back. It is NOT the provider's cancel handle.
--   * The provider-domain handle is the `chat_id` this factory mints and puts
--     in the request. The chatgpt-provider adapter states the contract
--     directly ("the chat_id IS the request handle": at most one in-flight
--     completion per chat, no parallel id invented — see
--     plugins/chatgpt-provider/lua/chatgpt-provider/init.lua `cancel`). So the
--     factory OWNS its cancel handle by construction. These two ids are
--     orthogonal: correlation id → reply routing; chat_id → provider-side
--     addressing of the in-flight work.
--   * Only one request is in flight at a time (completion defers until the
--     reply), so a single `pending.chat_id` is the whole handle state.
--
-- Explicit kill handler (SIGKILL analog — actor-model.md, Signals): the actor
-- is already unrouted when kill lands; the emit is a best-effort abort of the
-- external provider work. The cancel envelope is the chatgpt adapter's
-- `cancel(chat_id)` shape, inlined per the constitution (actor-model.md,
-- Cancellation: "plugin-level message shapes inline in the handler … the
-- factory necessarily knows the plugin's request shapes; it knows the abort
-- shape too"). It is NOT required as chatgpt-provider specifically — `provider`
-- is a param, so the prefix is taken from it and the shape inlined rather than
-- coupling this factory to one provider module (which also would not resolve on
-- the mag-kernel package.path).
--   Delivery seam: `<provider>.chat.cancel` is neither a reserved kernel kind
--   nor a declared/routed output, and at kill time the actor is unrouted — so
--   on_emit → route_output would drop it. The kernel now wires the raw-emit
--   path: dispatch_kill runs this handler with the id in signaling mode, so a
--   non-reserved envelope it emits lands straight on bus_emit, strictly before
--   forget (emit-before-forget; routing.lua on_emit, init.lua on_kill).
--
-- Explicit drain handler (SIGTERM analog): no new requests; finish the current
-- one, then die. If idle, complete immediately (the reserved kinds.complete,
-- matching stub.lua / human.lua). If in flight, mark draining so the pending
-- reply resolves normally (its mag.complete) and the kernel then removes the
-- actor; a new input arriving mid-drain is dropped (no new request is started).

local kinds = require("kinds")

local M = {}

-- Reserved suffered-failure tag (registry.lua RESERVED_ROUTE_KEYS). The
-- canonical constant is kinds.Failed, shared with the kernel and registry.
local FAILED_TAG = kinds.Failed

M.declaration = {
  name = "llm",

  params = {
    model = "string?", -- provider model id
    system = "string?", -- system prompt
    tools = "table?", -- advertised tool list for this call
    profile = "table?", -- opaque provider profile, passed through untouched
    provider = "string?", -- provider capability name (selection)
  },

  -- Single input: fires per arriving ProviderOut (the assembled turn state the
  -- provider consumes). Matches tests/fixtures/two-agents.modification.json,
  -- where adapters / tool-results / loop-counters all route
  -- generic-provider.ProviderOut into an `llm`.
  inputs = {
    provider_out = "generic-provider.ProviderOut",
  },

  -- Typed exit (actor-model.md, Factories): which output leaves is a type
  -- fact, never a position/shape heuristic. Downstream wiring keys on these.
  outputs = {
    "generic-tool.ToolCalls",
    "generic-provider.FinalAnswer",
  },

  signals = {
    "kill",
    "drain",
  },
}

-- construct(id, params, emit, deps) -> instance
function M.construct(id, params, emit, deps)
  params = params or {}
  -- Provider selection is authored data (params), matching the fixture. Its
  -- absence is a construct-time validation error, never a silent default: a
  -- modification that authors an llm node must say which provider capability it
  -- targets. Returning nil + message (not error()) keeps the failure on the
  -- construction seam — the instance never binds, never readies, and the ready
  -- barrier names it a straggler (init.lua set_construct logs the message).
  local provider = params.provider
  if type(provider) ~= "string" or #provider == 0 then
    return nil, string.format(
      "llm '%s': params.provider is required (which provider capability to target)", tostring(id))
  end

  -- Sign: stamp the actor id onto every outbound message (actor-model.md,
  -- Factories: "sign with the id").
  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }

  -- Per-instance boundary state.
  local seq = 0 -- monotone request counter → distinct chat_ids across cycles
  local pending = nil -- { chat_id = <handle> } while a request is in flight
  local draining = false -- drain arrived while in flight; finish then die

  -- Classification rule (FLAGGED): tool calls are "present" when the provider
  -- result carries a non-empty `tool_calls` array. This matches the landed
  -- baseline in starter/reasoners (tool_executor_run_node reads
  -- `out.tool_calls`). A provider that signals tool use only via a
  -- `finish_reason` with an empty array, or nests calls elsewhere, would
  -- misclassify — the array-presence rule is chosen for baseline consistency.
  local function has_tool_calls(result)
    return type(result) == "table"
        and type(result.tool_calls) == "table"
        and #result.tool_calls > 0
  end

  -- Build the provider request from params + the incoming turn message. The
  -- exact provider-capability request schema is the capability plugin's own
  -- concern (behind the tool.invoke abstraction); this carries the fields a
  -- provider needs plus the chat_id handle and the profile passthrough.
  local function build_request(input_message)
    return {
      chat_id = pending.chat_id,
      model = params.model,
      system = params.system,
      tools = params.tools,
      profile = params.profile,
      input = input_message,
    }
  end

  -- deliver(activation) -> completion (routing.lua, the kernel⇄factory
  -- contract). Two activation shapes reach here: a graph activation (a new
  -- turn) and a `{ kind = "reply" }` correlation response.
  function instance.deliver(activation)
    activation = activation or {}

    if activation.kind == "reply" then
      -- A provider answer for our outstanding request. None outstanding → a
      -- late/duplicate reply, ignored (not an activation).
      if pending == nil then
        return nil
      end
      pending = nil

      if activation.error ~= nil then
        -- Suffered provider error → deferred failure. The kernel routes the
        -- failure tag (ir.md, Firing: a suffered failure is kernel-synthesized
        -- so failure routes work uniformly).
        emit(sign({ kind = kinds.failed, failure = FAILED_TAG, value = { error = activation.error } }))
        return nil
      end

      local result = activation.result
      if has_tool_calls(result) then
        -- Canonical ToolCalls payload (actor-model.md, Canonical payloads):
        -- { kind, from, calls = { { id, name, args }, ... } }. This is the
        -- provider boundary, so it normalizes the provider's native tool-call
        -- shape into the pinned form once — downstream (run-tool) reads exactly
        -- id/name/args with no alias fallbacks.
        local calls = {}
        for i, tc in ipairs(result.tool_calls) do
          tc = tc or {}
          local fn = type(tc["function"]) == "table" and tc["function"] or {}
          calls[i] = {
            id = tc.id,
            name = tc.name or fn.name,
            args = tc.args or tc.arguments or fn.arguments,
          }
        end
        emit(sign({
          kind = "generic-tool.ToolCalls",
          calls = calls,
        }))
      else
        local final = { kind = "generic-provider.FinalAnswer", result = result }
        if type(result) == "table" then
          final.text = result.text
          final.final_answer = result.final_answer
        end
        emit(sign(final))
      end

      -- Deferred completion resolves: signal async success so the kernel emits
      -- mag.Unit along dependency edges. If we were draining, this same emit is
      -- the flush; the kernel removes the actor afterward.
      emit(sign({ kind = kinds.complete }))
      return nil
    end

    -- A graph activation: a new turn. While draining, start no new request.
    if draining then
      return nil
    end

    local input_message = ((activation.messages or {})[1] or {}).message
    seq = seq + 1
    pending = { chat_id = id .. "@r" .. tostring(seq) }

    -- Request the provider capability and defer completion. `ref` carries our
    -- handle so the correlation reply is traceable to this request (only one is
    -- ever in flight). The kernel mints its correlation id and puts a
    -- tool.invoke on the bus (routing.lua on_capability_invoke).
    emit(sign({
      kind = "capability.invoke",
      capability = provider,
      request = build_request(input_message),
      ref = pending.chat_id,
    }))
    return { status = "pending" }
  end

  -- Explicit kill handler (SIGKILL analog). Best-effort abort of external work:
  -- if a request is in flight, emit the provider-cancel envelope keyed by the
  -- chat_id handle (the chatgpt adapter's `cancel(chat_id)` shape, inlined).
  -- See the header's cancel-mechanics + delivery-seam flags.
  function instance.handle_kill()
    if pending ~= nil then
      emit(sign({ kind = provider .. ".chat.cancel", chat_id = pending.chat_id }))
      pending = nil
    end
  end

  -- Explicit drain handler (SIGTERM analog). Idle → complete now; in flight →
  -- mark draining and let the pending reply flush + complete, then the kernel
  -- removes the actor.
  function instance.handle_drain()
    if pending == nil then
      emit(sign({ kind = kinds.complete }))
    else
      draining = true
    end
  end

  -- Ready barrier (actor-model.md, Lifecycle): confirm creation for this id.
  emit(sign({ kind = kinds.ready }))

  return instance
end

return M
