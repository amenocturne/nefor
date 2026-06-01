-- starter/chat.lua — chat surface as a Lua composition over tui.* primitives.
--
-- The engine ships zero opinion. Every color, every layout, every
-- glyph below is editable — this composition IS the chat surface's
-- identity. The submodules under `chat/` carry the per-concern code;
-- this entry file installs the require searcher, glues the submodules
-- together, declares initial state, and hands view/update to
-- `tui.start`.
--
-- Inbound chat-contract events handled here:
--   chat.message.append, chat.stream.delta, chat.stream.end,
--   chat.stream.reasoning_delta, chat.stream.reasoning_end,
--   chat.session.stats, chat.tool.start, chat.tool.end,
--   chat.popup, chat.auth.status, chat.model.set_ack, chat.models.listed,
--   chat.tool.popup_request, tool-gate.mode_changed,
--   graph.run_started, graph.node.fired, tool.result.
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
  local dev_dir    = os.getenv("NEFOR_DEV_DIR")
  local local_dir  = os.getenv("NEFOR_LOCAL_DIR")
  local data_dir   = os.getenv("NEFOR_DATA_DIR")
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
    local_dir  and (local_dir  .. "/plugins/nefor-tui/lua") or nil,
    pm_root    and (pm_root    .. "/plugins/nefor-tui/lua") or nil,
    config_dir and (config_dir .. "/../plugins/nefor-tui/lua") or nil,
    "./plugins/nefor-tui/lua",
    "../plugins/nefor-tui/lua"
  ))
  if tui_lua_dir == nil then
    error("starter/chat.lua: could not locate plugins/nefor-tui/lua/init.lua")
  end

  -- Starter chat-sub dir holds the submodules. Defaults to
  -- `<NEFOR_CONFIG_DIR>/chat` (the user installs starter/ as their
  -- config dir; chat/ is a sibling of init.lua).
  -- Order matters: chat/* is the user-editable surface (the engine
  -- runs the copy at $NEFOR_CONFIG_DIR/chat/init.lua, and user edits
  -- to sibling submodules should win). Prefer config_dir over local or
  -- pm-cloned upstream copies. NEFOR_DEV_DIR still wins overall for
  -- in-repo iteration.
  local chat_dir = pick_dir("NEFOR_STARTER_CHAT_DIR", "/common.lua", table.pack(
    dev_dir    and (dev_dir    .. "/starter/chat") or nil,
    config_dir and (config_dir .. "/chat") or nil,
    local_dir  and (local_dir  .. "/starter/chat") or nil,
    pm_root    and (pm_root    .. "/starter/chat") or nil,
    "./starter/chat",
    "../starter/chat"
  ))
  if chat_dir == nil then
    error("starter/chat.lua: could not locate starter/chat submodules")
  end

  local chat_parent = chat_dir:match("^(.*)/chat$")
  local config_lua_dir = pick_dir("NEFOR_STARTER_CONFIG_DIR", "/config/init.lua", table.pack(
    dev_dir    and (dev_dir    .. "/starter") or nil,
    config_dir,
    local_dir  and (local_dir  .. "/starter") or nil,
    chat_parent,
    pm_root    and (pm_root    .. "/starter") or nil,
    "./starter",
    "../starter"
  ))
  if config_lua_dir ~= nil then
    package.path = table.concat({
      config_lua_dir .. "/?.lua",
      config_lua_dir .. "/?/init.lua",
      package.path,
    }, ";")
  end

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

local history = require("chat.history")
local view    = require("chat.view")
local update  = require("chat.update")

local function active_config()
  local ok, cfg = pcall(function() return require("config").active end)
  if ok and type(cfg) == "table" then return cfg end
  return {}
end

local function initial_state()
  local cfg = active_config()
  return {
    entries          = {},
    in_flight        = nil,
    input_value      = "",
    show_sidebar     = true,
    popup            = nil,
    stats            = {},
    pending          = false,
    pending_followups = "",
    turn_started_at  = nil,
    last_turn_duration_ms = nil,
    model            = cfg.default_model,
    provider         = cfg.default_provider,
    reasoning_effort = cfg.default_reasoning_effort,
    max_tokens       = nil,
    gate_mode        = "safe",
    auth             = {},
    expanded_details = false,
    completion       = nil,
    last_esc_ms      = nil,
    dag_runs         = {},
    toasts           = {},
    -- Hydrate from <data_root>/input-history so arrow-up in the chat
    -- input recalls submissions from prior nefor processes. Empty on
    -- first run / read failure.
    prompt_history   = history.load(),
    history_cursor   = nil,
  }
end

tui.start {
  initial_state = initial_state(),
  view          = view.render,
  update        = update.update,
}
