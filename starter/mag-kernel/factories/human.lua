-- starter/mag-kernel/factories/human.lua — the approval / input boundary.
--
-- A human-in-the-loop gate expressed as a factory like any other: it declares
-- a contract, signs its outputs, confirms ready. The chat surface that renders
-- the request and collects the reply does not exist at this layer — the
-- factory is kept purely to the message shape (request out, reply in), so it is
-- fully testable by feeding it a stubbed reply.
--
-- Flow (async factory — routing.lua, the kernel⇄factory contract; the pinned
-- contract lives in actor-model.md, The approval boundary):
--   * A subject arrives as a graph activation on the declared data input →
--     record it as pending, emit an approval request (`mag.ApprovalRequest` —
--     a kernel-intercepted emit the delivery layer surfaces to the control
--     plane as the run_id-stamped `mag.approval_request` event), and return
--     `{ status = "pending" }`: the gate defers completion until a human answers.
--   * The reply arrives later as a SECOND graph delivery whose one message
--     carries the reserved tag `mag.ApprovalReply` (delivered through the same
--     `deliver` entry, the way a signal is just a message — actor-model.md). It
--     is NOT a routed declared data input: replies originate at the chat
--     surface, not upstream actors — the control plane injects them as a
--     `mag.apply` modification message, and the kernel delivers them by tag
--     past the declared ports to the CONSTRUCTED instance (routing.lua, port
--     bypass; a reply at an unconstructed gate rejects the modification).
--     (Delivery choice: a graph second-delivery, not the capability
--     `{kind="reply"}` activation — the gate never issues a
--     `capability.invoke`, so it is not on the correlation channel; the reply
--     reuses the same graph channel as the subject, tag-discriminated.)
--   * The reply resolves the gate to a typed exit: `human.Approved` (carrying
--     the human's content) or `human.Rejected` (carrying a reason). A union
--     output makes the approve/reject fork a type fact for downstream wiring,
--     consistent with the constitution's typed-exit rule. Because completion was
--     deferred, the resolved gate signals success with a `mag.complete` emit
--     (the async-completion path — routing.lua), so the kernel emits mag.Unit
--     along any dependency edges; the reply delivery itself returns nil.
--
-- Message shapes (pinned — actor-model.md, Canonical payloads):
--   request out : { kind="mag.ApprovalRequest", from=id, correlation=id,
--                   prompt=<params.prompt?>, subject=<the input message> }
--   reply  in   : graph activation carrying { tag="mag.ApprovalReply",
--                   message = { approved=<bool>, content=<approved value?>,
--                               reason=<rejection reason?> } }
--   output      : human.Approved { subject, content } | human.Rejected { subject, reason }
--
-- drain handler: a human gate CAN hold pending external work — an
-- outstanding request a person hasn't answered. So per actor-model.md ("an
-- actor holding live external work implements the handler to abort it") drain
-- is meaningful: cancel the outstanding request (`mag.ApprovalCancel`, so the
-- chat surface can retract the prompt), drop the pending state, then die.

local kinds = require("kinds")

local M = {}

M.declaration = {
  name = "human",

  params = {
    prompt = "string?", -- optional label shown with the approval request
  },

  inputs = {
    subject = "generic-provider.FinalAnswer",
  },

  outputs = {
    "human.Approved",
    "human.Rejected",
  },

  signals = {
    "drain",
  },
}

-- construct(id, params, emit, deps) -> instance
function M.construct(id, params, emit, deps)
  params = params or {}

  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }

  -- Per-instance boundary state: the subject awaiting a human reply, or nil.
  local pending = nil

  -- deliver(activation) -> completion (routing.lua, the kernel⇄factory
  -- contract). One delivered message per graph activation; the reserved
  -- `mag.ApprovalReply` tag discriminates a reply from a subject.
  function instance.deliver(activation)
    activation = activation or {}
    local one = (activation.messages or {})[1] or {}
    local tag = one.tag
    local message = one.message or {}

    if tag == kinds.ApprovalReply then
      -- A reply for an outstanding request. With none outstanding, ignore it
      -- (a late or duplicate reply is not an activation).
      if pending == nil then
        return nil
      end
      local subject = pending
      pending = nil
      if message.approved then
        emit(sign({
          kind = "human.Approved",
          subject = subject,
          content = message.content,
        }))
      else
        emit(sign({
          kind = "human.Rejected",
          subject = subject,
          reason = message.reason,
        }))
      end
      -- Deferred completion resolves now: signal async success so the kernel
      -- emits mag.Unit along dependency edges. The delivery returns nil.
      emit(sign({ kind = kinds.complete }))
      return nil
    end

    -- Otherwise: a subject to approve. Record it, raise the request, and defer
    -- completion until the human answers.
    pending = message
    emit(sign({
      kind = kinds.ApprovalRequest,
      correlation = id,
      prompt = params.prompt,
      subject = message,
    }))
    return { status = "pending" }
  end

  -- Explicit drain handler (SIGTERM analog): abort the outstanding request,
  -- then a signed completion. Written inline — no wrapper composed this in. The
  -- `mag.ApprovalCancel` is a control-plane-bound cancel (not a declared
  -- output): the delivery layer intercepts it and surfaces the run_id-stamped
  -- `mag.approval_cancel` event so the chat surface can retract the prompt.
  -- The completion ack is the reserved kinds.complete — no separate
  -- "Completed" kind.
  function instance.handle_drain()
    if pending ~= nil then
      emit(sign({ kind = kinds.ApprovalCancel, correlation = id }))
      pending = nil
    end
    emit(sign({ kind = kinds.complete }))
  end

  -- Readiness confirmation (actor-model.md, Lifecycle): construction happens at
  -- the first activation, so this emit coincides with beginning work.
  emit(sign({ kind = kinds.ready }))

  return instance
end

return M
