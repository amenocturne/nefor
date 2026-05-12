-- Plugin lib for the openai-provider binary. Two primitives:
--   M.translator(name)          -> { outbound, inbound, publish, deliver,
--                                    maybe_inject_static_token, kinds }
--   M.replay_rebuild(env, name) -> nil (side-effecting via nefor.engine.deliver)

local json = nefor.json

local M = {}

-- Per-name owned chat_ids. Two providers on the same bus (e.g. mock-plugin
-- + ollama) coexist; without ownership filtering each translator would
-- react to every replayed tool.result and re-feed chat.append envelopes
-- for chat_ids it never owned, causing the binary to emit chat.error.
local owned_by_name = {}

local function owned_for(name)
  local t = owned_by_name[name]
  if t == nil then
    t = {}
    owned_by_name[name] = t
  end
  return t
end

-- Test-only: drop module-level state.
function M._reset()
  owned_by_name = {}
end

function M.translator(name)
  assert(type(name) == "string" and #name > 0,
    "openai-provider.translator: name required")

  local prefix = name .. "."

  local kinds = {
    stream_delta            = prefix .. "stream.delta",
    stream_end              = prefix .. "stream.end",
    stream_reasoning_delta  = prefix .. "stream.reasoning_delta",
    stream_reasoning_end    = prefix .. "stream.reasoning_end",
    session_stats           = prefix .. "session.stats",
    auth_status             = prefix .. "auth.status",
    auth_set                = prefix .. "auth.set",
    models_listed           = prefix .. "models.listed",
    models_list_requested   = prefix .. "models.list_requested",
    model_set               = prefix .. "model.set",
    model_set_ack           = prefix .. "model.set_ack",
    turn_error              = prefix .. "turn.error",
    chat_error              = prefix .. "chat.error",
    chat_complete_result    = prefix .. "chat.complete.result",
    chat_create             = prefix .. "chat.create",
    chat_append             = prefix .. "chat.append",
    hello                   = prefix .. "hello",
    ready                   = prefix .. "ready",
    goodbye                 = prefix .. "goodbye",
    login_requested         = prefix .. "login_requested",
    logout_requested        = prefix .. "logout_requested",
    interrupt               = prefix .. "interrupt",
    reset                   = prefix .. "reset",
    prompt                  = prefix .. "prompt",
  }

  local owned = owned_for(name)
  local injected_static = false

  -- binary -> bus. Returns the (shallow-copied) body with kind possibly
  -- renamed, or nil to drop.
  local function outbound(env)
    if type(env) ~= "table" or env.type ~= "event"
        or type(env.body) ~= "table" then
      return nil
    end

    -- Shallow-copy so callers can mutate without affecting the source.
    local body = {}
    for k, v in pairs(env.body) do body[k] = v end

    local k = body.kind
    if type(k) ~= "string" then return body end

    if k == kinds.stream_delta then
      body.kind = "chat.stream.delta"
      return body
    elseif k == kinds.stream_reasoning_delta then
      body.kind = "chat.stream.reasoning_delta"
      return body
    elseif k == kinds.stream_reasoning_end then
      body.kind = "chat.stream.reasoning_end"
      return body
    elseif k == kinds.stream_end then
      body.kind = "chat.stream.end"
      body.finish_reason = nil
      return body
    elseif k == kinds.session_stats then
      body.kind = "chat.session.stats"
      return body
    elseif k == kinds.auth_status then
      body.kind = "chat.auth.status"
      body.provider = name
      return body
    elseif k == kinds.models_listed then
      body.kind = "chat.models.listed"
      body.provider = name
      return body
    elseif k == kinds.model_set_ack then
      body.kind = "chat.model.set_ack"
      body.provider = name
      return body
    elseif k == kinds.turn_error then
      -- A missing message can happen if the binary emits turn.error for
      -- an unknown reason; fall back to a generic label rather than
      -- propagating "nil".
      local msg = tostring(body.message or "(unknown)")
      if msg == "interrupted" then
        return {
          kind = "chat.message.append",
          role = "system",
          text = "[interrupted]",
        }
      end
      return {
        kind = "chat.message.append",
        role = "system",
        text = "Error: " .. msg,
      }
    elseif k == kinds.hello then
      local model = body.model
      if type(model) == "string" and #model > 0 then
        return {
          kind     = "chat.model.set_ack",
          provider = name,
          model    = model,
        }
      end
      return nil
    elseif k == kinds.ready or k == kinds.goodbye then
      -- Control-plane envelopes; static-token injection runs through
      -- maybe_inject_static_token, not the bus.
      return nil
    end

    -- chat.complete.result / chat.error / chat.create / chat.append and
    -- other prefixed envelopes pass through unchanged: their kind stays
    -- prefixed so callers can pattern-match without losing shape.
    return body
  end

  -- bus -> binary. Returns body|nil; nil drops the delivery.
  local function inbound(env)
    if type(env) ~= "table" or env.type ~= "event"
        or type(env.body) ~= "table" then
      return nil
    end

    -- Don't echo back envelopes we ourselves published onto the bus.
    if env.from == name then return nil end

    local body = {}
    for k, v in pairs(env.body) do body[k] = v end

    local k = body.kind
    if type(k) ~= "string" then return body end

    -- Track live-path chat.create so an in-process /resume (which replays
    -- the same chat.create through replay_rebuild) can skip the
    -- duplicate delivery — the binary's chats.create errors on dup ids.
    if k == kinds.chat_create and type(body.chat_id) == "string" then
      owned[body.chat_id] = true
      return body
    end

    -- The openai-provider binary doesn't speak the UI-shaped prompt
    -- contract: prompts arrive via tool.invoke + the binary's own
    -- chat.complete flow. Drop on delivery so a stale fan-out wiring
    -- can't accidentally re-introduce the legacy path. Single-chat
    -- cancel still uses chat.interrupt below.
    if k == "chat.input.submit" or k == "chat.interrupt_all" then
      return nil
    elseif k == "chat.interrupt" then
      body.kind = kinds.interrupt
      return body
    elseif k == "chat.reset" then
      body.kind = kinds.reset
      return body
    elseif k == "chat.auth.set" then
      if body.provider ~= name then return nil end
      return { kind = kinds.auth_set, token = body.token }
    elseif k == "chat.login_requested" then
      if body.provider ~= name then return nil end
      return { kind = kinds.login_requested }
    elseif k == "chat.logout_requested" then
      if body.provider ~= name then return nil end
      return { kind = kinds.logout_requested }
    elseif k == "chat.model.list_requested" then
      if body.provider ~= name then return nil end
      return { kind = kinds.models_list_requested }
    elseif k == "chat.model.set" then
      if body.provider ~= name then return nil end
      -- Return the bare provider+model body; the caller threads any
      -- active chat_id in before handing to deliver.
      return {
        kind  = kinds.model_set,
        model = body.model,
      }
    end

    -- Pass-through for any other envelope (already-prefixed kinds like
    -- <name>.chat.append re-fed by replay, or canonical envelopes the
    -- binary tolerates).
    return body
  end

  local function publish(from, body)
    nefor.engine.send(json.encode({
      type = "event",
      from = from,
      ts   = nefor.engine.now(),
      body = body,
    }))
  end

  local function deliver(body)
    nefor.engine.deliver(name, json.encode({
      type = "event",
      from = "engine",
      ts   = nefor.engine.now(),
      body = body,
    }))
  end

  -- Once, when the binary's <prefix>.ready first arrives and
  -- opts.static_token is set, deliver an auth.set direct to the peer
  -- (don't pollute the bus log; auth.set is a targeted control
  -- envelope). Idempotent — second ready is a no-op.
  -- Returns true if an injection happened, false otherwise.
  local function maybe_inject_static_token(env, opts)
    if injected_static then return false end
    if type(env) ~= "table" or type(env.body) ~= "table" then return false end
    if env.body.kind ~= kinds.ready then return false end
    if type(opts) ~= "table" then return false end
    local token = opts.static_token
    if token == nil then return false end
    injected_static = true
    nefor.engine.deliver(name, json.encode({
      type = "event", from = "engine", ts = nefor.engine.now(),
      body = { kind = kinds.auth_set, token = token },
    }))
    return true
  end

  return {
    name                       = name,
    kinds                      = kinds,
    outbound                   = outbound,
    inbound                    = inbound,
    publish                    = publish,
    deliver                    = deliver,
    maybe_inject_static_token  = maybe_inject_static_token,
  }
end

-- Cross-process resume: filter recorded session envelopes down to the
-- ones that carry chat state for THIS provider and deliver them to the
-- binary to rebuild its per-chat_id history table.
--
-- Per-kind behaviour:
--   <prefix>.chat.create : delivered verbatim; skip if already owned.
--   <prefix>.chat.append : delivered verbatim, gated on ownership so
--                          coexisting providers don't double-feed.
--   tool.result          : synthesize an assistant <prefix>.chat.append
--                          (text + tool_calls) so the assistant turn
--                          lands in history. chat.complete is
--                          intentionally not re-delivered on replay, so
--                          there's no other channel for the turn.
-- Everything else drops.
function M.replay_rebuild(env, name)
  assert(type(name) == "string" and #name > 0,
    "openai-provider.replay_rebuild: name required")
  if type(env) ~= "table" or env.type ~= "event"
      or type(env.body) ~= "table" then
    return
  end

  local body = env.body
  local k = body.kind
  if type(k) ~= "string" then return end

  local prefix = name .. "."
  local owned = owned_for(name)

  local function deliver_body(b)
    nefor.engine.deliver(name, json.encode({
      type = "event",
      from = "engine",
      ts   = nefor.engine.now(),
      body = b,
    }))
  end

  if k == prefix .. "chat.create" then
    local cid = body.chat_id
    if type(cid) == "string" and owned[cid] then return end
    if type(cid) == "string" then owned[cid] = true end
    deliver_body(body)
    return
  end

  if k == prefix .. "chat.append" then
    local cid = body.chat_id
    if type(cid) ~= "string" or not owned[cid] then return end
    deliver_body(body)
    return
  end

  if k == "tool.result" then
    if body.error ~= nil then return end
    local result = body.result
    if type(result) ~= "table" then return end
    local ns = result.next_state
    if type(ns) ~= "table" then return end
    local cid = ns.chat_id
    if type(cid) ~= "string" or not owned[cid] then return end

    -- Empty-string text + missing tool_calls is the sentinel for "no
    -- assistant turn to record" (e.g. a control-only tool.result).
    local text = type(result.text) == "string" and result.text or ""
    local tcs = result.tool_calls
    local has_text = #text > 0
    local has_tcs = type(tcs) == "table" and #tcs > 0
    if not has_text and not has_tcs then return end

    local message = { role = "assistant", content = text }
    if has_tcs then message.tool_calls = tcs end
    deliver_body({
      kind    = prefix .. "chat.append",
      chat_id = cid,
      message = message,
    })
    return
  end
end

return M
