-- init.lua — starter composition.
--
-- Lua libraries are bootstrapped via sparse-clone of the upstream repo
-- into ~/.local/share/nefor/nefor/ (same as end-user install).
--
-- Bootstrap order:
--   1. NEFOR_DEV_DIR set     → in-checkout dev mode; resolve from there.
--   2. NEFOR_LOCAL_DIR set   → local checkout override for installed configs.
--   3. agentic-kit.json nefor_repo → per-install local checkout override.
--   4. sibling checkout      → auto-detected from agentic-kit.json dev workspace.
--   5. STARTER_ROOT/../lua/nefor-pm exists → in-checkout (running via
--                                            `just run`); use ../lua etc.
--   6. otherwise             → sparse-clone amenocturne/nefor at the
--                              pinned ref into <DATA>/nefor/, use that.

local STARTER_ROOT = NEFOR_CONFIG_DIR or "."
local NEFOR_DEV_DIR = os.getenv("NEFOR_DEV_DIR")
local NEFOR_LOCAL_DIR = os.getenv("NEFOR_LOCAL_DIR")

local UPSTREAM_REF
do
  local v = nefor and nefor.version
  if type(v) == "string" and v:match("^%d+%.%d+%.%d+$") then
    UPSTREAM_REF = "v" .. v
  else
    UPSTREAM_REF = "main"
  end
end
local SPARSE_CONE  = "lua starter plugins"

local function path_exists(p)
  if nefor and nefor.fs and nefor.fs.exists then
    return nefor.fs.exists(p)
  end
  local f = io.open(p, "r")
  if f then f:close(); return true end
  return false
end

local function run(cmd)
  local ok = os.execute(cmd)
  return ok == true or ok == 0
end

local function sh_quote(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

local function valid_nefor_root(root)
  return root and #root > 0 and path_exists(root .. "/lua/nefor-pm/init.lua")
end

local function explicit_nefor_root(env_name, root)
  if not root or #root == 0 then return nil end
  if valid_nefor_root(root) then return root end
  error("nefor bootstrap: " .. env_name .. "=" .. root
        .. " does not contain lua/nefor-pm/init.lua")
end

local function read_agentic_kit_path(key)
  local fh = io.open(STARTER_ROOT .. "/agentic-kit.json", "r")
  if not fh then return nil end
  local raw = fh:read("*a")
  fh:close()
  local ok, decoded = pcall(nefor.json.decode, raw)
  if ok and type(decoded) == "table" and type(decoded[key]) == "string" then
    return decoded[key]
  end
  return raw:match('"' .. key .. '"%s*:%s*"([^"]+)"')
end

local function read_agentic_kit_nefor_repo()
  return read_agentic_kit_path("nefor_repo")
end

local function read_agentic_kit_dev_workspace()
  return read_agentic_kit_path("dev_workspace")
end

local function detected_local_nefor_root()
  local dev_workspace = read_agentic_kit_dev_workspace()
  if dev_workspace then
    local candidate = dev_workspace .. "/personal/nefor"
    if valid_nefor_root(candidate) then return candidate end
  end
  local sibling = STARTER_ROOT .. "/../../../../nefor"
  if valid_nefor_root(sibling) then return sibling end
  return nil
end

local function ensure_upstream_checkout(pm_root)
  local root = sh_quote(pm_root)
  local ref = sh_quote(UPSTREAM_REF)
  local fetch_ref = ref
  if UPSTREAM_REF:match("^v%d+%.%d+%.%d+$") then
    fetch_ref = "tag " .. ref
  end

  if not path_exists(pm_root) then
    nefor.fs.mkdir_p(nefor.fs.data_root())
    local clone_cmd = "git clone --depth 1 --filter=blob:none --sparse "
                   .. "--branch " .. ref .. " "
                   .. "https://github.com/amenocturne/nefor.git " .. root
    if not run(clone_cmd) then
      error("nefor bootstrap: git clone failed for ref " .. UPSTREAM_REF
            .. ". Check git is on PATH, the network is reachable, and the "
            .. "ref exists on origin.")
    end
  elseif not path_exists(pm_root .. "/.git") then
    error("nefor bootstrap: " .. pm_root .. " exists but is not a git checkout")
  else
    -- A release checkout already at its exact local tag is authoritative and
    -- needs no network. The shell comparison deliberately resolves annotated
    -- tags to commits before comparing them with HEAD.
    local exact_local_pin = UPSTREAM_REF:match("^v%d+%.%d+%.%d+$")
      and run("test \"$(git -C " .. root .. " rev-parse HEAD)\" = "
           .. "\"$(git -C " .. root .. " rev-parse " .. ref .. "^{commit})\"")
    if not exact_local_pin then
      if not run("git -C " .. root .. " fetch --depth 1 origin " .. fetch_ref) then
        error("nefor bootstrap: git fetch failed for ref " .. UPSTREAM_REF
              .. "; no valid exact local pin is available")
      end
      if not run("git -C " .. root .. " checkout --force FETCH_HEAD") then
        error("nefor bootstrap: git checkout failed for ref " .. UPSTREAM_REF)
      end
    end
  end

  if not run("git -C " .. root .. " sparse-checkout set " .. SPARSE_CONE) then
    error("nefor bootstrap: git sparse-checkout failed for " .. pm_root)
  end
end

local NEFOR_ROOT
local explicit_root = explicit_nefor_root("NEFOR_DEV_DIR", NEFOR_DEV_DIR)
                   or explicit_nefor_root("NEFOR_LOCAL_DIR", NEFOR_LOCAL_DIR)
                   or explicit_nefor_root(
                     "agentic-kit.json nefor_repo",
                     read_agentic_kit_nefor_repo())
local detected_root = detected_local_nefor_root()
if explicit_root then
  NEFOR_ROOT = explicit_root
elseif detected_root then
  NEFOR_ROOT = detected_root
elseif path_exists(STARTER_ROOT .. "/../lua/nefor-pm/init.lua") then
  NEFOR_ROOT = STARTER_ROOT .. "/.."
else
  local data_dir = nefor.fs.data_root()
  local pm_root  = data_dir .. "/nefor"
  ensure_upstream_checkout(pm_root)
  NEFOR_ROOT = pm_root
end

local LUA_ROOT = NEFOR_ROOT .. "/lua"

package.path = table.concat({
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  LUA_ROOT .. "/?.lua",
  LUA_ROOT .. "/?/init.lua",
  package.path,
}, ";")

-- nefor-pm wires the core primitives, generic libs, and every plugin
-- lib. Every entry's `dir` resolves from NEFOR_ROOT (whichever way
-- the bootstrap above picked it). `tag` matches UPSTREAM_REF so a
-- future pm consistency check or refresh path uses one source of truth.
local pm = require("nefor-pm")
pm.install({
  {
    "amenocturne/nefor",
    name = "core",
    tag  = UPSTREAM_REF,
    path = "lua/core/",
    dir  = NEFOR_ROOT .. "/lua/core",
  },

  {
    "amenocturne/nefor",
    name = "libs",
    tag  = UPSTREAM_REF,
    path = "lua/libs/",
    dir  = NEFOR_ROOT .. "/lua/libs",
  },

  {
    "amenocturne/nefor",
    name = "openai-provider",
    tag  = UPSTREAM_REF,
    path = "plugins/openai-provider/lua/openai-provider/",
    dir  = NEFOR_ROOT .. "/plugins/openai-provider/lua/openai-provider",
  },

  {
    "amenocturne/nefor",
    name = "chatgpt-provider",
    tag  = UPSTREAM_REF,
    path = "plugins/chatgpt-provider/lua/chatgpt-provider/",
    dir  = NEFOR_ROOT .. "/plugins/chatgpt-provider/lua/chatgpt-provider",
  },

  {
    "amenocturne/nefor",
    name = "tool-gate",
    tag  = UPSTREAM_REF,
    path = "plugins/tool-gate/lua/tool-gate/",
    dir  = NEFOR_ROOT .. "/plugins/tool-gate/lua/tool-gate",
  },

  {
    "amenocturne/nefor",
    name = "nefor-tui",
    tag  = UPSTREAM_REF,
    path = "plugins/nefor-tui/lua/",
    dir  = NEFOR_ROOT .. "/plugins/nefor-tui/lua",
  },
})

local ncp            = require("core.ncp")
local actor          = require("core.actor")
local replay_window = require("core.replay_window")
local sessions       = require("libs.sessions")
local cfg            = require("config").active
local lead_role      = require("libs.lead-workflow.role")

function dispatch(current_log)
  ncp.dispatch(current_log)
end

function invoke_from_plugin(source, payload)
  ncp.invoke_from_plugin(source, payload)
end

actor.install()
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
actor.spawn(require("libs.state-tracking"))

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
      }
    ))
  else
    error("starter/init.lua: unknown provider kind: " .. tostring(p.kind))
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
-- popup_request translation: classifies bash commands through `da`
-- (approve/deny/defer), routes only the deferred ones to a user popup.
-- Must be spawned BEFORE tool-gate so its subscription is live when
-- the first gated invocation lands. The chat surface listens to
-- popup_request, not permission_request — without the validator
-- running, gated invocations never reach the popup.
actor.spawn(require("tool-validator"))

actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))
actor.spawn(tools.git_worktree_actor_spec())
actor.spawn(tools.basic_actor_spec())
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
      cfg.default_provider, "mag", "tool-gate", "git-worktree", "basic-tools",
    },
    required_tools = {
      "read_file", "read_image", "write_file", "edit_file", "bash", "search_text",
      "git_worktree_create", "git_worktree_open", "list_dir", "python-read",
      "instructions", "discover_instruction_files", "graph-status", "await-run",
      "terminate-graph", "write-review", "mag", "mag-eval",
    },
    tool_sources = {
      ["basic-tools"] = { "read_file", "read_image", "write_file", "edit_file", "bash", "search_text" },
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
      nefor.engine.exit(1)
    end,
  }
end
