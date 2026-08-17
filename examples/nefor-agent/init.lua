-- init.lua — starter composition.
--
-- Runtime source and plugin commands are selected by the distribution helper.
-- Installed launchers provide immutable roots; NEFOR_DEV_DIR is an explicit
-- source-checkout override used by `just run`.

local STARTER_ROOT = NEFOR_CONFIG_DIR or "."
package.path = table.concat({
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  package.path,
}, ";")

local distribution = require("config.distribution")
local NEFOR_ROOT = distribution.runtime_root()

local LUA_ROOT = NEFOR_ROOT .. "/lua"

package.path = table.concat({
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  LUA_ROOT .. "/?.lua",
  LUA_ROOT .. "/?/init.lua",
  package.path,
}, ";")

-- nefor-pm registers the core primitives, generic libs, and every plugin
-- lib from the already-selected source root. Registration is read-only: an
-- installed immutable generation must not create links or lockfiles below its
-- writable data root merely to make modules require-able.
local pm = require("nefor-pm")
pm.register({
  { name = "core", dir = NEFOR_ROOT .. "/lua/core" },
  { name = "libs", dir = NEFOR_ROOT .. "/lua/libs" },
  { name = "openai-provider", dir = NEFOR_ROOT .. "/plugins/openai-provider/lua/openai-provider" },
  { name = "chatgpt-provider", dir = NEFOR_ROOT .. "/plugins/chatgpt-provider/lua/chatgpt-provider" },
  { name = "tool-gate", dir = NEFOR_ROOT .. "/plugins/tool-gate/lua/tool-gate" },
  { name = "nefor-tui", dir = NEFOR_ROOT .. "/plugins/nefor-tui/lua" },
})

local ncp            = require("core.ncp")
local actor          = require("core.actor")
local replay_window = require("core.replay_window")
local sessions       = require("libs.sessions")
local sessions_root = os.getenv("NEFOR_SESSIONS_DIR")
if sessions_root == nil or sessions_root == "" then
  sessions_root = nefor.fs.data_root() .. "/sessions"
end
sessions.configure { root = sessions_root }
local cfg            = require("config").active
local lead_role      = require("libs.lead-workflow.role")

function dispatch(current_log)
  ncp.dispatch(current_log)
  local entry = current_log[#current_log]
  if entry and entry.origin == "engine" then
    local ok, decoded = pcall(nefor.json.decode, entry.payload)
    local body = ok and type(decoded) == "table" and decoded.body or nil
    if type(body) == "table" and body.kind == "engine.plugin_process_terminated" then
      plugin_process_terminated(body)
    end
  end
end

function invoke_from_plugin(source, payload)
  ncp.invoke_from_plugin(source, payload)
end

-- Starter lifecycle policy: every spawned-process termination ends this
-- composition. Rust reports the fact before invoking this callback.
function plugin_process_terminated(fact)
  nefor.engine.shutdown {
    code = 0,
    reason = "plugin " .. tostring(fact.plugin) .. " terminated",
    grace_ms = 2000,
  }
end

actor.install()
require("libs.mag-workspace").configure {
  library_dir = STARTER_ROOT .. "/mag/lib",
  sessions_root = sessions_root,
}
-- Defense-in-depth fallback for the synchronous `replay_window.set`
-- path that sessions drives around its replay burst. Wired explicitly
-- here so module load stays free of bus dependencies.
replay_window.install()
actor.spawn(sessions)
local conversation_service = require("libs.conversation-manager.service").new()
local conversation_reader = conversation_service:reader()
actor.spawn(require("libs.conversation-manager.runtime").build({
  service = conversation_service,
}))

local startup_args = require("startup")
local startup = startup_args.parse((nefor.runtime and nefor.runtime.argv) or {})
sessions.init(startup.session_id)

-- Spawn order matters: type-tag registrations must complete before the
-- kernel queries on submit. Order:
--   1. libs.generic-{provider,tool}.declare()
--   2. agentic-loop
--   3. providers
--   4. mag + tool-gate + basic-tools
--   5. lead-workflow
--   6. chat (declarative TUI)

require("libs.generic-provider").declare()
require("libs.generic-tool").declare()

-- The actor runtime queues incoming envelopes during boot, so spawning
-- the orchestrator before the plugins it coordinates is safe even if a
-- plugin's `ready` arrives early.
-- Build runtime context: cwd, workspace index, agentic-kit paths.
-- Appended to the lead system prompt so the agent knows where it's
-- operating and what projects are available.
local function build_runtime_context()
  local parts = {}

  local cwd = nefor and nefor.fs and nefor.fs.cwd and nefor.fs.cwd()
  if not cwd then
    local p = io.popen("pwd")
    if p then cwd = p:read("*l"); p:close() end
  end
  if cwd then
    parts[#parts + 1] = "## Working directory\n\n`" .. cwd .. "`"
  end

  if #parts == 0 then return "" end
  return "\n\n---\n\n# Runtime Context\n\n" .. table.concat(parts, "\n\n")
end

local agentic_loop = require("libs.agentic-loop")
agentic_loop.configure {
  provider         = cfg.default_provider,
  model            = cfg.default_model,
  reasoning_effort = cfg.lead_reasoning_effort,
  system           = lead_role.LEAD_SYSTEM_PROMPT .. build_runtime_context(),
  -- The lead's turn-program (config-as-program): each user message spawns
  -- this constellation on the mag kernel. The lead's tool surface is
  -- authored INSIDE the program (:tools on the agent config); the system
  -- prompt / provider / model above overlay onto its llm actor per turn.
  lead_program = {
    source_dir = STARTER_ROOT,
    entry      = "agentic-loop/lead-turn.mag",
  },
}
actor.spawn(agentic_loop)

local provider = require("libs.compositors.provider")
for _, p in ipairs(cfg.providers or {}) do
  if p.kind == "mock" then
    -- mock-plugin speaks the same wire protocol as the openai-provider
    -- binary, so the same actor spec works — only the binary differs.
    actor.spawn(provider.spawn_spec(
      p.name,
      {
        require("config").bin("mock-plugin"),
        "--script", STARTER_ROOT .. "/" .. p.mock_script,
      },
      { agentic_loop = agentic_loop, conversations = conversation_reader }
    ))
  elseif p.kind == "openai" then
    local provider_command = {
      require("config").bin("openai-provider"),
      "--name",     p.name,
      "--base-url", p.base_url,
    }
    if p.model then
      table.insert(provider_command, "--model")
      table.insert(provider_command, p.model)
    end
    for _, a in ipairs(p.extra_args or {}) do
      table.insert(provider_command, a)
    end
    actor.spawn(provider.spawn_spec(
      p.name,
      provider_command,
      {
        static_token = p.static_token,
        agentic_loop = agentic_loop,
        conversations = conversation_reader,
        request_additions = p.request_additions,
        usage = p.usage,
      }
    ))
  elseif p.kind == "chatgpt" then
    local provider_command = {
      require("config").bin("chatgpt-provider"),
      "--name", p.name,
    }
    if p.base_url then
      table.insert(provider_command, "--base-url")
      table.insert(provider_command, p.base_url)
    end
    -- No `--model` flag: chatgpt-provider fetches its model list from
    -- the backend at runtime; the user picks via `/model` in chat.
    for _, a in ipairs(p.extra_args or {}) do
      table.insert(provider_command, a)
    end
    actor.spawn(provider.spawn_spec(
      p.name,
      provider_command,
      {
        translator_lib = "chatgpt-provider",
        agentic_loop = agentic_loop,
        conversations = conversation_reader,
        usage = p.usage,
      }
    ))
  else
    error("examples/nefor-agent/init.lua: unknown provider kind: " .. tostring(p.kind))
  end
end

-- mag: the MAG actor-kernel runtime — the only execution path; every
-- run (the lead's turn-programs and its dispatched sub-runs) executes
-- here. Speaks the canonical wire shape, so it spawns via
-- `identity_spec`. Binary is `mag-plugin` (the `mag` binary is
-- nefor-mag's compiler CLI); bus identity is `mag`.
-- The plugin ships and loads its own kernel (`plugins/mag/lua/mag-kernel`),
-- resolved off `--lua-root`'s parent (NEFOR_ROOT) — the config no longer
-- carries a kernel copy. Pass `--kernel <path>` only to override it.
-- `--tool-gate` threads the composition-owned gate identity (the same name
-- tools.gate_spec below spawns the gate under): the plugin rewrites the
-- kernel's tool-class capability invokes onto `<gate>.tool.invoke`, and the
-- composition layer — not the plugin — owns cross-plugin names.
-- `--lua-root` threads the bootstrap-resolved shared Lua tree (LUA_ROOT
-- above) into the plugin's embedded VM so the kernel resolves both its own
-- tree and the shared libs (`output-persistence`) regardless of where the
-- config dir lives — installed configs carry no `lua/` tree of their own.
actor.spawn(actor.identity_spec("mag", {
  require("config").bin("mag-plugin"),
  "--tool-gate", "tool-gate",
  "--lua-root", LUA_ROOT,
}))

local tools = require("libs.compositors.tools")
local model_context_policy = require("libs.model-context-policy")
local tool_gate_argv = { require("config").bin("tool-gate") }
for _, t in ipairs(cfg.tool_gate.auto_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--auto"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
for _, t in ipairs(cfg.tool_gate.prompt_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--prompt"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
tool_gate_argv[#tool_gate_argv + 1] = "--default"
tool_gate_argv[#tool_gate_argv + 1] = cfg.tool_gate.default_action

-- lead-workflow lives alongside agentic-loop, not inside it: separate
-- bus subscriptions, separate state. Owns plan/approval state and the
-- active graph run id; advertises mag / write-review / graph-status to
-- tool-gate. Registered BEFORE tool-gate's spawn so
-- its bus subscription is live when tool-gate.hello arrives —
-- otherwise the advertise is missed and the lead model gets "no such
-- tool" at runtime.
actor.spawn(require("libs.lead-workflow"))

-- read-only-tools advertises the composition-selected Lua read tools.
-- basic-tools is the canonical shipped search_text owner. Same ordering
-- reason as lead-workflow: register before
-- tool-gate spawn so the gate's first hello triggers our advertise.
actor.spawn(require("read-only-tools"))

-- Tool-validator owns the chat.tool.permission_request → chat.tool.
-- popup_request translation: classifies shell scripts through `da` and process argv structurally
-- (approve/deny/defer), routes only the deferred ones to a user popup.
-- Must be spawned BEFORE tool-gate so its subscription is live when
-- the first gated invocation lands. The chat surface listens to
-- popup_request, not permission_request — without the validator
-- running, gated invocations never reach the popup.
actor.spawn(require("tool-validator"))

actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))
actor.spawn(tools.git_worktree_actor_spec())
actor.spawn(tools.basic_actor_spec { max_read_bytes = model_context_policy.item_limit })
startup_args.apply_mode(startup, agentic_loop)

actor.spawn(require("libs.compositors.chat_bridge").spawn_spec({
  require("config").bin("nefor-tui"),
  "--script", STARTER_ROOT .. "/chat/init.lua",
  -- Thread the composition's resolved lua/ tree explicitly (same contract
  -- as mag's --lua-root). The chat script's env-based fallbacks can
  -- otherwise resolve a different tree than the one this init picked.
  "--lua-root", NEFOR_ROOT .. "/lua",
}))

if startup.prompt ~= nil then
  require("libs.startup-readiness").wait {
    required_plugins = {
      cfg.default_provider, "mag", "tool-gate", "git-worktree", "basic-tools", "chat-surface",
    },
    required_tools = {
      "read_file", "read_image", "write_file", "edit_file", "search_text", "process.exec", "shell.script",
      "git_worktree_create", "git_worktree_open", "list_dir", "python-read",
      "instructions", "discover_instruction_files", "graph-status", "await-run",
      "terminate-graph", "write-review", "mag", "mag-eval",
    },
    tool_sources = {
      ["basic-tools"] = { "read_file", "read_image", "write_file", "edit_file", "search_text", "process.exec", "shell.script" },
      ["git-worktree"] = { "git_worktree_create", "git_worktree_open" },
      ["read-only-tools"] = { "list_dir", "python-read", "instructions", "discover_instruction_files" },
      ["lead-workflow"] = { "graph-status", "await-run", "terminate-graph", "write-review", "mag", "mag-eval" },
    },
    timeout_ms = tonumber(os.getenv("NEFOR_STARTUP_TIMEOUT_MS")) or 10000,
    on_ready = function()
      nefor.engine.send(nefor.json.encode({
        type = "event",
        from = "startup",
        ts   = nefor.engine.now(),
        body = { kind = "chat.input.submit", text = startup.prompt },
      }))
    end,
    on_error = function(message)
      io.stderr:write("nefor: " .. message .. "\n")
      io.stderr:flush()
      nefor.engine.shutdown { code = 1, reason = "composition requested shutdown", grace_ms = 2000 }
    end,
  }
end
