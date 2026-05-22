-- starter/confluence/init.lua — Confluence wiki fetch tool.
--
-- Advertises `wiki({ page_id })` via tool-gate. Calls npx
-- @acq-tech/confluence directly via bash; no Python wrapper needed.
-- Config (host + username) lives in config/init.lua as M.confluence.

local json     = nefor.json
local envelope = require("core.envelope")
local emit_as  = envelope.emit_as

local SOURCE_NAME = "confluence-tools"

local function emit_ok(firing_id, text)
  emit_as(SOURCE_NAME, nil, {
    kind   = "tool.result",
    id     = firing_id,
    output = { text = tostring(text or "") },
  })
end

local function emit_err(firing_id, err)
  emit_as(SOURCE_NAME, nil, {
    kind  = "tool.result",
    id    = firing_id,
    error = tostring(err),
  })
end

local function find_page_file(tmpdir, page_id)
  local index = tmpdir .. "/" .. page_id .. "/index.md"
  local f = io.open(index, "r")
  if f then f:close(); return index end
  local flat = tmpdir .. "/" .. page_id .. ".md"
  f = io.open(flat, "r")
  if f then f:close(); return flat end
  return nil
end

-- Scan tmpdir for numeric directory names that are not the main page.
-- These are subpage IDs created when @acq-tech/confluence downloads children.
local function find_subpage_ids(tmpdir, page_id)
  local result = nefor.process.run {
    cmd  = "bash",
    args = { "-c", string.format(
      "find %q -mindepth 1 -maxdepth 2 -type d 2>/dev/null",
      tmpdir
    )},
  }
  if type(result) ~= "table" or result.code ~= 0 then return {} end
  local ids = {}
  local seen = {}
  for line in (result.stdout or ""):gmatch("[^\n]+") do
    local id = line:match("/(%d+)$")
    if id and id ~= page_id and not seen[id] then
      seen[id] = true
      ids[#ids + 1] = id
    end
  end
  return ids
end

local function read_file(path)
  local f, err = io.open(path, "r")
  if not f then return nil, err end
  local content = f:read("*a")
  f:close()
  return content
end

local function tool_wiki(firing_id, args)
  local page_id = args and args.page_id
  if type(page_id) == "number" then
    page_id = tostring(math.floor(page_id))
  end
  if type(page_id) ~= "string" or #page_id == 0 then
    emit_err(firing_id, "wiki: args.page_id must be a page ID string (e.g. '12345678')")
    return
  end

  local cfg = require("config").confluence
  if type(cfg) ~= "table" or type(cfg.host) ~= "string" then
    emit_err(firing_id, "wiki: missing config — add M.confluence = { host } to config/init.lua")
    return
  end

  local who = nefor.process.run { cmd = "whoami", args = {} }
  if type(who) ~= "table" or who.code ~= 0 then
    emit_err(firing_id, "wiki: could not determine username via whoami")
    return
  end
  local username = who.stdout:match("^%s*(.-)%s*$")
  if not username or #username == 0 then
    emit_err(firing_id, "wiki: whoami returned empty output")
    return
  end

  local host = cfg.host:gsub("/$", "")
  local url  = host .. "/pages/viewpage.action?pageId=" .. page_id
  local tmpdir  = "/tmp/nefor-confluence-" .. page_id .. "-" .. tostring(os.time())

  os.execute("mkdir -p " .. tmpdir)

  local cmd = string.format(
    "printf 'Y\\n' | npx @acq-tech/confluence --username %q --folder-path %q %q 2>&1",
    username, tmpdir, url
  )
  local out = nefor.process.run { cmd = "bash", args = { "-c", cmd } }

  if type(out) ~= "table" then
    os.execute("rm -rf " .. tmpdir)
    emit_err(firing_id, "wiki: nefor.process.run returned non-table")
    return
  end
  if out.code ~= 0 then
    os.execute("rm -rf " .. tmpdir)
    emit_err(firing_id, string.format("wiki: npx exited %d: %s", out.code, tostring(out.stdout or "")))
    return
  end

  local page_path = find_page_file(tmpdir, page_id)
  if not page_path then
    os.execute("rm -rf " .. tmpdir)
    emit_err(firing_id, "wiki: page file not found after download (page_id=" .. page_id .. ")")
    return
  end

  local content, err = read_file(page_path)
  local subpage_ids   = find_subpage_ids(tmpdir, page_id)
  os.execute("rm -rf " .. tmpdir)

  if not content then
    emit_err(firing_id, "wiki: could not read page file: " .. tostring(err))
    return
  end

  if #subpage_ids > 0 then
    content = content .. "\n\n---\nSubpages: " .. table.concat(subpage_ids, ", ")
  end

  emit_ok(firing_id, content)
end

local TOOL_HANDLERS = { wiki = tool_wiki }

local function handle_tool_invoke(body)
  local firing_id = body.id
  if type(firing_id) ~= "string" then return end
  local handler = TOOL_HANDLERS[body.name]
  if not handler then
    emit_err(firing_id, SOURCE_NAME .. ": unknown tool '" .. tostring(body.name) .. "'")
    return
  end
  local ok, err = pcall(handler, firing_id, body.args or {})
  if not ok then
    emit_err(firing_id, SOURCE_NAME .. "." .. tostring(body.name)
      .. ": handler raised: " .. tostring(err))
  end
end

local advertised = false
local function advertise_tools(gate_name)
  if advertised then return end
  advertised = true
  emit_as(SOURCE_NAME, nil, {
    kind   = (gate_name or "tool-gate") .. ".tools.advertise",
    source = SOURCE_NAME,
    tools  = {
      {
        name        = "wiki",
        description =
          "Fetch a Confluence wiki page by its numeric page ID. Returns " ..
          "the full page as Markdown. If the page has subpages, their IDs " ..
          "are listed at the end under 'Subpages:' — call wiki again for " ..
          "each one you need. Use when the user references a Confluence " ..
          "page or wiki URL containing a pageId.",
        parameters  = {
          type       = "object",
          properties = {
            page_id = {
              type        = "string",
              description = "Numeric Confluence page ID (from the URL's pageId= parameter).",
            },
          },
          required   = { "page_id" },
        },
      },
    },
  })
end

local function receive_msg(entry)
  if entry.origin == "step" and entry.target ~= nil then return end
  local payload = entry.payload
  if type(payload) ~= "string" or payload == "" then return end
  local ok, decoded = pcall(json.decode, payload)
  if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
  local body = decoded.body
  local kind = body.kind
  if type(kind) ~= "string" then return end

  if kind == SOURCE_NAME .. ".tool.invoke" then
    handle_tool_invoke(body)
    return
  end
  if kind == "tool-gate.hello" then
    advertise_tools("tool-gate")
    return
  end
end

return {
  name        = SOURCE_NAME,
  receive_msg = receive_msg,
  send_msg    = function(_) end,
}
