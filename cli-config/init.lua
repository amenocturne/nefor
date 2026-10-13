-- cli-config/init.lua — engine composition for the agentic-cli plugin.
--
-- Mirrors `examples/nefor-agent/init.lua` post-Phase-3a but:
--   * No nefor-tui (the CLI surface IS stdout).
--   * Registers a virtual `agentic-cli` plugin via nefor.plugins.spawn
--     directly (the engine dispatches to it via
--     `nefor plugin agentic-cli [args...]`).
--
-- Run:
--   ./target/debug/nefor --config cli-config/ plugin agentic-cli "your prompt"

local CONFIG_ROOT = NEFOR_CONFIG_DIR or "."

-- Reuse the modules that live in examples/nefor-agent/. Add examples/nefor-agent/ to the
-- package path so require() resolves there.
local PROJECT_ROOT = CONFIG_ROOT:match("^(.*)/[^/]+$") or "."
local STARTER_ROOT = PROJECT_ROOT .. "/examples/nefor-agent"
local LUA_ROOT = PROJECT_ROOT .. "/lua"

package.path = table.concat({
  CONFIG_ROOT .. "/?.lua",
  CONFIG_ROOT .. "/?/init.lua",
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  LUA_ROOT .. "/?.lua",
  LUA_ROOT .. "/?/init.lua",
  package.path,
}, ";")

-- nefor-pm wires the core primitives, generic libs, and every plugin
-- lib. The `dir` overrides skip the clone path; pm registers each dir
-- and puts it on package.path so `require("<name>")` resolves to the
-- plugin lib. Starter composers live as per-domain files (provider,
-- tools) at the starter root and are reached via plain
-- `require("<name>")`.
local pm = require("nefor-pm")
pm.install({
  -- Multi-consumer protocol primitives.
  {
    "amenocturne/nefor",
    name = "core",
    tag  = "v0.1.5",
    path = "lua/core/",
    dir  = LUA_ROOT .. "/core",
  },

  -- Independent generic libs (no plugin binary, no cross-deps beyond core).
  {
    "amenocturne/nefor",
    name = "libs",
    tag  = "v0.1.5",
    path = "lua/libs/",
    dir  = LUA_ROOT .. "/libs",
  },

  {
    "amenocturne/nefor",
    name = "openai-provider",
    tag  = "v0.1.5",
    path = "plugins/openai-provider/lua/openai-provider/",
    dir  = PROJECT_ROOT .. "/plugins/openai-provider/lua/openai-provider",
  },

  {
    "amenocturne/nefor",
    name = "tool-gate",
    tag  = "v0.1.5",
    path = "plugins/tool-gate/lua/tool-gate/",
    dir  = PROJECT_ROOT .. "/plugins/tool-gate/lua/tool-gate",
  },

})
pm.register({
  { name = "nefor-mag", dir = PROJECT_ROOT .. "/mag" },
})

local cfg = require("config").active
local MAG_PACKAGE_ROOT = pm.root("nefor-mag")
local MAG_MODULE_ROOTS = {
  MAG_PACKAGE_ROOT .. "/lib",
  STARTER_ROOT .. "/mag/lib",
}

local ncp      = require("core.ncp")
local actor    = require("core.actor")
local sessions = require("libs.sessions")
local sessions_root = os.getenv("NEFOR_SESSIONS_DIR")
if sessions_root == nil or sessions_root == "" then
  sessions_root = nefor.fs.data_root() .. "/sessions"
end
sessions.configure { root = sessions_root }
function dispatch(current_log)
  ncp.dispatch(current_log)
end

function invoke_from_plugin(source, payload)
  ncp.invoke_from_plugin(source, payload)
end

actor.install()

-- The CLI config lives beside starter rather than containing its own MAG
-- library tree. Seed session workspaces from the shared starter library.
require("libs.mag-workspace").configure {
  sessions_root = sessions_root,
}

actor.spawn(sessions)
local conversation_service = require("libs.conversation-manager.service").new()
local conversation_reader = conversation_service:reader()
actor.spawn(require("libs.conversation-manager.runtime").build({
  service = conversation_service,
}))

local agentic_cli = require("libs.cli")
agentic_cli.configure {
  initialize_session = function(session_id) sessions.init(session_id) end,
  readiness = {
    required_plugins = { cfg.provider.name, "mag", "tool-gate", "basic-tools" },
    required_tools = {
      "read_file", "read_image", "write_file", "process.exec", "shell.script",
      "mag-status", "mag-await", "mag-terminate", "write-review",
      "mag-write-file", "mag-preview", "mag-apply",
    },
    tool_sources = {
      ["basic-tools"] = { "read_file", "read_image", "write_file", "process.exec", "shell.script" },
      ["lead-workflow"] = { "mag-status", "mag-await", "mag-terminate", "write-review", "mag-write-file", "mag-preview", "mag-apply" },
    },
    timeout_ms = tonumber(os.getenv("NEFOR_STARTUP_TIMEOUT_MS")) or 10000,
  },
}

-- ------------------------------------------------------------------
-- Plugin spawn order (mirrors examples/nefor-agent/init.lua minus chat/tui).
-- ------------------------------------------------------------------

require("libs.generic-provider").declare()
require("libs.generic-tool").declare()

local agentic_loop = require("libs.agentic-loop")
local cli_system = [[
You are a helpful assistant. For decomposition tasks (multiple independent sub-questions whose answers roll up into one), create a MAG program with `mag-write-file`, then run it with `mag-apply` and no run_id. The run's result arrives automatically as a follow-up turn — after applying, stop and wait for it. For simple chat turns, just answer directly.
]]
local mag_context = require("libs.mag-context").new {
  guides = {
    { title = "MAG in Five Minutes", path = MAG_PACKAGE_ROOT .. "/book/01. core/00. MAG in Five Minutes.md" },
    { title = "Nefor MAG in Five Minutes", path = MAG_PACKAGE_ROOT .. "/book/02. nefor/00. Nefor MAG in Five Minutes.md" },
  },
  book_path = MAG_PACKAGE_ROOT .. "/book/README.md",
  module_roots = {
    { name = "nefor-mag", path = MAG_PACKAGE_ROOT .. "/lib" },
    { name = "config", path = STARTER_ROOT .. "/mag/lib" },
  },
  -- Embedded: what every workflow is written against (this configuration's
  -- agent module, the data types agents and commands exchange, and the shell
  -- script step). Listed with
  -- their path only: the modules a task may or may not need.
  public_modules = {
    STARTER_ROOT .. "/mag/lib/agents.mag",
    MAG_PACKAGE_ROOT .. "/lib/nefor/contracts.mag",
    MAG_PACKAGE_ROOT .. "/lib/nefor/shell.mag",
    { path = MAG_PACKAGE_ROOT .. "/lib/nefor.mag", embed = false },
    { path = MAG_PACKAGE_ROOT .. "/lib/nefor/workflow.mag", embed = false },
    { path = MAG_PACKAGE_ROOT .. "/lib/nefor/process.mag", embed = false },
    { path = MAG_PACKAGE_ROOT .. "/lib/nefor/path.mag", embed = false },
    { path = MAG_PACKAGE_ROOT .. "/lib/nefor/worktree.mag", embed = false },
    { path = MAG_PACKAGE_ROOT .. "/lib/nefor/collections.mag", embed = false },
    { path = MAG_PACKAGE_ROOT .. "/lib/nefor/retry.mag", embed = false },
    { path = MAG_PACKAGE_ROOT .. "/lib/nefor/human.mag", embed = false },
    { path = MAG_PACKAGE_ROOT .. "/lib/nefor/actors.mag", embed = false },
  },
  toolsets = {
    manifest = STARTER_ROOT .. "/mag/lib/nefor/toolsets.json",
    names = {
      { key = "read_only", name = "nefor.read_only_tools" },
      { key = "general", name = "nefor.general_tools" },
    },
  },
}
agentic_loop.configure {
  provider = cfg.provider.name,
  model    = cfg.provider.model,
  system   = cli_system,
  ambient_context = mag_context,
  -- The lead's turn-program ships in examples/nefor-agent/ (the CLI config reuses the
  -- starter modules; the program lives beside them).
  lead_program = {
    source_dir = STARTER_ROOT,
    entry      = "agentic-loop/lead-turn.mag",
    module_roots = MAG_MODULE_ROOTS,
  },
}
actor.spawn(agentic_loop)

local PROVIDER_NAME  = cfg.provider.name
local PROVIDER_MODEL = cfg.provider.model

local provider = require("libs.compositors.provider")
-- mock-plugin implements the same wire contract as provider plugins, so the
-- complete CLI flow remains deterministic while exercising the real boundary.
actor.spawn(provider.spawn_spec(
  PROVIDER_NAME,
  {
    require("config").bin("mock-plugin"),
    "--script", STARTER_ROOT .. "/" .. cfg.provider.mock_script,
    "--lua-root", LUA_ROOT,
  },
  { agentic_loop = agentic_loop, conversations = conversation_reader }
))

-- mag: the MAG actor-kernel runtime — the lead's turn-programs execute
-- here (agentic-loop spawns one per user message). Mirrors the starter
-- composition: the plugin ships its own kernel (resolved off --lua-root's
-- parent), gate identity threaded, shared Lua tree for output-persistence.
actor.spawn(actor.identity_spec("mag", {
  require("config").bin("mag-plugin"),
  "--tool-gate", "tool-gate",
  "--lua-root", LUA_ROOT,
}))

local tools = require("libs.compositors.tools")
local model_context_policy = require("libs.model-context-policy")
local tool_gate_argv = { require("config").bin("tool-gate") }

-- lead-workflow owns the lead's kernel-dispatch tool surface (mag /
-- MAG lifecycle/source tools and write-review) and relays
-- kernel-run completions back into agentic-loop's deferred queue.
-- Mirrors examples/nefor-agent/init.lua: registered BEFORE tool-gate's spawn so its
-- bus subscription is live when tool-gate.hello arrives — otherwise
-- the advertise is missed and the lead gets "no such tool" at runtime.
local lead_workflow = require("libs.lead-workflow")
lead_workflow.configure {
  dependency_module_roots = MAG_MODULE_ROOTS,
  ambient_context = mag_context,
  agent_system = cli_system,
  resolve_model_snapshot = agentic_loop.model_snapshot,
}
actor.spawn(lead_workflow)

actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))

-- Deterministic fixture policy: this configuration has no command classifier
-- and nobody to ask. A Default caller's commands run, as every tool does here;
-- a ReadOnly caller's commands cannot be shown to be read-only.
local approval = require("libs.tool.approval")
local function fixture_classification(policy)
  if policy == approval.Default then return approval.safe end
  return approval.unsure, "this fixture configuration has no command classifier"
end
local basic_tools = tools.basic_actor_spec {
  max_read_bytes = model_context_policy.item_limit,
  classify_command = fixture_classification,
  classify_argv = fixture_classification,
  -- Nobody reviews plans in this fixture: a Default caller's writes run.
  decide_write = "safe",
}
actor.spawn(basic_tools)
lead_workflow.configure_approval {
  decide_capability = require("libs.tool").decision { basic_tools.source },
}

-- ------------------------------------------------------------------
-- Virtual agentic-cli plugin — calls nefor.plugins.spawn directly to
-- pass the `cli` field (actor.spawn / ncp.spawn don't accept it).
-- ------------------------------------------------------------------

nefor.plugins.spawn {
  name = "agentic-cli",
  cli  = function(argv) return agentic_cli.run(argv) end,
}
