-- starter/mag-kernel/inventory.lua — the actor inventory and the fold.
--
-- One kernel holds one inventory: a single map from actor id to instance
-- record, shared across all factories (actor-model.md). This module owns
-- the fold over graph modifications (docs/ir.md, "The fold" and
-- "Application semantics"): each modification is validated, then applied
-- atomically — spawns, sends, kills — with monotone per-id lifecycles.
--
-- It is deliberately free of host/bus knowledge. All side effects it needs
-- are injected: a `log` sink (info/warn/error) and, later, a `factory`
-- hook that constructs real instances. That keeps the fold unit-testable in
-- a bare Lua VM and keeps this file mergeable alongside the factory registry
-- a sibling builds in its own module.
--
-- What is NOT here yet (separate tasks): the ready barrier / factory
-- construction (actor-model.md lifecycle), routing of returned outputs
-- (routes are *retained* per actor but not consumed), and rule evaluation
-- (a non-empty `rules` list is rejected "not implemented").

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

  -- rules: accepted in the shape, but evaluation is a separate task.
  if #rules > 0 then
    return nil, "rules not implemented"
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
  end

  return spawned, nil
end

-- ---------------------------------------------------------------------------
-- execution — mutates the inventory. Only reached after validate passed,
-- so shape and targets are already sound. Order: spawns, then sends, then
-- kills, so a message addressed to an id created in the same modification
-- queues in the freshly-opened mailbox.
-- ---------------------------------------------------------------------------

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
    return
  end

  -- New id: register the instance record and open its pending mailbox.
  -- Factory construction / the ready barrier land later; for now the
  -- record is the instance, `routes` retained verbatim for the (separate)
  -- routing task, and the mailbox holds sends until a factory drains it.
  self.actors[actor.id] = {
    id = actor.id,
    factory = actor.factory,
    params = actor.params or {},
    routes = actor.routes or {},
    spec = spec_of(actor),
    state = ALIVE,
    mailbox = {},
  }
end

local function do_send(self, msg)
  local entry = self.actors[msg.to]
  -- validate guaranteed the target exists (alive or just created) or is
  -- dead. A live target with no factory yet accumulates in its mailbox
  -- (actor-model.md); a dead target drops the send as a logged no-op —
  -- the race artifact of first-applied-wins (docs/ir.md).
  if entry and entry.state == ALIVE then
    entry.mailbox[#entry.mailbox + 1] = msg.content
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
  -- Unroute + drop the pending mailbox; the id stays in the map as a
  -- tombstone so the lifecycle stays monotone (spawn can never revive it).
  entry.state = DEAD
  entry.mailbox = {}
  entry.routes = {}
end

local function execute(self, mod)
  for _, actor in ipairs(mod.actors or {}) do
    do_spawn(self, actor)
  end
  for _, msg in ipairs(mod.messages or {}) do
    do_send(self, msg)
  end
  for _, id in ipairs(mod.kills or {}) do
    do_kill(self, id)
  end
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
  execute(self, mod)
  return { ok = true }
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

local noop = function() end

-- Construct an inventory. `opts.log` is a sink with info/warn/error
-- functions (defaults to no-ops). `opts.factory` is reserved for the
-- factory registry a sibling builds; unused here.
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
    factory = opts.factory,
  }
  self.apply = function(mod)
    return M.apply(self, mod)
  end
  self.state_of = function(id)
    return M.state_of(self, id)
  end
  self.get = function(id)
    return M.get(self, id)
  end
  return self
end

return M
