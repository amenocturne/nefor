-- Dedicated scoped diagnostic emitter.
--
-- Instruction notices are deliberately not chat.message.append events: they
-- are observability metadata, not canonical/model-facing conversation. The
-- caller supplies provenance stamped by the authoritative MAG run context.

local M = {}

local REQUIRED = {
  "session_id", "run_id", "run_scope", "actor_id", "capability_id", "principal",
}

local OWNERSHIP = { "conversation_id" }

local function nonempty_string(value)
  return type(value) == "string" and value ~= ""
end

local function validate(invocation)
  if type(invocation) ~= "table" then return nil, "missing invocation provenance" end
  for _, field in ipairs(REQUIRED) do
    if not nonempty_string(invocation[field]) then
      return nil, "invalid invocation provenance field " .. field
    end
  end
  if invocation.principal ~= "lead" and invocation.principal ~= "subagent" then
    return nil, "unknown invocation principal"
  end
  if invocation.capability_id:sub(1, #invocation.run_scope + 1)
      ~= invocation.run_scope .. "/" then
    return nil, "capability id contradicts run scope"
  end
  local copy = {}
  for _, field in ipairs(REQUIRED) do copy[field] = invocation[field] end
  for _, field in ipairs(OWNERSHIP) do
    if invocation[field] ~= nil then
      if not nonempty_string(invocation[field]) then
        return nil, "invalid invocation provenance field " .. field
      end
      copy[field] = invocation[field]
    end
  end
  return copy, nil
end

function M.instruction(invocation, emit_fn)
  assert(type(emit_fn) == "function", "chat-emitter: emit_fn must be a function")
  local provenance, validation_error = validate(invocation)
  local E = {}

  function E.valid()
    return provenance ~= nil
  end

  function E.validation_error()
    return validation_error
  end

  function E.principal_key()
    if not provenance then return nil end
    if provenance.principal == "lead" then
      -- Preserve useful once-per-session lead cadence, but only through an
      -- explicit validated lead principal. There is no nil/global bucket.
      return "lead:" .. provenance.session_id
    end
    return table.concat({
      "subagent", provenance.session_id, provenance.run_id, provenance.actor_id,
    }, ":")
  end

  function E.notice(text, opts)
    if not provenance or type(text) ~= "string" or text == "" then return false end
    opts = opts or {}
    if not nonempty_string(opts.notice_id)
        or not nonempty_string(opts.path)
        or not nonempty_string(opts.dir) then
      return false
    end
    emit_fn({
      kind = "chat.instruction.notice",
      notice_id = opts.notice_id,
      text = text,
      path = opts.path,
      dir = opts.dir,
      invocation = provenance,
    })
    return true
  end

  return E
end

M.validate_invocation = validate

return M
