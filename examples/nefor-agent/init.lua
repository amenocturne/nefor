local startup
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
  { name = "nefor-mag", dir = NEFOR_ROOT .. "/mag" },
})

-- External executable dependencies use nefor-pm's writable package lifecycle;
-- the selected immutable Nefor runtime tree above remains read-only.
local da_package = require("libs.tool-validator.da")
pm.install({
  da_package.release_package {
    version = "0.2.1",
    checksums = {
      ["aarch64-apple-darwin"] = "2ab4714780f7e6509e30137b7e1434c8d534d035ddfddbc5a68278a023b281cf",
      ["aarch64-unknown-linux-gnu"] = "14469f64a8aafbddd7f8d1c76fb85771123188657368e5f01f17031c147fa0ea",
      ["x86_64-unknown-linux-gnu"] = "f2ab7637c0129291419683d686eeb9bd9154d7d06f5bba65abd574cf1786cfbb",
    },
  },
}, { on_progress = pm.stderr_progress })
local SHELL_CLASSIFIER = pm.bin("da", "da")

local MAG_PACKAGE_ROOT = pm.root("nefor-mag")
local MAG_MODULE_ROOTS = {
  MAG_PACKAGE_ROOT .. "/lib",
  STARTER_ROOT .. "/mag/lib",
}

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
  local entry = current_log[#current_log]
  if entry and entry.origin == "engine" then
    local ok, decoded = pcall(nefor.json.decode, entry.payload)
    local body = ok and type(decoded) == "table" and decoded.body or nil
    if type(body) == "table" then
      if body.kind == "engine.plugin_process_terminated" then
        plugin_process_terminated(body)
      elseif body.kind == "engine.interrupt_requested" then
        interrupt_requested(body)
      end
    end
  end
  ncp.dispatch(current_log)
end

function invoke_from_plugin(source, payload)
  ncp.invoke_from_plugin(source, payload)
end

-- Starter lifecycle policy: interactive mode tears down immediately. The CLI
-- settles before teardown. MAG death is reconciled explicitly as unknown by
-- the surviving run/request owners; it is never fabricated as mag.run_result.
function plugin_process_terminated(fact)
  if fact.plugin == "mag" then
    require("libs.lead-workflow").reconcile_mag_authority_loss(
      "MAG process terminated; accepted run outcomes are unknown")
  end
  if startup and startup.frontend == "cli" then return end
  nefor.engine.shutdown {
    code = 0,
    reason = "plugin " .. tostring(fact.plugin) .. " terminated",
    grace_ms = 2000,
  }
end

local MAG_PROJECT_BUILD = { cache_dir = nefor.fs.data_root() .. "/mag/cache" }

function interrupt_requested(_fact)
  if startup and startup.frontend == "cli" then return end
  nefor.engine.shutdown {
    code = 0,
    reason = "external interrupt",
    grace_ms = 2000,
  }
end

actor.install()
require("libs.mag-workspace").configure {
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
local parsed, options = pcall(startup_args.parse, (nefor.runtime and nefor.runtime.argv) or {})
if not parsed then
  io.stderr:write("nefor: " .. tostring(options) .. "\n")
  io.stderr:flush()
  -- The broker's shutdown sink is installed only after init returns. No
  -- process has been spawned at this point, so usage exits directly.
  os.exit(2)
end
startup = options

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
-- Appended to the lead system prompt so the agent knows where it is operating.
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

local mag_context = require("libs.mag-context").new {
  guides = {
    {
      title = "MAG in Five Minutes",
      path = MAG_PACKAGE_ROOT .. "/book/01. core/00. MAG in Five Minutes.md",
    },
    {
      title = "Nefor MAG in Five Minutes",
      path = MAG_PACKAGE_ROOT .. "/book/02. nefor/00. Nefor MAG in Five Minutes.md",
    },
  },
  book_path = MAG_PACKAGE_ROOT .. "/book/README.md",
  module_roots = {
    { name = "nefor-mag", path = MAG_PACKAGE_ROOT .. "/lib" },
    { name = "config", path = STARTER_ROOT .. "/mag/lib" },
  },
  trailing_sections = { build_runtime_context() },
}

local agentic_loop = require("libs.agentic-loop")
agentic_loop.configure {
  provider         = cfg.default_provider,
  model            = cfg.default_model,
  reasoning_effort = cfg.lead_reasoning_effort,
  system           = lead_role.LEAD_SYSTEM_PROMPT,
  ambient_context  = mag_context,
  -- The lead's turn-program (config-as-program): each user message spawns
  -- this constellation on the mag kernel. The lead's tool surface is
  -- authored INSIDE the program (:tools on the agent config); the system
  -- prompt / provider / model above overlay onto its llm actor per turn.
  lead_program = {
    project_build = MAG_PROJECT_BUILD,
    source_dir = STARTER_ROOT,
    entry      = "agentic-loop/lead-turn.mag",
    module_roots = MAG_MODULE_ROOTS,
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
    -- No `--model` flag: chatgpt-provider fetches its model list from
    -- the backend at runtime; the user picks via `/model` in chat.
    local provider_command = require("config.provider_command").chatgpt(
      require("config").bin("chatgpt-provider"), p)
    actor.spawn(provider.spawn_spec(
      p.name,
      provider_command,
      {
        translator_lib = "chatgpt-provider",
        tool_gate = "tool-gate",
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
-- active graph run id; advertises mag / write-review / mag-status to
-- tool-gate. Registered BEFORE tool-gate's spawn so
-- its bus subscription is live when tool-gate.hello arrives —
-- otherwise the advertise is missed and the lead model gets "no such
-- tool" at runtime.
local lead_workflow = require("libs.lead-workflow")
lead_workflow.configure {
  project_build = MAG_PROJECT_BUILD,
  dependency_module_roots = MAG_MODULE_ROOTS,
  ambient_context = mag_context,
  agent_system = lead_role.WORKER_SYSTEM_PROMPT,
  resolve_model_snapshot = agentic_loop.model_snapshot,
}
actor.spawn(lead_workflow)

-- read-only-tools advertises the composition-selected Lua read tools.
-- Same ordering as lead-workflow: register before
-- tool-gate spawn so the gate's first hello triggers our advertise.
actor.spawn(require("read-only-tools"))

-- Tool-validator owns the chat.tool.permission_request → chat.tool.
-- popup_request translation: classifies shell scripts through `da` and process argv structurally
-- (approve/deny/defer), routes only the deferred ones to a user popup.
-- Must be spawned BEFORE tool-gate so its subscription is live when
-- the first gated invocation lands. The chat surface listens to
-- popup_request, not permission_request — without the validator
-- running, gated invocations never reach the popup.
actor.spawn(require("tool-validator").build {
  shell_classifier = SHELL_CLASSIFIER,
})

actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))
actor.spawn(tools.git_worktree_actor_spec())
actor.spawn(tools.basic_actor_spec { max_read_bytes = model_context_policy.item_limit })
if startup.frontend == "tui" then
actor.spawn(require("libs.compositors.chat_bridge").spawn_spec({
  require("config").bin("nefor-tui"),
  "--script", STARTER_ROOT .. "/chat/init.lua",
  -- Thread the composition's resolved lua/ tree explicitly (same contract
  -- as mag's --lua-root). The chat script's env-based fallbacks can
  -- otherwise resolve a different tree than the one this init picked.
  "--lua-root", NEFOR_ROOT .. "/lua",
}))

end

local composition_readiness = {
    required_plugins = {
      "mag", "tool-gate", "git-worktree", "basic-tools",
    },
    required_provider = function() return agentic_loop.model_snapshot().provider end,
    required_tools = {
      "read_file", "read_image", "write_file", "process.exec", "shell.script",
      "git_worktree_create", "git_worktree_open",
      "discover_instruction_files", "mag-status", "mag-await",
      "mag-terminate", "write-review", "mag-write-file", "mag-preview", "mag-apply",
    },
    tool_sources = {
      ["basic-tools"] = { "read_file", "read_image", "write_file", "process.exec", "shell.script" },
      ["git-worktree"] = { "git_worktree_create", "git_worktree_open" },
      ["read-only-tools"] = { "discover_instruction_files" },
      ["lead-workflow"] = { "mag-status", "mag-await", "mag-terminate", "write-review", "mag-write-file", "mag-preview", "mag-apply" },
    },
    timeout_ms = tonumber(os.getenv("NEFOR_STARTUP_TIMEOUT_MS")) or 10000,
}
if startup.frontend == "cli" then
  require("libs.cli").start {
    prompt = startup.prompt, format = startup.format, readiness = composition_readiness,
  }
elseif startup.prompt ~= nil then
  composition_readiness.required_plugins[#composition_readiness.required_plugins + 1] = "chat-surface"
  composition_readiness.is_ready = function() return sessions.ready() and agentic_loop.is_ready() end
  composition_readiness.on_ready = function()
    require("core.envelope").emit_as("startup", nil, {
      kind = "chat.input.submit", text = startup.prompt,
      submission_id = "request-" .. require("core.envelope").uuid_lite(),
    })
  end
  composition_readiness.on_error = function(message)
    io.stderr:write("nefor: " .. message .. "\n")
    io.stderr:flush()
    nefor.engine.shutdown { code = 1, reason = "startup failed", grace_ms = 2000 }
  end
  require("libs.startup-readiness").wait(composition_readiness)
end

-- Register all replay consumers and frontend observers before session activation:
-- resume may synchronously emit its first chunk during init.
sessions.init(startup.session_id)
startup_args.apply_mode(startup, agentic_loop)
