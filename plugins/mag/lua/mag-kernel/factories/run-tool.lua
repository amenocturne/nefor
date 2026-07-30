-- plugins/mag/lua/mag-kernel/factories/run-tool.lua — the tool-invocation boundary.
--
-- The contract-declared replacement for the shape-sniffing `tool-executor`
-- handler (starter/reasoners/init.lua): given the model's tool calls, invoke
-- each through the gated tool surface and hand the aggregated results to the
-- next node. Nothing here inspects an input's runtime shape to decide what it
-- received — the declared input contract does that (docs/ir.md, Firing).
--
-- Contract (reconciled against tests/fixtures/two-agents.modification.json —
-- `run-tool` routes `generic-tool.ToolHandle` to its `tool-result`; flagged):
--   input   generic-tool.ToolCalls   (single; fires per ToolCalls message)
--   output  generic-tool.ToolHandle  (one aggregated handle per batch)
--   params  { allowlist, da-policy } per-node gating threaded to the tool surface
--
-- (The task note names the output `generic-tool.ToolResults`; the landed
-- fixture wires `generic-tool.ToolHandle`. The fixture is the contract source,
-- so `ToolHandle` is used and flagged. `ToolResults` is a one-line change if the
-- fixture is later reconciled the other way.)
--
-- ── async flow (routing.lua, the kernel⇄factory contract) ───────────────────
--   * A ToolCalls activation arrives as a graph delivery. Per call, emit a
--     `capability.invoke` (correlated request — routing mints a request id and
--     puts a `tool.invoke` envelope on the bus) and return `{ status="pending" }`:
--     completion defers until every call answers.
--   * Each answer arrives as a SEPARATE reply activation
--     (`{ kind="reply", ref, result, error }` — routing.lua bus_response), the
--     `ref` echoed back untouched so the batch/slot it belongs to is known
--     without the kernel understanding the scheme. Fill the slot; while the
--     batch is incomplete return nil (still pending). On the last answer, emit
--     the aggregated `generic-tool.ToolHandle` and signal async success with a
--     `mag.complete` emit (the kernel then emits mag.Unit along dependency
--     edges); the reply delivery itself returns nil.
--
-- ── multiple tool calls → one aggregated output (aggregation shape, flagged) ──
--   Several calls in one ToolCalls fan out to parallel `capability.invoke`s
--   (one bus request each, all in flight together), and their answers reassemble
--   into ONE `generic-tool.ToolHandle` per batch:
--     { kind="generic-tool.ToolHandle", from=id,
--       results = { { id=<tool_call_id>, name=<tool>, output=<result>, error=<e> }, … } }
--   `results` is index-ordered to match the incoming calls (slot = call index),
--   and each entry keeps the model's `tool_call_id` so the downstream provider
--   turn can map a result back to the call that produced it. One handle, not one
--   per call: the next node (`tool-result`) fires once with the whole batch,
--   mirroring a provider turn that consumes all tool outputs together.
--
-- ── da-policy threading (closes task-nefor-mag-per-node-da-policies; flagged) ──
--   The MAG node authors `:da-policy {…}` and a tool allowlist; lowering places
--   them in THIS actor's `params`. Per call the factory carries both in the
--   invocation `request` (which routing forwards as the `tool.invoke` args), so
--   the policy authored on the node reaches the exact tool invocation. Enforcing
--   it (narrowing bash approvals to this agent) is tool-gate's side, beyond this
--   factory — the factory's job is that the policy travels with the call.
--
-- ── kill with in-flight calls (signal choice, flagged) ───────────────────────
--   run-tool holds live external work (in-flight tool invocations), so a signal
--   handler is meaningful (actor-model.md, Signals). But the tool surface
--   (basic-tools `bash`) exposes NO abort primitive: a running bash process
--   finishes server-side regardless of the kernel. On kill the kernel has
--   already unrouted this id and dropped its correlations (router:forget), so
--   each in-flight answer, when it lands, finds no correlation and is discarded —
--   the result is voided. So kill has nothing to abort; the handler only drops
--   local batch state and documents the void. Written inline — no wrapper.

local kinds = require("kinds")

local M = {}
local preview_components = require("preview-components")

M.declaration = {
  preview = preview_components.tool_exchange(),
  name = "run-tool",
  semantic = {
    input={kind="named",name="nefor.contracts.ToolCalls",arguments={}},
    output={kind="named",name="nefor.contracts.ToolHandle",arguments={}},
    inputs={{wire="generic-tool.ToolCalls",type={kind="named",name="nefor.contracts.ToolCalls",arguments={}}}},
    outputs={{wire="generic-tool.ToolHandle",type={kind="named",name="nefor.contracts.ToolHandle",arguments={}}}},
  },

  params = {
    allowlist = "table?",  -- tool-name allowlist for this node (lowered from :tools)
    ["da-policy"] = "table?", -- per-node bash approval rules (lowered from :da-policy)
  },

  inputs = {
    calls = "generic-tool.ToolCalls",
  },

  outputs = {
    "generic-tool.ToolHandle",
  },

  signals = {
    "kill",
  },
}

-- construct(id, params, emit, deps) -> instance
function M.construct(id, params, emit, deps)
  params = params or {}
  deps = deps or {}
  local observe = type(deps.preview) == "function" and deps.preview or function() return false end

  -- Per-node gating, threaded to every tool invocation this instance makes.
  -- Read both the authored hyphen key (JSON-lowered `:da-policy`) and an
  -- underscore alias; likewise allowlist / tools. Opaque plain data — the
  -- factory forwards it, tool-gate interprets it.
  local da_policy = params["da-policy"]
  if da_policy == nil then
    da_policy = params.da_policy
  end
  if type(da_policy) == "table" and type(da_policy.rules) == "table" then
    da_policy = da_policy.rules
  end
  local allowlist = params.allowlist
  if allowlist == nil then
    allowlist = params.tools
  end

  local function sign(message)
    message.from = id
    return message
  end

  -- Per-instance batch state: batch id -> { expected, received, results, }.
  -- A batch is one ToolCalls activation; concurrent batches (single input
  -- fires per message) stay independent, disambiguated by the ref echoed on
  -- each reply.
  local batches = {}
  local batch_seq = 0

  local instance = { id = id }

  -- Assemble the ordered results of a complete batch into one ToolHandle,
  -- emit it, and signal deferred success. Slots are index-keyed so `results`
  -- matches the incoming call order regardless of answer arrival order.
  local function complete_batch(bid, batch)
    batches[bid] = nil
    local results = {}
    for i = 1, batch.expected do
      results[i] = batch.results[i]
    end
    emit(sign({ kind = "generic-tool.ToolHandle",
      value = { results = results }, results = results }))
    -- Deferred completion resolves: async success so the kernel emits mag.Unit
    -- along dependency edges. The reply delivery returns nil.
    emit(sign({ kind = kinds.complete }))
  end

  -- A correlated tool answer. Fill its slot; fire only on the complete batch.
  local function handle_reply(activation)
    local ref = activation.ref or {}
    local batch = batches[ref.batch]
    if not batch then
      -- Batch gone (killed, or a duplicate/late reply): not an activation.
      return nil
    end
    if batch.results[ref.index] == nil then
      batch.received = batch.received + 1
    end
    batch.results[ref.index] = {
      id = ref.call_id,
      name = ref.name,
      output = activation.result,
      error = activation.error,
    }
    observe("append", "tool_events", {
      kind = activation.error ~= nil and "error" or "result",
      id = ref.call_id,
      value = activation.error ~= nil and { error = activation.error } or activation.result,
    })
    if batch.received >= batch.expected then
      complete_batch(ref.batch, batch)
    end
    return nil
  end

  -- Fan a ToolCalls batch out to parallel capability.invokes; defer completion.
  local function handle_calls(message)
    local calls = message.calls or {}

    -- A degenerate empty batch has nothing to await: emit an empty handle and
    -- complete synchronously rather than deferring on zero outstanding calls
    -- (which would never resolve). (Flagged.)
    if #calls == 0 then
      emit(sign({ kind = "generic-tool.ToolHandle",
        value = { results = {} }, results = {} }))
      return { status = "ok" }
    end

    batch_seq = batch_seq + 1
    local bid = batch_seq
    batches[bid] = { expected = #calls, received = 0, results = {} }

    for i, call in ipairs(calls) do
      -- Canonical ToolCalls entries (actor-model.md, Canonical payloads):
      -- { id, name, args }. llm emits exactly this shape, so read it directly —
      -- no name/tool or args/arguments alias fallbacks.
      local call_name = call.name
      local call_args = call.args or {}
      observe("append", "tool_events", {
        kind = "call", id = call.id,
        value = { name = call_name, arguments = call_args },
      })
      emit(sign({
        kind = "capability.invoke",
        capability = call_name,
        -- Invocation args forwarded verbatim by routing as the tool.invoke
        -- payload, carrying the per-node gating alongside the tool name/args.
        request = {
          name = call_name,
          args = call_args,
          allowlist = allowlist,
          ["da-policy"] = da_policy,
        },
        -- Opaque correlation ref, echoed back on the reply: which batch + slot,
        -- plus the model's call id / name for the assembled result entry.
        ref = { batch = bid, index = i, call_id = call.id, name = call_name },
      }))
    end
    return { status = "pending" }
  end

  -- deliver(activation) -> completion (routing.lua, the kernel⇄factory
  -- contract). A reply activation is a correlated tool answer; any other is a
  -- graph delivery of a ToolCalls message.
  function instance.deliver(activation)
    activation = activation or {}
    if activation.kind == "reply" then
      return handle_reply(activation)
    end
    local message = ((activation.messages or {})[1] or {}).message or {}
    return handle_calls(message)
  end

  -- Explicit kill handler (SIGKILL analog; signal choice documented in the
  -- header). Nothing to abort — bash finishes server-side and the kernel's
  -- correlation drop voids the answer — so only local batch state is dropped.
  -- Written inline; no wrapper composed this in.
  function instance.handle_kill()
    batches = {}
  end

  -- Readiness confirmation (actor-model.md, Lifecycle): construction happens at
  -- the first activation, so this emit coincides with beginning work.
  emit(sign({ kind = kinds.ready }))

  return instance
end

return M
