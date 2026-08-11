-- examples/nefor-agent/chat.lua — chat surface as a Lua composition over tui.* primitives.
--
-- The engine ships zero opinion. Every color, every layout, every
-- glyph below is editable — this composition IS the chat surface's
-- identity. The submodules under `chat/` carry the per-concern code;
-- this entry file installs the require searcher, glues the submodules
-- together, declares initial state, and hands view/update to
-- `tui.start`.
--
-- Inbound chat-contract events handled here:
--   conversation.active.changed, conversation.projection.delta,
--   conversation.snapshot, chat.instruction.notice,
--   chat.popup, chat.auth.status, chat.model.set_ack, chat.models.listed,
--   chat.tool.popup_request, tool-gate.mode_changed,
--   mag.run_started and the rest of the mag.* kernel lifecycle stream.
--
-- Outbound:
--   chat.input.submit, chat.interrupt, chat.interrupt_all, chat.reset,
--   chat.command, tool.permission_response.

-- The nefor-tui binary loads this file via `--script chat.lua`. Its
-- embedded Lua VM starts with a vanilla package.path; install two
-- searchers up front:
--   * nefor-tui[.<sub>] → <plugin lib>/lua/<...>.lua
--   * chat.<sub>        → <starter dir>/chat/<...>.lua
-- A custom searcher avoids the filesystem mutation a path graft would
-- need (the plugin lib's init.lua sits directly at `<lua-dir>/init.lua`
-- rather than `<lua-dir>/<name>/`).
do
  local function path_exists(p)
    local f = io.open(p, "r")
    if f == nil then return false end
    f:close()
    return true
  end

  -- `fallbacks` is a `table.pack` result so callers can pass `nil`
  -- entries (e.g. for unset env vars) without losing later candidates.
  -- ipairs would stop at the first nil hole; we iterate up to
  -- `fallbacks.n` and skip nils explicitly. This is the bug that hid
  -- the bootstrap-clone fallback when NEFOR_DEV_DIR was unset.
  local function pick_dir(env_var, sentinel, fallbacks)
    local candidates = {}
    local explicit = os.getenv(env_var)
    if explicit and explicit ~= "" then
      candidates[#candidates + 1] = explicit
    end
    for i = 1, fallbacks.n do
      local f = fallbacks[i]
      if f and f ~= "" then candidates[#candidates + 1] = f end
    end
    for _, c in ipairs(candidates) do
      if path_exists(c .. sentinel) then return c end
    end
    return nil
  end

  local config_dir = os.getenv("NEFOR_CONFIG_DIR")
  local dev_dir     = os.getenv("NEFOR_DEV_DIR")
  local runtime_dir = os.getenv("NEFOR_RUNTIME_ROOT")
  local data_dir    = os.getenv("NEFOR_DATA_DIR")
  -- Bootstrap clone path (team consumers); mirrors how
  -- nefor.fs.data_root resolves $XDG_DATA_HOME/nefor or
  -- $HOME/.local/share/nefor on a fresh machine.
  local xdg_data   = os.getenv("XDG_DATA_HOME")
  local home       = os.getenv("HOME")
  local pm_root
  if data_dir and data_dir ~= "" then
    pm_root = data_dir .. "/nefor"
  elseif xdg_data and xdg_data ~= "" then
    pm_root = xdg_data .. "/nefor/nefor"
  elseif home and home ~= "" then
    pm_root = home .. "/.local/share/nefor/nefor"
  end


  local tui_lua_dir = pick_dir("NEFOR_TUI_LUA_DIR", "/init.lua", table.pack(
    dev_dir    and (dev_dir    .. "/plugins/nefor-tui/lua") or nil,
    runtime_dir and (runtime_dir .. "/plugins/nefor-tui/lua") or nil,
    pm_root    and (pm_root    .. "/plugins/nefor-tui/lua") or nil,
    config_dir and (config_dir .. "/../plugins/nefor-tui/lua") or nil,
    "./plugins/nefor-tui/lua",
    "../plugins/nefor-tui/lua"
  ))
  if tui_lua_dir == nil then
    error("examples/nefor-agent/chat.lua: could not locate plugins/nefor-tui/lua/init.lua")
  end

  -- The entry script and its sibling modules are one canonical consumer.
  -- NEFOR_CONFIG_DIR is deliberately absent: foreign configs contribute
  -- only `config.active.chat_extension` and cannot shadow update/slash/
  -- statusline with stale reducer copies. nefor-tui exports the `--script`
  -- parent as NEFOR_STARTER_CHAT_DIR; explicit overrides remain available to
  -- in-process tests and development launchers.
  local chat_dir = pick_dir("NEFOR_STARTER_CHAT_DIR", "/update.lua", table.pack(
    dev_dir    and (dev_dir    .. "/examples/nefor-agent/chat") or nil,
    runtime_dir and (runtime_dir .. "/examples/nefor-agent/chat") or nil,
    pm_root    and (pm_root    .. "/examples/nefor-agent/chat") or nil,
    "./examples/nefor-agent/chat",
    "../examples/nefor-agent/chat"
  ))
  if chat_dir == nil then
    error("examples/nefor-agent/chat.lua: could not locate examples/nefor-agent/chat submodules")
  end

  local chat_parent = chat_dir:match("^(.*)/chat$")
  local config_lua_dir = pick_dir("NEFOR_STARTER_CONFIG_DIR", "/config/init.lua", table.pack(
    config_dir,
    dev_dir    and (dev_dir    .. "/examples/nefor-agent") or nil,
    runtime_dir and (runtime_dir .. "/examples/nefor-agent") or nil,
    chat_parent,
    pm_root    and (pm_root    .. "/examples/nefor-agent") or nil,
    "./examples/nefor-agent",
    "../examples/nefor-agent"
  ))
  if config_lua_dir ~= nil then
    package.path = table.concat({
      config_lua_dir .. "/?.lua",
      config_lua_dir .. "/?/init.lua",
      package.path,
    }, ";")
  end

  -- The shared Lua tree (`NEFOR_ROOT/lua`) holds `core.*` and `libs.*`.
  -- The chat mechanism modules live at `libs/chat/*`; the opinion files
  -- in this dir require them as `libs.chat.<m>` (and the moved modules
  -- require each other the same way). The engine VM exposes this tree
  -- via its own package.path; the tui VM gets a vanilla one, so graft
  -- it here. `tui_lua_dir` already resolved to `<root>/plugins/nefor-tui/lua`,
  -- so `<root>/lua` is its sibling — the standard candidate list mirrors
  -- the tui/chat dir resolution above.
  local tui_root = tui_lua_dir:match("^(.*)/plugins/nefor%-tui/lua$")
  local lua_dir = pick_dir("NEFOR_LUA_DIR", "/core/ncp.lua", table.pack(
    dev_dir      and (dev_dir      .. "/lua") or nil,
    tui_root     and (tui_root     .. "/lua") or nil,
    runtime_dir  and (runtime_dir  .. "/lua") or nil,
    pm_root      and (pm_root      .. "/lua") or nil,
    "./lua",
    "../lua"
  ))
  if lua_dir == nil then
    error("examples/nefor-agent/chat.lua: could not locate the shared lua/ tree (libs.chat.*)")
  end
  package.path = table.concat({
    lua_dir .. "/?.lua",
    lua_dir .. "/?/init.lua",
    package.path,
  }, ";")

  local function make_prefix_searcher(prefix, root)
    return function(name)
      if name ~= prefix and name:sub(1, #prefix + 1) ~= prefix .. "." then
        return nil
      end
      local rel
      if name == prefix then
        rel = "/init.lua"
      else
        local sub = name:sub(#prefix + 2):gsub("%.", "/")
        rel = "/" .. sub .. ".lua"
      end
      local file_path = root .. rel
      if not path_exists(file_path) then
        local init_path = root .. rel:gsub("%.lua$", "/init.lua")
        if path_exists(init_path) then file_path = init_path
        else return "\n\tno file " .. file_path end
      end
      local chunk, err = loadfile(file_path)
      if chunk == nil then return "\n\t" .. tostring(err) end
      return chunk, file_path
    end
  end

  local searchers = package.searchers or package.loaders
  table.insert(searchers, 1, make_prefix_searcher("nefor-tui", tui_lua_dir))
  table.insert(searchers, 1, make_prefix_searcher("chat",       chat_dir))
end

local function active_config()
  local ok, cfg = pcall(function() return require("config").active end)
  if ok and type(cfg) == "table" then return cfg end
  return {}
end

local extensions = require("libs.chat.extensions")
extensions.load(active_config())

local history = require("libs.chat.history")
local view    = require("libs.chat.view")
local update  = require("chat.update")

local function sidebar_fixture_runs()
  if os.getenv("NEFOR_TEST_SIDEBAR_OVERFLOW") ~= "1" then return {} end
  local nodes = {}
  for i = 1, 20 do
    nodes[string.format("fixture-%02d.member", i)] = {
      reasoner = "test-fixture",
      status = "pending",
      started_at_ms = 0,
      finished_at_ms = nil,
      seq = i,
    }
  end
  return {
    ["sidebar-overflow-fixture"] = {
      run_id = "sidebar-overflow-fixture",
      run_name = "sidebar-overflow-fixture",
      principal = "test",
      total_nodes = 20,
      started_at_ms = 0,
      nodes = nodes,
      completed_at_ms = nil,
      status = nil,
      rejected = 0,
      noops = 0,
      actor_seq = 20,
    },
  }
end

local function initial_state()
  local cfg = active_config()
  local fixture_runs = sidebar_fixture_runs()
  if os.getenv("NEFOR_TEST_SIDEBAR_OVERFLOW") == "1" and next(fixture_runs) == nil then
    error("sidebar overflow fixture environment was visible but fixture construction failed")
  end
  local state = {
    entries          = {},
    in_flight        = nil,
    pending_graph_results = nil,
    -- The selected conversation is a manager-owned identity. Provider request
    -- and chat handles never cross into presentation state.
    conversation_id = nil,
    conversation_projection = require("libs.chat.conversation_projection").new(),
    input_value      = "",
    show_sidebar     = true,
    -- Pane key focus: "prompt" | "sidebar" (Tab/Shift-Tab cycles). The
    -- prompt widget's `focused` flag derives from this in view.lua.
    focus            = "prompt",
    sidebar_cursor   = 1,
    runs             = fixture_runs,
    sidebar_folds    = {},
    -- TUI-local projection of canonical MAG/capability facts.
    scope_to_run     = {},
    node_previews    = {},
    mag_arrivals     = {},
    capability_owners = {},
    popup            = nil,
    stats            = {},
    current_context_tokens = nil,
    pending          = false,
    turn_started_at  = nil,
    last_turn_duration_ms = nil,
    model            = cfg.default_model,
    provider         = cfg.default_provider,
    mode             = "default",
    reasoning_effort = cfg.default_reasoning_effort,
    max_tokens       = nil,
    gate_mode        = "safe",
    auth             = {},
    supports_usage   = {},
    usage            = {},
    expanded_details = false,
    raw_tool_id     = nil,
    tool_displays   = {},
    completion       = nil,
    last_esc_ms      = nil,
    escape_token     = nil,
    escape_token_seq = 0,
    escape_count     = nil,
    last_ctrl_c_ms   = nil,
    exit_token       = nil,
    exit_token_seq   = 0,
    runs             = {},
    toasts           = {},
    -- Hydrate from <data_root>/input-history so arrow-up in the chat
    -- input recalls submissions from prior nefor processes. Empty on
    -- first run / read failure.
    prompt_history   = history.load(),
    history_cursor   = nil,
  }
  local patch = extensions.initial_patch(state)
  if patch ~= nil then
    for key, value in pairs(patch) do state[key] = value end
  end
  return state
end

tui.start {
  initial_state = initial_state(),
  view          = view.render,
  update        = update.update,
}

-- Startup prompts must wait until this process has loaded the composition;
-- the NCP ready handshake only establishes transport readiness.
tui.emit { kind = "chat.surface.ready" }
