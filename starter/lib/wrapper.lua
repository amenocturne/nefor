-- starter/lib/wrapper.lua — helpers for the common wrapper-callback shapes.
--
-- Wrappers' `from_plugin` and `to_plugin` are batched side-effecting
-- callbacks post-refactor: the framework hands a list of parsed
-- envelopes to them and doesn't inspect the return value. The wrapper
-- iterates `envs` and decides whether to publish
-- (`nefor.engine.send`) or deliver (`nefor.engine.deliver`).
--
-- Most wrappers translate inbound and outbound between the bus-canonical
-- envelope shape and the wrapped plugin's native shape. `M.translator`
-- captures that pattern so the boilerplate (loop over envs + build
-- envelope wrapper + json.encode + send / deliver) doesn't repeat in
-- every wrapper.
--
-- Wrappers that need full callback flexibility (multiple deliveries per
-- event, conditional drops based on state, replay-window filtering,
-- side-effects beyond translation) should write callbacks by hand.

local json = nefor.json

local M = {}

-- Build a `from_plugin` / `to_plugin` callback pair that follows the
-- common shape. Each callback receives a LIST of envelopes (`envs`)
-- per dispatch tick; the helper iterates and applies the
-- per-envelope `inbound` / `outbound` translation underneath.
--
--   inbound  (env): bus → peer    — receives the bus envelope; if it
--                                   returns a body table, the wrapper
--                                   delivers `{type=event, from=engine,
--                                   ts=now, body=<returned>}` to the
--                                   peer's stdin via `engine.deliver`.
--                                   Returns nil to skip delivery.
--   outbound (env): peer → bus    — receives the post-decode envelope
--                                   the peer emitted; returns a body
--                                   table → wrapper publishes
--                                   `{type=event, from=opts.name,
--                                   ts=now, body=<returned>}` via
--                                   `engine.send` (broadcast). Returns
--                                   nil to drop the emission entirely
--                                   (it never lands on the bus).
--
-- Both default to identity passthrough when omitted: `from_plugin`
-- republishes each envelope verbatim, `to_plugin` delivers each
-- verbatim. The omitted case still benefits from the helper because
-- the json encoding + targeted-vs-broadcast plumbing lives here.
--
-- Usage:
--   local wrapper = require("lib.wrapper")
--   return wrapper.translator {
--     name     = "openai-provider",
--     outbound = function(env) ... return body end,
--     inbound  = function(env) ... return body end,
--   }
function M.translator(opts)
  assert(type(opts) == "table", "wrapper.translator: opts must be a table")
  assert(type(opts.name) == "string" and #opts.name > 0,
    "wrapper.translator: opts.name required")

  local function from_plugin(envs)
    for _, env in ipairs(envs) do
      if opts.outbound then
        local body = opts.outbound(env)
        if body ~= nil then
          nefor.engine.send(json.encode({
            type = "event",
            from = opts.name,
            ts   = nefor.engine.now(),
            body = body,
          }))
        end
      else
        -- Default: republish verbatim.
        nefor.engine.send(json.encode({
          type = "event",
          from = env.from or opts.name,
          ts   = nefor.engine.now(),
          body = env.body,
        }))
      end
    end
  end

  local function to_plugin(envs)
    for _, env in ipairs(envs) do
      if opts.inbound then
        local body = opts.inbound(env)
        if body ~= nil then
          nefor.engine.deliver(opts.name, json.encode({
            type = "event",
            from = "engine",
            ts   = nefor.engine.now(),
            body = body,
          }))
        end
      else
        -- Default: deliver verbatim. Strip framework-only fields
        -- (`replay`, …) — the protocol parser rejects unknown
        -- envelope fields.
        nefor.engine.deliver(opts.name, json.encode({
          type = env.type,
          from = env.from,
          ts   = env.ts,
          body = env.body,
        }))
      end
    end
  end

  return {
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
  }
end

-- Helper to publish an envelope onto the bus from inside a wrapper
-- callback. Convenience over hand-encoding `{type, from, ts, body}`
-- and calling `nefor.engine.send`. `target` defaults to nil (broadcast).
function M.publish(from, body, target)
  nefor.engine.send(json.encode({
    type = "event",
    from = from,
    ts   = nefor.engine.now(),
    body = body,
  }), target)
end

-- Helper to deliver an envelope to a specific peer's stdin without
-- logging. Convenience wrapper.
function M.deliver(peer, from, body)
  nefor.engine.deliver(peer, json.encode({
    type = "event",
    from = from,
    ts   = nefor.engine.now(),
    body = body,
  }))
end

return M
