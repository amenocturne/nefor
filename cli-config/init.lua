-- cli-config/init.lua — engine composition for the agentic-cli plugin.
--
-- Mirrors `starter/init.lua` post-Phase-3a but:
--   * No nefor-tui (the CLI surface IS stdout).
--   * Registers a virtual `agentic-cli` plugin via nefor.plugins.spawn
--     directly (the engine dispatches to it via
--     `nefor plugin agentic-cli [args...]`).
--
-- Run:
--   ./target/debug/nefor --config cli-config/ plugin agentic-cli "your prompt"
--   NEFOR_CONFIG=test ./target/debug/nefor --config cli-config/ plugin agentic-cli "..."

local CONFIG_ROOT = NEFOR_CONFIG_DIR or "."

-- Reuse the modules that live in starter/. Add starter/ to the
-- package path so require() resolves there.
local PROJECT_ROOT = CONFIG_ROOT:match("^(.*)/[^/]+$") or "."
local STARTER_ROOT = PROJECT_ROOT .. "/starter"
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

local ncp      = require("core.ncp")
local actor    = require("core.actor")
local sessions = require("libs.sessions")
local cfg      = require("config").active

function dispatch(current_log)
  ncp.dispatch(current_log)
end

function invoke_from_plugin(source, payload)
  ncp.invoke_from_plugin(source, payload)
end

actor.install()

-- The CLI config lives beside starter rather than containing its own MAG
-- library tree. Seed session workspaces from the shared starter library.
require("libs.mag-workspace").configure { library_dir = STARTER_ROOT .. "/mag/lib" }

actor.spawn(sessions)
actor.spawn(require("libs.conversation-manager.runtime").build())
sessions.init()

local agentic_cli = require("libs.cli")
agentic_cli.configure {
  readiness = {
    required_plugins = { cfg.provider.name, "mag", "tool-gate", "basic-tools" },
    required_tools = {
      "read_file", "read_image", "write_file", "edit_file", "bash", "search_text",
      "graph-status", "await-run", "terminate-graph", "write-review", "mag", "mag-eval",
    },
    tool_sources = {
      ["basic-tools"] = { "read_file", "read_image", "write_file", "edit_file", "bash", "search_text" },
      ["lead-workflow"] = { "graph-status", "await-run", "terminate-graph", "write-review", "mag", "mag-eval" },
    },
    timeout_ms = tonumber(os.getenv("NEFOR_STARTUP_TIMEOUT_MS")) or 10000,
  },
}

-- ------------------------------------------------------------------
-- Plugin spawn order (mirrors starter/init.lua minus chat/tui).
-- ------------------------------------------------------------------

require("libs.generic-provider").declare()
require("libs.generic-tool").declare()

local agentic_loop = require("libs.agentic-loop")
agentic_loop.configure {
  provider = cfg.provider.name,
  model    = cfg.provider.model,
  system   = [[
You are a helpful assistant. For decomposition tasks (multiple independent sub-questions whose answers roll up into one), use the `mag` tool: write a MAG program to the workspace with action='write', then run it with action='execute'. The run's result arrives automatically as a follow-up turn — after executing, stop and wait for it. For simple chat turns, just answer directly.
]],
  -- The lead's turn-program ships in starter/ (the CLI config reuses the
  -- starter modules; the program lives beside them).
  lead_program = {
    source_dir = STARTER_ROOT,
    entry      = "agentic-loop/lead-turn.mag",
  },
}
actor.spawn(agentic_loop)

local PROVIDER_NAME  = cfg.provider.name
local PROVIDER_MODEL = cfg.provider.model

local provider = require("libs.compositors.provider")
if cfg.plugins.spawn_mock then
  -- mock-plugin uses the same wire protocol as the openai-provider
  -- binary, so the provider actor spec works as-is.
  actor.spawn(provider.spawn_spec(
    PROVIDER_NAME,
    {
      require("config").bin("mock-plugin"),
      "--script", STARTER_ROOT .. "/" .. cfg.provider.mock_script,
    },
    { agentic_loop = agentic_loop }
  ))
else
  local provider_command = {
    require("config").bin("openai-provider"),
    "--name",     PROVIDER_NAME,
    "--base-url", cfg.provider.base_url,
  }
  if PROVIDER_MODEL then
    table.insert(provider_command, "--model")
    table.insert(provider_command, PROVIDER_MODEL)
  end
  for _, a in ipairs(cfg.provider.extra_args or {}) do
    table.insert(provider_command, a)
  end
  actor.spawn(provider.spawn_spec(
    PROVIDER_NAME,
    provider_command,
    { static_token = cfg.provider.static_token, agentic_loop = agentic_loop }
  ))
end

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
local tool_gate_argv = { require("config").bin("tool-gate") }
for _, t in ipairs(cfg.tool_gate.prompt_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--prompt"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
tool_gate_argv[#tool_gate_argv + 1] = "--default"
tool_gate_argv[#tool_gate_argv + 1] = cfg.tool_gate.default_action

-- lead-workflow owns the lead's kernel-dispatch tool surface (mag /
-- mag-eval / graph-status / terminate-graph / write-review) and relays
-- kernel-run completions back into agentic-loop's deferred queue.
-- Mirrors starter/init.lua: registered BEFORE tool-gate's spawn so its
-- bus subscription is live when tool-gate.hello arrives — otherwise
-- the advertise is missed and the lead gets "no such tool" at runtime.
actor.spawn(require("libs.lead-workflow"))

actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))

actor.spawn(tools.basic_actor_spec())

-- ------------------------------------------------------------------
-- Virtual agentic-cli plugin — calls nefor.plugins.spawn directly to
-- pass the `cli` field (actor.spawn / ncp.spawn don't accept it).
-- ------------------------------------------------------------------

nefor.plugins.spawn {
  name = "agentic-cli",
  cli  = function(argv) return agentic_cli.run(argv) end,
}
