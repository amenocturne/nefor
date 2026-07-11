-- plugins/mag/lua/mag-kernel/inventory.lua — the actor inventory and the fold.
--
-- One kernel holds one inventory: a single map from actor id to instance
-- record, shared across all factories (actor-model.md). This module owns
-- the fold over graph modifications (docs/ir.md, "The fold" and
-- "Application semantics"): each modification is validated, then applied
-- atomically — spawns, sends, kills — with monotone per-id lifecycles.
--
-- It is deliberately free of host/bus knowledge. All side effects it needs
-- are injected: a `log` sink (info/warn/error), an `on_spawn` hook fired at
-- registration (the observer emits `mag.actor_spawned` through it), and a
-- `deliver` hook that hands live-target sends to the routing layer. That
-- keeps the fold unit-testable in a bare Lua VM (hooks absent → records stand
-- in as placeholders, sends queue directly) and keeps this file mergeable
-- alongside the sibling routing/registry modules.
--
-- Construction is NOT the fold's concern: a spawn only registers the spec
-- (id, factory, params, routes). The routing layer constructs the instance
-- lazily, at the actor's first satisfied input contract (actor-model.md,
-- Lifecycle). Initial rule registration belongs to the run composition in
-- init.lua; the fold accepts only actor/message/kill deltas.

local kinds = require("kinds")

local M = {}

-- Per-id lifecycle states. Absence from the map is the third state,
-- "never-existed"; every id moves never-existed -> alive -> dead, each
-- transition at most once (docs/ir.md, "Monotone lifecycles").
local ALIVE = "alive"
local DEAD = "dead"

-- ---------------------------------------------------------------------------
-- small structural helpers
-- ---------------------------------------------------------------------------

local function is_array(t)
  if type(t) ~= "table" then
    return false
  end
  local n = 0
  for k in pairs(t) do
    if type(k) ~= "number" then
      return false
    end
    n = n + 1
  end
  -- contiguous 1..n (or empty)
  for i = 1, n do
    if t[i] == nil then
      return false
    end
  end
  return true
end

local function deep_equal(a, b)
  if a == b then
    return true
  end
  if type(a) ~= "table" or type(b) ~= "table" then
    return false
  end
  for k, v in pairs(a) do
    if not deep_equal(v, b[k]) then
      return false
    end
  end
  for k in pairs(b) do
    if a[k] == nil then
      return false
    end
  end
  return true
end

-- The comparable core of an actor spec — factory, params, routes. `id` is
-- excluded (it is the key), so this answers "is this the *same* spec?".
local function spec_of(actor)
  return { factory = actor.factory, params = actor.params or {}, routes = actor.routes or {} }
end

-- ---------------------------------------------------------------------------
-- validation — pure, no mutation (docs/ir.md, "every modification is
-- validated before applying"). Returns nil on success or an error string.
-- ---------------------------------------------------------------------------

local function validate_actor_shape(actor, idx)
  if type(actor) ~= "table" then
    return string.format("actors[%d] is not a table", idx)
  end
  if type(actor.id) ~= "string" or actor.id == "" then
    return string.format("actors[%d] missing string id", idx)
  end
  if type(actor.factory) ~= "string" or actor.factory == "" then
    return string.format("actor '%s' missing string factory", actor.id)
  end
  if actor.params ~= nil and type(actor.params) ~= "table" then
    return string.format("actor '%s' params must be a table", actor.id)
  end
  if actor.routes ~= nil then
    if type(actor.routes) ~= "table" then
      return string.format("actor '%s' routes must be a table", actor.id)
    end
    for typ, dests in pairs(actor.routes) do
      if type(typ) ~= "string" then
        return string.format("actor '%s' route key must be a type string", actor.id)
      end
      if not is_array(dests) then
        return string.format("actor '%s' route '%s' must be an array of ids", actor.id, typ)
      end
      for _, d in ipairs(dests) do
        if type(d) ~= "string" then
          return string.format("actor '%s' route '%s' has a non-string destination", actor.id, typ)
        end
      end
    end
  end
  return nil
end

-- Route/port contract validation via the injected registry (nil when no
-- registry is wired — the bare-VM fold path). Destinations resolve against
-- the POST-APPLY actor set: ids spawned in this modification (the registry
-- resolves those from `mod.actors` itself) plus the live inventory (the
-- resolver below). Returns nil on success or the joined error string.
local function validate_routes(self, mod)
  if not self.registry then
    return nil
  end
  local result = self.registry:validate_modification(mod, function(dest_id)
    local entry = self.actors[dest_id]
    if not entry then
      return nil
    end
    return entry.factory, entry.state
  end)
  if result.ok then
    return nil
  end
  return table.concat(result.errors, "; ")
end

-- Validate the whole modification against `self` (live inventory). On the
-- first problem returns nil + an error string; otherwise returns the set of
-- ids spawned by this modification (so message-target checks can treat
-- created-in-this-modification ids as valid).
local function validate(self, mod)
  if type(mod) ~= "table" then
    return nil, "modification is not a table"
  end

  local actors = mod.actors or {}
  local messages = mod.messages or {}
  local kills = mod.kills or {}
  local rules = mod.rules or {}

  if not is_array(actors) then
    return nil, "actors must be an array"
  end
  if not is_array(messages) then
    return nil, "messages must be an array"
  end
  if not is_array(kills) then
    return nil, "kills must be an array"
  end
  if not is_array(rules) then
    return nil, "rules must be an array"
  end

  -- Initial subscriptions are registered by init.lua before the first fold.
  -- A later delta cannot mutate that immutable subscription set.
  if #rules > 0 then
    return nil, "rules are immutable initial subscriptions"
  end

  -- actors: shape + intra-modification id uniqueness. A single
  -- modification naming one id twice is an authoring/lowering bug, not a
  -- race, so it is a hard rejection (distinct from the cross-modification
  -- duplicate-spawn no-op handled at apply time).
  local spawned = {}
  for idx, actor in ipairs(actors) do
    local err = validate_actor_shape(actor, idx)
    if err then
      return nil, err
    end
    if spawned[actor.id] then
      return nil, string.format("id collision: '%s' spawned twice in one modification", actor.id)
    end
    spawned[actor.id] = true
  end

  -- kills: must be strings.
  for idx, id in ipairs(kills) do
    if type(id) ~= "string" or id == "" then
      return nil, string.format("kills[%d] is not an id string", idx)
    end
  end

  -- messages: shape + target reachability. A target is valid when it
  -- exists in the inventory (alive or dead) or is created within this same
  -- modification (docs/ir.md). Never-existed targets are program bugs and
  -- reject the modification; dead targets are race artifacts (the sender
  -- computed the send while the target lived) and drop at execution as
  -- logged no-ops.
  for idx, msg in ipairs(messages) do
    if type(msg) ~= "table" then
      return nil, string.format("messages[%d] is not a table", idx)
    end
    if type(msg.to) ~= "string" or msg.to == "" then
      return nil, string.format("messages[%d] missing string 'to'", idx)
    end
    local entry = self.actors[msg.to]
    if entry == nil and not spawned[msg.to] then
      return nil, string.format("unknown message target '%s'", msg.to)
    end
    -- Control-plane reply injection (plugins/mag/docs/actor-model.md, The
    -- approval boundary): a `mag.ApprovalReply` message can only answer an
    -- outstanding request, and an outstanding request implies a CONSTRUCTED
    -- gate — the request is emitted inside the gate's first activation. A
    -- reply at a registered-but-unconstructed target (an actor spawned in
    -- this same modification included) is a control-plane protocol error,
    -- rejected loudly here; parking it would let a stale reply resolve a
    -- FUTURE request the human never saw. A reply at a DEAD target stays a
    -- race artifact (the gate resolved/was killed while the reply was in
    -- flight) and drops at execution as a logged no-op, like any other send.
    -- Checked only when the composition wires a construction probe
    -- (set_is_constructed; the bare-VM fold path has no routing layer).
    if self.is_constructed
        and type(msg.content) == "table"
        and msg.content.kind == kinds.ApprovalReply then
      local unconstructed_alive = entry ~= nil
          and entry.state == ALIVE
          and not self.is_constructed(msg.to)
      if entry == nil or unconstructed_alive then
        return nil, string.format(
          "'%s' rejected: target '%s' has no outstanding approval request "
          .. "(the actor never constructed — a reply answers a request)",
          kinds.ApprovalReply, msg.to)
      end
    end
  end

  -- routes: contract validation against the injected registry (when wired —
  -- init.lua passes it; bare-VM fold tests run without one). Every spawned
  -- actor's route keys must be declared outputs (or the reserved
  -- kernel-synthesized status tags), and every destination — spawned in this
  -- modification OR already in the inventory — must declare an input port
  -- accepting the routed tag. A violation REJECTS the modification with the
  -- registry's precise wiring error, so a route no port accepts can never
  -- reach the delivery layer's drop path (registry.lua,
  -- validate_modification).
  local err = validate_routes(self, mod)
  if err then
    return nil, err
  end

  return spawned, nil
end

-- ---------------------------------------------------------------------------
-- execution — mutates the inventory. Only reached after validate passed,
-- so shape and targets are already sound. Order: spawns, then sends, then
-- kills, so a message addressed to an id created in the same modification
-- finds its spec (routes, input contract) already registered.
-- ---------------------------------------------------------------------------

-- Register a new actor record. Returns true when a fresh id was created,
-- false for a monotone no-op (already alive, or dead and unrevivable). No
-- instance is built here: construction is lazy — the routing layer constructs
-- via the factory at the actor's first satisfied input contract
-- (actor-model.md, Lifecycle).
local function do_spawn(self, actor)
  local existing = self.actors[actor.id]
  if existing then
    -- Monotone lifecycle: an id that already exists (alive or dead) never
    -- re-enters. Spawn is a no-op; the flavor decides the log level.
    if existing.state == ALIVE then
      if deep_equal(existing.spec, spec_of(actor)) then
        self.log.info(string.format("spawn no-op: '%s' already alive (identical spec)", actor.id))
      else
        self.log.warn(string.format("spawn no-op: '%s' already alive with a different spec", actor.id))
      end
    else
      self.log.warn(string.format("spawn no-op: '%s' is dead and cannot be respawned", actor.id))
    end
    return false
  end

  -- New id: register the spec record. `routes` are retained verbatim for the
  -- routing layer; the record also opens a mailbox used only by the hook-less
  -- bare-VM path (see do_send). The on_spawn hook surfaces the registration
  -- (the observer emits mag.actor_spawned through it) before any send in the
  -- same modification can fire the actor.
  self.actors[actor.id] = {
    id = actor.id,
    factory = actor.factory,
    params = actor.params or {},
    routes = actor.routes or {},
    spec = spec_of(actor),
    state = ALIVE,
    mailbox = {},
  }
  self.on_spawn(self.actors[actor.id])
  return true
end

local function do_send(self, msg)
  local entry = self.actors[msg.to]
  -- validate guaranteed the target exists (alive or just created) or is
  -- dead. A live target routes through the injected `deliver` hook, the
  -- single delivery decision point: routing feeds the firing machine, which
  -- buffers partial product inputs and constructs + fires the actor when its
  -- input contract is satisfied (actor-model.md). Absent a hook (bare-VM fold
  -- tests) the send queues in the record's mailbox — there is no routing
  -- layer to fire into. A dead target drops the send as a logged no-op — the
  -- race artifact of first-applied-wins.
  if entry and entry.state == ALIVE then
    if self.deliver then
      self.deliver(msg.to, "mag.control", msg.content)
    else
      entry.mailbox[#entry.mailbox + 1] = msg.content
    end
  else
    self.log.info(string.format("send dropped: target '%s' is dead", msg.to))
  end
end

local function do_kill(self, id)
  local entry = self.actors[id]
  if entry == nil then
    -- Kill on a never-existed id: uniform no-op, logged.
    self.log.warn(string.format("kill no-op: '%s' never existed", id))
    return
  end
  if entry.state == DEAD then
    self.log.info(string.format("kill no-op: '%s' already dead", id))
    return
  end
  -- Unroute; the id stays in the map as a tombstone so the lifecycle stays
  -- monotone (spawn can never revive it). A registered-but-unconstructed
  -- actor dies the same way: the spec drops, no instance ever exists.
  entry.state = DEAD
  entry.mailbox = {}
  entry.routes = {}
  -- Notify the routing layer so it drops the instance (when one was
  -- constructed), the firing slots, and outstanding capability correlations
  -- (kill drops slots; actor-model.md).
  self.on_kill(id)
end

-- Apply order (docs/ir.md, "Application semantics"): register spawns, then
-- sends, then kills. Registration precedes sends so every route and input
-- contract in the modification resolves before the first delivery can
-- construct + fire an actor. Returns the set of ids spawned by this
-- modification.
local function execute(self, mod)
  local created = {}
  for _, actor in ipairs(mod.actors or {}) do
    if do_spawn(self, actor) then
      created[actor.id] = true
    end
  end
  for _, msg in ipairs(mod.messages or {}) do
    do_send(self, msg)
  end
  for _, id in ipairs(mod.kills or {}) do
    do_kill(self, id)
  end
  return created
end

-- ---------------------------------------------------------------------------
-- public surface
-- ---------------------------------------------------------------------------

-- Apply one modification: validate, then (if valid) execute atomically.
-- Strictly serialized by construction — one call, one modification, no
-- interleaving (docs/ir.md, "Serialized and atomic"). Returns
--   { ok = true }                on success
--   { ok = false, error = "..." } on a rejected modification
-- A rejection leaves the inventory untouched and the run continues.
function M.apply(self, mod)
  local _spawned, err = validate(self, mod)
  if err then
    self.log.error(string.format("modification rejected: %s", err))
    return { ok = false, error = err }
  end
  local created = execute(self, mod)
  return { ok = true, spawned = created }
end

-- Drop every actor record, dead tombstones included. Monotone lifecycles are
-- scoped to ONE run, and the composition layer enforces that by giving each
-- run its own inventory (init.lua run contexts) — a new run's fold starts
-- from never-existed by construction, so nothing calls this per run anymore.
-- Kept for bare-VM fold tests that reuse one inventory across scenarios.
-- Live actors must be killed through a modification FIRST (so kill handlers
-- run and the routing layer forgets them — do_kill's on_kill seam); clear
-- itself runs no hooks.
function M.clear(self)
  self.actors = {}
end

-- Read-only lifecycle probe: "alive" | "dead" | "never-existed".
function M.state_of(self, id)
  local entry = self.actors[id]
  if entry == nil then
    return "never-existed"
  end
  return entry.state
end

-- The live instance record for `id`, or nil. Callers must not mutate it.
function M.get(self, id)
  return self.actors[id]
end

-- Iterate every actor record: `for id, record in inv.pairs() do ... end`.
-- Read-only — the routing layer uses it to derive product slots from the
-- routes topology (which senders route which types to a given actor).
function M.pairs(self)
  return pairs(self.actors)
end

-- Register (or replace) the kill notification hook after construction, so the
-- inventory and the routing layer can be wired without a construction-order
-- cycle (routing needs the inventory; the inventory needs routing's forget).
function M.set_on_kill(self, fn)
  self.on_kill = fn or function() end
end

-- Register (or replace) the spawn notification hook: fn(record) fires at
-- registration, inside the fold's spawn pass — before any send in the same
-- modification can construct or fire the actor. The observer wires this to
-- emit `mag.actor_spawned`, keeping the wire order truthful (spawned strictly
-- before the ready an activation may trigger).
function M.set_on_spawn(self, fn)
  self.on_spawn = fn or function() end
end

-- Register the construction probe: fn(id) -> bool, true when a bound instance
-- exists (routing.lua is_constructed). Consulted by validate for control-plane
-- reply injection (a `mag.ApprovalReply` message requires a constructed
-- target). Optional — absent (bare-VM fold tests), the check is skipped.
function M.set_is_constructed(self, fn)
  self.is_constructed = fn
end

local noop = function() end

-- Construct an inventory. `opts.log` is a sink with info/warn/error
-- functions (defaults to no-ops). `opts.deliver` is the delivery hook,
-- usually set after the router exists (set_deliver) to break the
-- construction-order cycle; it defaults to nil (bare-VM fold tests run
-- without it, queueing sends in the record mailbox).
function M.new(opts)
  opts = opts or {}
  local log = opts.log or {}
  local self = {
    actors = {},
    log = {
      info = log.info or noop,
      warn = log.warn or noop,
      error = log.error or noop,
    },
    -- Delivery hook: fn(to, from, content) -> routing feeds the firing
    -- machine for a live target. Absent, do_send queues into the mailbox.
    deliver = opts.deliver,
    -- Kill notification hook (actor-model.md, Signals: kill drops slots).
    -- Defaults to a no-op; init.lua rebinds it to routing's dispatch_kill +
    -- forget once both are built (M.set_on_kill), breaking the
    -- construction-order cycle.
    on_kill = opts.on_kill or noop,
    -- Spawn notification hook (set_on_spawn); the observer emits
    -- mag.actor_spawned through it at registration time.
    on_spawn = opts.on_spawn or noop,
    -- Optional factory registry (registry.lua). When present, validate checks
    -- every modification's routes against factory-declared contracts
    -- (validate_routes); absent (bare-VM fold tests), the check is skipped.
    registry = opts.registry,
    -- Optional construction probe (set_is_constructed): fn(id) -> bool.
    -- Consulted by validate for `mag.ApprovalReply` injection; usually wired
    -- after the router exists, like `deliver`.
    is_constructed = opts.is_constructed,
  }
  self.apply = function(mod)
    return M.apply(self, mod)
  end
  self.clear = function()
    return M.clear(self)
  end
  self.state_of = function(id)
    return M.state_of(self, id)
  end
  self.get = function(id)
    return M.get(self, id)
  end
  self.pairs = function()
    return M.pairs(self)
  end
  self.set_on_kill = function(fn)
    return M.set_on_kill(self, fn)
  end
  self.set_on_spawn = function(fn)
    return M.set_on_spawn(self, fn)
  end
  self.set_deliver = function(fn)
    self.deliver = fn
  end
  self.set_is_constructed = function(fn)
    return M.set_is_constructed(self, fn)
  end
  return self
end

return M
