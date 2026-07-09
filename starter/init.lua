-- starter/init.lua — nefor-team composition (consumer config).
--
-- The team owns init.lua, config, lead-workflow/role, auth, prompts/,
-- and a small qwen_hooks module that splices the qwen `<think>` filter
-- + Nestor's models-list-drop into upstream's provider compositor via
-- its hooks API. Every other module (agentic-loop, reasoners, sessions,
-- lead-workflow, chat, the compositors themselves) is shipped by
-- upstream and reached through nefor-pm and the package.path overlay.
--
-- ## Known limitation — JWT on the cmdline
--
-- In the Nestor branch below, the JWT is passed to openai-provider as
-- `--api-key <jwt>`. That makes it visible to other users on the host
-- via `ps -ef` / `/proc/<pid>/cmdline`. The proper fix requires
-- per-instance env-var support in upstream's plugin spawn API (a Rust
-- change). Until that lands, do NOT run this on a shared host.
--
-- NEFOR_CONFIG selects the variant:
--   * prod (default) — DP -> JWT exchange + Nestor model list fetch at
--     boot; qwen think-tag filter + cached-model-list intercept active.
--   * test (alias: dev) — openai-provider against local ollama. No auth.
--
-- NEFOR_DEV_DIR controls fetch path: when set, pm specs point at a
-- local upstream checkout via `dir =` overrides; when unset, pm clones
-- the pinned tag into $NEFOR_DATA_DIR.

-- The pm itself ships inside the upstream amenocturne/nefor repo at
-- lua/nefor-pm/. On first boot of a fresh machine, sparse-clone the
-- repo at UPSTREAM_REF under $NEFOR_DATA_DIR/nefor/ with `lua starter
-- plugins` in the cone; on subsequent boots, re-apply the cone
-- (idempotent) in case a previous run used a narrower one.
--
-- UPSTREAM_REF derived from the engine's version. Exact release tags
-- (e.g. 0.1.9 → v0.1.9) pin Lua libs to the matching binary version;
-- nightly/dev builds fall back to "main". Must be a ref that exists on
-- origin and contains the modern lua/ reorg (v0.1.3 and earlier will
-- clone successfully but leave `lua/nefor-pm/` missing).
local UPSTREAM_REF
do
  local v = nefor and nefor.version
  if type(v) == "string" and v:match("^%d+%.%d+%.%d+$") then
    UPSTREAM_REF = "v" .. v
  else
    UPSTREAM_REF = "main"
  end
end
local STARTER_ROOT  = NEFOR_CONFIG_DIR or "."
local SPARSE_CONE   = "lua starter plugins"

local function read_env_value(path, key)
  local f = io.open(path, "r")
  if not f then return nil end
  for line in f:lines() do
    local k, v = line:match("^%s*([A-Za-z_][A-Za-z0-9_]*)%s*=%s*(.-)%s*$")
    if k == key then
      f:close()
      return v
    end
  end
  f:close()
  return nil
end

local PINNED_ENV_PATH = STARTER_ROOT .. "/.env"
local PINNED_LOCAL_ENV_PATH = STARTER_ROOT .. "/.env.local"
local PINNED_NEFOR_VERSION = read_env_value(PINNED_ENV_PATH, "NEFOR_VERSION")
if type(PINNED_NEFOR_VERSION) ~= "string" or PINNED_NEFOR_VERSION == "" then
  PINNED_ENV_PATH = STARTER_ROOT .. "/../.env"
  PINNED_LOCAL_ENV_PATH = STARTER_ROOT .. "/../.env.local"
  PINNED_NEFOR_VERSION = read_env_value(PINNED_ENV_PATH, "NEFOR_VERSION")
end
if type(PINNED_NEFOR_VERSION) ~= "string" or PINNED_NEFOR_VERSION == "" then
  error("nefor-team startup: missing NEFOR_VERSION in " .. STARTER_ROOT .. "/.env or " .. STARTER_ROOT .. "/../.env; run `just sync` from the nefor-team repo")
end

local NEFOR_DEV_DIR = os.getenv("NEFOR_DEV_DIR")
if type(NEFOR_DEV_DIR) ~= "string" or NEFOR_DEV_DIR == "" then
  NEFOR_DEV_DIR = read_env_value(PINNED_LOCAL_ENV_PATH, "NEFOR_DEV_DIR")
end
if type(NEFOR_DEV_DIR) ~= "string" or NEFOR_DEV_DIR == "" then
  NEFOR_DEV_DIR = nil
end

if not NEFOR_DEV_DIR and (not nefor or nefor.version ~= PINNED_NEFOR_VERSION) then
  error("nefor-team startup: nefor version " .. tostring(nefor and nefor.version)
        .. " does not match pinned NEFOR_VERSION=" .. PINNED_NEFOR_VERSION
        .. "; run `just sync` from the nefor-team repo")
end

-- Lua 5.2+ returns (true|nil, "exit"|"signal", code); 5.1 returns the
-- raw exit code. Normalise to a single bool so callers don't have to
-- re-invoke os.execute to inspect the result.
local function run(cmd)
  local ok = os.execute(cmd)
  return ok == true or ok == 0
end

local function capture(cmd)
  local f = io.popen(cmd)
  if not f then return nil end
  local out = f:read("*l")
  f:close()
  return out
end

local STARTER_UPSTREAM
local LUA_ROOT

if NEFOR_DEV_DIR and #NEFOR_DEV_DIR > 0 then
  local pm_dir = NEFOR_DEV_DIR .. "/lua"
  package.path = table.concat({
    pm_dir .. "/?.lua",
    pm_dir .. "/?/init.lua",
    package.path,
  }, ";")
  STARTER_UPSTREAM = NEFOR_DEV_DIR .. "/starter"
  LUA_ROOT = NEFOR_DEV_DIR .. "/lua"
else
  local data_dir = nefor.fs.data_root()
  local pm_root  = data_dir .. "/nefor"

  if not nefor.fs.exists(pm_root) then
    nefor.fs.mkdir_p(data_dir)
    local clone_cmd = "git clone --depth 1 --filter=blob:none --sparse "
                   .. "--branch '" .. UPSTREAM_REF .. "' "
                   .. "https://github.com/amenocturne/nefor.git '" .. pm_root .. "'"
    if not run(clone_cmd) then
      error("nefor-team bootstrap: git clone failed for ref " .. UPSTREAM_REF
            .. "; check network + ref existence on origin")
    end
  else
    local local_hash = capture("git -C '" .. pm_root .. "' rev-parse HEAD 2>/dev/null") or ""
    -- Ask for both the plain ref and its peeled form (^{}): annotated tags
    -- resolve to a tag object under the plain ref, and comparing that
    -- against a local commit hash would report a permanent mismatch. The
    -- peeled line sorts directly after the tag line, so the last line is
    -- always the commit hash for both tags and branches.
    local remote_hash = capture("git -C '" .. pm_root .. "' ls-remote origin '"
                                .. UPSTREAM_REF .. "' '" .. UPSTREAM_REF .. "^{}' 2>/dev/null | tail -1") or ""
    remote_hash = remote_hash:match("^(%x+)") or ""
    if remote_hash == "" then
      print("[nefor-team] warning: cannot reach origin to refresh " .. pm_root
            .. " (ref " .. UPSTREAM_REF .. "); continuing with the existing checkout")
    elseif local_hash ~= remote_hash then
      -- fetch + reset to FETCH_HEAD works uniformly for branches and tags;
      -- `checkout <tag>` / `reset origin/<tag>` do not (a shallow tag fetch
      -- creates no local ref). Failures are fatal: silently keeping a stale
      -- checkout means running new binaries against old Lua libs.
      if not run("git -C '" .. pm_root .. "' fetch --depth 1 origin '" .. UPSTREAM_REF .. "'") then
        error("nefor-team bootstrap: git fetch failed for ref " .. UPSTREAM_REF .. " in " .. pm_root)
      end
      if not run("git -C '" .. pm_root .. "' reset --hard FETCH_HEAD") then
        error("nefor-team bootstrap: git reset --hard FETCH_HEAD failed in " .. pm_root)
      end
    end
  end
  if not run("git -C '" .. pm_root .. "' sparse-checkout set " .. SPARSE_CONE) then
    error("nefor-team bootstrap: git sparse-checkout set failed for " .. pm_root)
  end

  package.path = table.concat({
    pm_root .. "/lua/?.lua",
    pm_root .. "/lua/?/init.lua",
    package.path,
  }, ";")
  STARTER_UPSTREAM = pm_root .. "/starter"
  LUA_ROOT = pm_root .. "/lua"
end

local pm = require("nefor-pm")

-- The team's composition reuses upstream's starter modules verbatim
-- (agentic-loop, reasoners, sessions, lead-workflow, chat, the
-- unchanged compositors). pm.install grafts the PARENT of each spec's
-- dir onto package.path so `require("<name>")` resolves under it — for
-- the upstream starter we want the directory ITSELF on the path so
-- `require("agentic-loop")` finds `<starter>/agentic-loop/init.lua`.
-- Manual graft below handles that.

local function dev(relpath)
  if NEFOR_DEV_DIR and #NEFOR_DEV_DIR > 0 then
    return NEFOR_DEV_DIR .. "/" .. relpath
  end
  return nil
end

pm.install({
  { "amenocturne/nefor",
    name = "core",
    tag  = UPSTREAM_REF,
    path = "lua/core/",
    dir  = dev("lua/core") or (LUA_ROOT .. "/core"),
  },

  { "amenocturne/nefor",
    name = "libs",
    tag  = UPSTREAM_REF,
    path = "lua/libs/",
    dir  = dev("lua/libs") or (LUA_ROOT .. "/libs"),
  },

  { "amenocturne/nefor",
    name = "openai-provider",
    tag  = UPSTREAM_REF,
    path = "plugins/openai-provider/lua/openai-provider/",
    dir  = dev("plugins/openai-provider/lua/openai-provider") or (STARTER_UPSTREAM .. "/../plugins/openai-provider/lua/openai-provider"),
  },

  { "amenocturne/nefor",
    name = "chatgpt-provider",
    tag  = UPSTREAM_REF,
    path = "plugins/chatgpt-provider/lua/chatgpt-provider/",
    dir  = dev("plugins/chatgpt-provider/lua/chatgpt-provider") or (STARTER_UPSTREAM .. "/../plugins/chatgpt-provider/lua/chatgpt-provider"),
  },

  { "amenocturne/nefor",
    name = "tool-gate",
    tag  = UPSTREAM_REF,
    path = "plugins/tool-gate/lua/tool-gate/",
    dir  = dev("plugins/tool-gate/lua/tool-gate") or (STARTER_UPSTREAM .. "/../plugins/tool-gate/lua/tool-gate"),
  },

  { "amenocturne/nefor",
    name = "nefor-tui",
    tag  = UPSTREAM_REF,
    path = "plugins/nefor-tui/lua/",
    dir  = dev("plugins/nefor-tui/lua") or (STARTER_UPSTREAM .. "/../plugins/nefor-tui/lua"),
  },

})

-- Graft path entries in REVERSE precedence order — later additions win
-- because each table.concat prepends. STARTER_UPSTREAM goes in first
-- (lowest precedence: upstream's starter modules), STARTER_ROOT last
-- (highest precedence: team's overrides that share a module name with
-- an upstream-overlay file).
package.path = table.concat({
  STARTER_UPSTREAM .. "/?.lua",
  STARTER_UPSTREAM .. "/?/init.lua",
  package.path,
}, ";")

package.path = table.concat({
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  package.path,
}, ";")

local ncp       = require("core.ncp")
local actor     = require("core.actor")
local sessions  = require("libs.sessions")
local lead_role = require("lead-workflow.role")
local config    = require("config")
local cfg       = config.active

local PROVIDER_NAME = cfg.default_provider
local provider_cfg = cfg.providers and cfg.providers[PROVIDER_NAME]
if type(provider_cfg) ~= "table" then
  error("config variant " .. tostring(config.variant) ..
        " must define providers[default_provider]")
end
provider_cfg.name = provider_cfg.name or PROVIDER_NAME

-- Lazy-require auth so a developer without DP on their machine can
-- still run the test/dev variants.
local nestor_auth
if provider_cfg.kind == "nestor" then
  nestor_auth = require("auth")
end

function dispatch(current_log)
  ncp.dispatch(current_log)
end

function invoke_from_plugin(source, payload)
  ncp.invoke_from_plugin(source, payload)
end

local history_replay = require("core.history_replay")

actor.install()
history_replay.install()
actor.spawn(sessions)
actor.spawn(require("libs.state-tracking"))
sessions.init()

-- Auth runs inline at startup so a missing DP session surfaces before
-- the user types their first message. The JWT is passed to
-- openai-provider as --api-key; the engine spawn API has no
-- per-instance env vars, so the only injection channel is the command
-- line.

local function log_banner(line)
  io.stderr:write("[nefor-team] " .. line .. "\n")
  io.stderr:flush()
end

local jwt, CHOSEN_MODEL
local NESTOR_MODEL_NAMES = {}

if provider_cfg.kind ~= "nestor" then
  CHOSEN_MODEL = cfg.default_model
  log_banner(string.format("variant %q (provider=%s kind=%s) active; skipping Nestor auth",
                           config.variant, PROVIDER_NAME, provider_cfg.kind))
else
  log_banner("authenticating against Nestor (" .. nestor_auth.NESTOR_BASE .. ")...")

  jwt = nestor_auth.get_jwt()
  log_banner("Nestor JWT acquired.")

  local models = nestor_auth.list_models(jwt)
  log_banner(string.format("Nestor models available: %d", #models))
  for _, m in ipairs(models) do
    if type(m) == "table" and type(m.name) == "string" then
      NESTOR_MODEL_NAMES[#NESTOR_MODEL_NAMES + 1] = m.name
      local default_marker = m.is_default and " (default)" or ""
      log_banner("  - " .. m.name .. (m.desc and (" — " .. m.desc) or "") .. default_marker)
    end
  end

  -- Model preference order:
  --   1. NEFOR_TEAM_MODEL env var (explicit override).
  --   2. A concrete cfg.default_model pin — anything other than the
  --      "default" sentinel forces that exact model (prod pins qwen35).
  --   3. First entry the API marks `is_default = true`.
  --   4. First entry in the list.
  --   5. The "default" sentinel as a last resort.
  local function pick_model()
    local env = os.getenv("NEFOR_TEAM_MODEL")
    if type(env) == "string" and #env > 0 then return env end
    local pinned = cfg.default_model
    if type(pinned) == "string" and #pinned > 0 and pinned ~= "default" then
      return pinned
    end
    for _, m in ipairs(models) do
      if type(m) == "table" and m.is_default == true and type(m.name) == "string" then
        return m.name
      end
    end
    if type(models[1]) == "table" and type(models[1].name) == "string" then
      return models[1].name
    end
    return cfg.default_model or "default"
  end

  CHOSEN_MODEL = pick_model()
  log_banner("using model: " .. CHOSEN_MODEL)
end

-- Order matters: type-tag registrations must complete before MAG submits.
-- Sequence: contracts → agentic-loop → provider → mag → tool-gate/tools →
-- lead-workflow/read-only-tools → TUI.

require("libs.generic-provider").declare()
require("libs.generic-tool").declare()

local agentic_loop = require("libs.agentic-loop")
agentic_loop.configure {
  provider = PROVIDER_NAME,
  model    = CHOSEN_MODEL,
  reasoning_effort = cfg.lead_reasoning_effort,
  system   = lead_role.LEAD_SYSTEM_PROMPT,
  lead_program = {
    source_dir = STARTER_ROOT,
    entry      = "agentic-loop/lead-turn.mag",
  },
}
actor.spawn(agentic_loop)

local bin = config.bin

if type(cfg.spawn_provider) == "function" then
  local ok, err = pcall(cfg.spawn_provider, {
    actor          = actor,
    bin            = bin,
    provider_name  = PROVIDER_NAME,
    chosen_model   = CHOSEN_MODEL,
    agentic_loop   = agentic_loop,
    config         = config,
    cfg            = cfg,
    provider_cfg   = provider_cfg,
    starter_root   = STARTER_ROOT,
    starter_upstream = STARTER_UPSTREAM,
    log_banner     = log_banner,
  })
  if not ok then
    error("local provider spawn failed for variant " .. tostring(config.variant) ..
          ": " .. tostring(err))
  end

elseif provider_cfg.kind == "ollama" then
  -- static_token is decorative (ollama ignores it, but openai-provider
  -- requires --api-key). No think-tag wiring needed.
  actor.spawn(require("libs.compositors.provider").spawn_spec(
    PROVIDER_NAME,
    {
      bin("openai-provider"),
      "--name",     PROVIDER_NAME,
      "--api-key",  "ollama-local",
      "--base-url", provider_cfg.base_url,
      "--model",    CHOSEN_MODEL,
    },
    { static_token = "ollama-local", agentic_loop = agentic_loop }
  ))

elseif provider_cfg.kind == "nestor" then
  -- Upstream's provider compositor + team-owned qwen_hooks that splice
  -- the `<think>` filter, the chat.complete.result inline-strip, and
  -- the chat.model.list_requested drop into the lib's translation
  -- pipeline via opts.hooks.
  local nestor_opts = {
    agentic_loop                 = agentic_loop,
    static_token                 = jwt,
    enable_think_tag_filter      = true,
    intercept_model_list_request = true,
  }
  nestor_opts.hooks = require("compositors.qwen_hooks").make(PROVIDER_NAME, nestor_opts)
  actor.spawn(require("libs.compositors.provider").spawn_spec(
    PROVIDER_NAME,
    {
      bin("openai-provider"),
      "--name",        PROVIDER_NAME,
      "--base-url",    nestor_auth.OPENAI_PROVIDER_BASE_URL,
      "--model",       CHOSEN_MODEL,
      "--api-key",     jwt,
      -- Nestor gates on `Nestor-Token: <jwt>` rather than the standard
      -- `Authorization: Bearer ...`.
      "--auth-header", "Nestor-Token",
    },
    nestor_opts
  ))

  -- Nestor's API has no /v1/models, so the openai-provider binary's
  -- normal path 404s and the picker stays blank. Serve the cached list
  -- from boot.
  if nefor.bus and nefor.bus.on_event then
    local json = nefor.json
    nefor.bus.on_event("chat.model.list_requested", function(entry)
      if type(entry) ~= "table" or type(entry.payload) ~= "string" then return end
      local ok, decoded = pcall(json.decode, entry.payload)
      if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
      if decoded.body.provider ~= PROVIDER_NAME then return end
      nefor.engine.send(json.encode({
        type = "event",
        from = PROVIDER_NAME,
        ts   = nefor.engine.now(),
        body = {
          kind     = "chat.models.listed",
          provider = PROVIDER_NAME,
          models   = NESTOR_MODEL_NAMES,
        },
      }))
    end)
  end

else
  error("unknown provider kind: " .. tostring(provider_cfg.kind) ..
        " (expected one of: ollama, nestor, or local spawn_provider)")
end

actor.spawn(actor.identity_spec("mag", {
  bin("mag-plugin"),
  "--tool-gate", "tool-gate",
  "--lua-root", LUA_ROOT,
}))

-- Build the tool-gate argv. 0.4 MAG dispatch tools are auto-approved at the
-- outer call; write-capable execution is checked inside lead-workflow before
-- `mag.execute`, and concrete file/bash capabilities still pass through the
-- gate/validator. Unknown future tools default to prompt.
local tool_gate_argv = { bin("tool-gate") }
for _, t in ipairs(cfg.tool_gate.prompt_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--prompt"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
for _, t in ipairs(cfg.tool_gate.auto_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--auto"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
for _, t in ipairs(cfg.tool_gate.deny_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--deny"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
tool_gate_argv[#tool_gate_argv + 1] = "--default"
tool_gate_argv[#tool_gate_argv + 1] = cfg.tool_gate.default_action or "prompt"

-- Register lead-workflow BEFORE spawning tool-gate so the lead's
-- bus subscription is live when tool-gate.hello arrives — otherwise
-- the advertise of mag/write-review/graph-status is missed and the lead model
-- gets "no such tool" at runtime.
actor.spawn(require("lead-workflow"))

-- read-only-tools advertises list_dir + search_text + the `skill` tool
-- (Lua-resident, pure-read). Same ordering reason as lead-workflow: register
-- before tool-gate spawn so the gate's first hello triggers our advertise.
-- Jira/Confluence are no longer Lua tools — the lead loads the `dp` and
-- `confluence` skills and runs those CLIs through mag-eval/bash.
actor.spawn(require("read-only-tools"))

-- Tool-validator translates tool-gate's chat.tool.permission_request
-- into either an auto tool.permission_response (da-classified bash
-- safe-ops) or a chat.tool.popup_request (defer to user). The chat
-- surface listens only to popup_request, so without this actor every
-- gated tool call would silently stall. See upstream's
-- starter/tool-validator/init.lua for policy details.
actor.spawn(require("tool-validator"))

local tools = require("libs.compositors.tools")
actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))

actor.spawn(tools.basic_actor_spec())

actor.spawn(require("libs.compositors.chat_bridge").spawn_spec({
  bin("nefor-tui"),
  "--script", STARTER_UPSTREAM .. "/chat/init.lua",
  -- Thread the resolved lua/ tree explicitly (same contract as mag's
  -- --lua-root above). NEFOR_DEV_DIR read from .env.local is a Lua value,
  -- not process env — without this argv the TUI's chat script cannot see
  -- dev mode and silently falls back to the pm overlay tree.
  "--lua-root", LUA_ROOT,
}))
