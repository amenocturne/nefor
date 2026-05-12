-- starter/reasoners/run-wrappers.lua — concrete wrappers over the
-- generic `run(bash)` reasoner.
--
-- The orchestrator does not call `run` directly with arbitrary
-- commands; it calls high-level typed names like `compileProject` or
-- `lintDocs`, each of which the wrapper layer maps to a concrete
-- shell-command string. This keeps the orchestrator's tool surface
-- tight and project-aware while reusing one bash primitive.
--
-- This file ships ONE example concrete wrapper, `runCommand`, which
-- takes a `name` arg and resolves it against either a per-call
-- `args.registry` (a `{ name -> command }` table threaded in by the
-- caller) or a starter-level default registry — the demo registry
-- here is `{ list = "ls -la", pwd = "pwd" }`. Project teams override
-- by registering their own wrapper types (e.g. `compileProject` whose
-- registry maps to `sbt --client '...'`) on top of the same `run`
-- primitive. Don't ship project-specific compile/lint commands in
-- the starter.
--
-- ## Dispatch envelope
--
--   tool.invoke {
--     id   = <firing_id>,
--     name = "runCommand",
--     args = {
--       run_id   = <string>,
--       node_id  = <string>,
--       args     = {
--         name     = <string>,            -- key into the registry
--         registry = { name -> command }, -- optional override
--       },
--       inputs   = { ... },         -- ignored
--       prev_state = ...,           -- ignored (single-firing reasoner)
--     }
--   }
--
-- ## Reply envelope (terminal)
--
-- Same shape as `run`'s reply (stdout/stderr/exit_code). The wrapper
-- forwards the inner `run` reasoner's tool.result verbatim — the only
-- mapping is the typed-name → command-string lookup at dispatch.
--
-- ## Internal flow
--
-- On dispatch, resolve `name` against `args.registry` (caller-supplied)
-- with a fallback to the module-level `DEFAULT_REGISTRY`, then dispatch
-- a `tool.invoke { name="run", args={ command=<resolved> } }` against
-- our own firing_id. The `run` reasoner's terminal `tool.result`
-- carries `id = firing_id`, which closes the firing without further
-- bookkeeping in this layer — no correlation map needed.

local envelope = require("core.envelope")

local emit_as = envelope.emit_as

local M = {}

-- Demonstration registry. Project-specific configs supersede this via
-- `args.registry`. Keep it intentionally thin so teams don't grow
-- starter coupling to their command set.
local DEFAULT_REGISTRY = {
  list = "ls -la",
  pwd  = "pwd",
}

-- Resolve a name against the call-time registry (if supplied) and the
-- module-level default registry. Returns nil if absent in both.
local function resolve_command(name, registry)
  if type(registry) == "table" then
    local v = registry[name]
    if type(v) == "string" and #v > 0 then return v end
  end
  local v = DEFAULT_REGISTRY[name]
  if type(v) == "string" and #v > 0 then return v end
  return nil
end

-- ------------------------------------------------------------------
-- dispatch handler — called from reasoners/init.lua
-- ------------------------------------------------------------------

-- Returns nil on accept (reply lands later via the bus through `run`),
-- or a string error to synth-fail the firing.
local function handle(body)
  local firing_id = body.firing_id
  local args = body.args
  if type(args) ~= "table" then
    return "runCommand reasoner: missing args"
  end
  local name = args.name
  if type(name) ~= "string" or #name == 0 then
    return "runCommand reasoner: args.name must be a non-empty string"
  end
  local command = resolve_command(name, args.registry)
  if command == nil then
    return "runCommand reasoner: unknown command name '" .. name .. "'"
  end

  -- Dispatch a fresh tool.invoke against `run` keyed by OUR firing_id;
  -- the `run` reasoner's terminal tool.result will close the same
  -- firing because reasoner-graph correlates on `tool.result.id`.
  emit_as("runCommand", nil, {
    kind = "tool.invoke",
    id   = firing_id,
    name = "run",
    args = {
      args = { command = command },
    },
  })

  return nil
end

M.handle = handle

-- ------------------------------------------------------------------
-- test escape hatch
-- ------------------------------------------------------------------

M._internals = {
  default_registry = DEFAULT_REGISTRY,
  resolve_command  = resolve_command,
}

return M
