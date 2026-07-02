-- starter/mag-kernel/init.lua — MAG actor-kernel entry.
--
-- Loaded by the `mag` plugin's embedded Lua VM at startup. This module is
-- the wiring layer: it adapts the host `nefor` surface into the plain
-- dependencies the kernel modules expect, constructs the inventory (which
-- owns the fold over graph modifications — see inventory.lua,
-- plugins/mag/docs/actor-model.md, docs/ir.md), and returns the kernel
-- table the plugin holds for the session.
--
-- `nefor.log` is a host binding that writes to the plugin's tracing
-- subscriber (stderr). The kernel must never write to stdout — that is the
-- NCP wire. The host sets `package.path` to this directory before loading,
-- so sibling modules resolve by bare name (`require("inventory")`).

local inventory = require("inventory")

-- Adapt the host's single `nefor.log(msg)` function into the leveled sink
-- the kernel modules expect. Level travels as a prefix so the one host
-- binding covers info/warn/error without a wider native surface.
local function make_logger()
  local function at(level)
    return function(msg)
      nefor.log(string.format("[%s] %s", level, msg))
    end
  end
  return { info = at("info"), warn = at("warn"), error = at("error") }
end

nefor.log("mag-kernel loading")

local inv = inventory.new({ log = make_logger() })

nefor.log("mag-kernel ready")

return {
  name = "mag-kernel",

  -- Apply one graph modification through the fold. Strictly serialized —
  -- one call, one modification (docs/ir.md). Returns { ok = true } or
  -- { ok = false, error = "..." }.
  apply = function(mod)
    return inv.apply(mod)
  end,

  -- Read-only introspection for the host / tests.
  state_of = function(id)
    return inv.state_of(id)
  end,
  actor = function(id)
    return inv.get(id)
  end,

  -- The inventory itself, for wiring the factory registry and routing in
  -- their own tasks without re-reaching through this table.
  inventory = inv,
}
