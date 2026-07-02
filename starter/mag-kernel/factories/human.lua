-- starter/mag-kernel/factories/human.lua — the approval / input boundary.
--
-- A human-in-the-loop gate expressed as a factory like any other: it declares
-- a contract, signs its outputs, confirms ready. The chat surface that renders
-- the request and collects the reply does not exist at this layer — the
-- factory is kept purely to the message shape (request out, reply in), so it is
-- fully testable by feeding it a stubbed reply.
--
-- Flow:
--   * A subject arrives on the declared data input → record it as pending and
--     emit an approval request (`mag.ApprovalRequest`) for the control plane /
--     chat surface to render.
--   * The reply arrives later as a control-plane message with the reserved
--     kind `mag.ApprovalReply` (delivered through the same activate entry, the
--     way a signal is just a message — actor-model.md). It is NOT a routed
--     data input: replies originate at the chat surface, not upstream actors,
--     so they carry a reserved kind rather than a declared input port.
--   * The reply resolves the gate to a typed exit: `human.Approved` (carrying
--     the human's content) or `human.Rejected` (carrying a reason). A union
--     output makes the approve/reject fork a type fact for downstream wiring,
--     consistent with the constitution's typed-exit rule.
--
-- Message shapes (all flagged for review):
--   request out : { kind="mag.ApprovalRequest", from=id, correlation=id,
--                   prompt=<params.prompt?>, subject=<the input message> }
--   reply  in   : { kind="mag.ApprovalReply", approved=<bool>,
--                   content=<approved value?>, reason=<rejection reason?> }
--   output      : human.Approved { subject, content } | human.Rejected { subject, reason }
--
-- drain handler (flagged): a human gate CAN hold pending external work — an
-- outstanding request a person hasn't answered. So per actor-model.md ("an
-- actor holding live external work implements the handler to abort it") drain
-- is meaningful: cancel the outstanding request (`mag.ApprovalCancel`, so the
-- chat surface can retract the prompt), drop the pending state, then die.

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

-- construct(id, params, emit) -> instance
function M.construct(id, params, emit)
  params = params or {}

  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }

  -- Per-instance boundary state: the subject awaiting a human reply, or nil.
  local pending = nil

  function instance.activate(message)
    message = message or {}

    if message.kind == "mag.ApprovalReply" then
      -- A reply for an outstanding request. With none outstanding, ignore it
      -- (a late or duplicate reply is not an activation).
      if pending == nil then
        return
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
      return
    end

    -- Otherwise: a subject to approve. Record it and raise the request.
    pending = message
    emit(sign({
      kind = "mag.ApprovalRequest",
      correlation = id,
      prompt = params.prompt,
      subject = message,
    }))
  end

  -- Explicit drain handler (SIGTERM analog): abort the outstanding request,
  -- then a signed completion. Written inline — no wrapper composed this in.
  function instance.handle_drain()
    if pending ~= nil then
      emit(sign({ kind = "mag.ApprovalCancel", correlation = id }))
      pending = nil
    end
    emit(sign({ kind = "mag.Completed" }))
  end

  -- Ready barrier (actor-model.md, Lifecycle): confirm creation for this id.
  emit(sign({ kind = "mag.ready" }))

  return instance
end

return M
