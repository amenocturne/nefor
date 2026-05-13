-- starter/lib/envelope.lua — envelope construction + id helpers.
--
-- Pure helpers extracted from agentic_workflow.lua during the Phase 1
-- refactor. The behaviour is unchanged: envelopes are stamped with
-- from="engine" + ts via nefor.engine.now() and shipped via
-- nefor.engine.send. A pcall around json.encode catches non-UTF-8
-- payloads, surfaces a chat.popup, and exits the engine cleanly so a
-- broker hang doesn't strand the user.
--
-- `id_seq` lives here because both `uuid_lite()` (this module) and
-- `ids.mint_chat_run_id()` fold the same monotonic counter into their
-- output for collision resistance. ids.lua reads/bumps it via
-- `next_seq()`.

local M = {}

local json = nefor.json

-- Seed math.random at module load so uuid_lite / mint_chat_run_id
-- don't draw from the deterministic Lua-default sequence. os.time() is
-- whole-seconds; mix in os.clock() (sub-second CPU time) and the
-- address of a fresh table for additional entropy across processes
-- spawned in the same wall-clock second.
do
  local addr_byte = string.byte(tostring({}):sub(-2, -2)) or 0
  math.randomseed((os.time() * 1000) + math.floor((os.clock() or 0) * 1e6) + addr_byte)
end

local id_seq = 0
local id_counter = 0

function M.next_seq()
  id_seq = id_seq + 1
  return id_seq
end

function M.next_id(prefix)
  id_counter = id_counter + 1
  return prefix .. "-" .. tostring(id_counter)
end

function M.uuid_lite()
  id_seq = id_seq + 1
  return string.format(
    "rg-%d-%d-%d",
    os.time(),
    id_seq,
    math.random(0, 2 ^ 31 - 1)
  )
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
function M.emit(target, body)
  return M.emit_as("engine", target, body)
end

-- Emit with a custom `from` identity. Used by contract libs that emit
-- on behalf of a canonical-type plugin (e.g. `generic-provider`,
-- `generic-tool`) — combinators reads `envelope.from` to namespace
-- declared types, so Lua-resident type registrations have to set the
-- field to the canonical-plugin name, not the engine.
function M.emit_as(from, target, body)
  local ok, payload = pcall(json.encode, {
    type = "event",
    from = from,
    ts   = nefor.engine.now(),
    body = body,
  })
  if not ok then
    local kind = (type(body) == "table" and tostring(body.kind)) or "(unknown)"
    nefor.log.error("agentic_workflow: json.encode failed — payload not emitted", {
      kind  = kind,
      error = tostring(payload),
    })
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
    if nefor.engine and type(nefor.engine.exit) == "function" then
      nefor.engine.exit(1)
    end
    return
  end
  -- target ~= nil → engine sends to one peer (one log entry).
  -- target == nil → engine broadcasts to every ready peer (still ONE
  -- log entry, with target=None on the broker side). The previous
  -- per-peer loop was a Phase-1 fallback dating from before the
  -- engine binding accepted nil as a broadcast target; that loop
  -- generated N log entries per broadcast, which the actor.lua bus
  -- subscriber then dispatched N times — causing duplicate handling
  -- of every emit_broadcast envelope (e.g. graph.node_result fired
  -- once per peer through the resident reasoners' receive_msg).
  -- Ship one entry; let the broker's targeted/broadcast distinction
  -- handle peer fan-out without polluting the in-VM dispatch path.
  nefor.engine.send(payload, target)
end

-- Test-only: reset module-level counters.
function M._reset()
  id_seq = 0
  id_counter = 0
end

return M
