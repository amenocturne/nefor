-- starter/jira/init.lua — Jira issue lookup tool for the lead orchestrator.
--
-- Advertises a single `jira` tool via tool-gate. The lead can call it
-- directly without spinning up an explorer subagent.
--
-- Tool: jira({ key = "ITAL-1234" })
-- Returns a formatted plain-text summary: header, metadata, description,
-- and comments. Issue links are not available via the dp CLI.

local json     = nefor.json
local envelope = require("core.envelope")
local emit_as  = envelope.emit_as

local SOURCE_NAME = "jira-tools"

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

-- Strip Jira wiki markup to readable plain text.
local function strip_markup(s)
  if type(s) ~= "string" then return "" end
  s = s:gsub("\r\n", "\n"):gsub("\r", "\n")
  -- {panel:title=X|...} → \n### X\n
  s = s:gsub("{panel:title=([^|}]+)[^}]*}", function(t)
    return "\n### " .. t:match("^%s*(.-)%s*$") .. "\n"
  end)
  s = s:gsub("{panel}", "")
  -- {quote}...{quote} — just remove the tags
  s = s:gsub("{quote}", "")
  -- {code...}...{/code}, {noformat...}...{noformat} — strip tags, keep content
  s = s:gsub("{noformat[^}]*}", ""):gsub("{code[^}]*}", "")
  -- [text|url] → text
  s = s:gsub("%[([^|%]]+)|[^%]]+%]", "%1")
  -- [[url]] → url
  s = s:gsub("%[%[([^%]]+)%]%]", "%1")
  -- [url] → url
  s = s:gsub("%[([^%]]+)%]", "%1")
  -- Remove excessive blank lines
  s = s:gsub("\n\n\n+", "\n\n")
  return s:match("^%s*(.-)%s*$") or ""
end

local MONTHS = {"Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"}
local function fmt_date(iso)
  if type(iso) ~= "string" then return "" end
  local y, m, d = iso:match("^(%d%d%d%d)-(%d%d)-(%d%d)")
  if not y then return iso end
  return string.format("%d %s %s", tonumber(d), MONTHS[tonumber(m)] or m, y)
end

-- dp prepends an update notice to stdout — find the JSON start.
local function parse_response(raw)
  local start = raw:find("{")
  if not start then return nil, "no JSON in dp output" end
  local ok, data = pcall(json.decode, raw:sub(start))
  if not ok then return nil, "JSON parse error: " .. tostring(data) end
  return data
end

-- nefor.json returns a special null userdata for JSON null values rather
-- than Lua nil, so truthy checks like `x and x.field` aren't safe for
-- nested objects that may be null. Use type checks throughout.
local function tget(t, ...)
  local cur = t
  for _, k in ipairs({...}) do
    if type(cur) ~= "table" then return nil end
    cur = cur[k]
  end
  if type(cur) == "string" or type(cur) == "number" or type(cur) == "boolean" then
    return cur
  end
  if type(cur) == "table" then return cur end
  return nil  -- nil or userdata null → nil
end

local function format_issue(data)
  local issue = type(data.issues) == "table" and data.issues[1]
  if not issue then return nil, "no issue in response" end
  local f = issue.fields
  if type(f) ~= "table" then return nil, "no fields in issue" end

  local out = {}

  -- Header
  out[#out+1] = issue.key .. " — " .. (tget(f, "summary") or "(no summary)")

  -- Metadata line
  local meta = {}
  local t = tget(f, "issuetype", "name"); if t then meta[#meta+1] = t end
  local p = tget(f, "priority",  "name"); if p then meta[#meta+1] = p end
  local s = tget(f, "status",    "name"); if s then meta[#meta+1] = s end
  local sp = tget(f, "customfield_10003")
  if type(sp) == "number" then meta[#meta+1] = "SP: " .. sp end
  local epic = tget(f, "customfield_11914")
  if type(epic) == "string" and #epic > 0 then meta[#meta+1] = "Epic: " .. epic end
  if #meta > 0 then out[#out+1] = table.concat(meta, " · ") end

  -- Assignee + Team
  out[#out+1] = "Assignee: " .. (tget(f, "assignee", "displayName") or "—")
  local team = tget(f, "customfield_156807", "value")
  if team then out[#out+1] = "Team: " .. team end

  -- Description
  local desc = tget(f, "description")
  if type(desc) == "string" and #desc > 0 then
    out[#out+1] = ""
    out[#out+1] = "── Description " .. string.rep("─", 55)
    out[#out+1] = strip_markup(desc)
  end

  -- Comments
  local comments = tget(f, "comment", "comments")
  if type(comments) == "table" and #comments > 0 then
    out[#out+1] = ""
    out[#out+1] = "── Comments " .. string.rep("─", 58)
    for _, c in ipairs(comments) do
      if type(c) == "table" then
        local author = tget(c, "author", "displayName") or "?"
        out[#out+1] = author .. " · " .. fmt_date(tget(c, "created") or "")
        local body = strip_markup(tget(c, "body") or "")
        for line in (body .. "\n"):gmatch("([^\n]*)\n") do
          if #line > 0 then out[#out+1] = "  " .. line end
        end
      end
    end
  end

  return table.concat(out, "\n")
end

local function tool_jira(firing_id, args)
  local key = args and args.key
  if type(key) ~= "string" or #key == 0 then
    emit_err(firing_id, "jira: args.key must be a non-empty string (e.g. 'ITAL-1234')")
    return
  end
  key = key:upper()

  local out = nefor.process.run { cmd = "dp", args = { "jira", "issue", "--key", key } }
  if type(out) ~= "table" then
    emit_err(firing_id, "jira: nefor.process.run returned non-table")
    return
  end
  if out.code ~= 0 then
    local stderr = tostring(out.stderr or "")
    if stderr:find("[Uu]nauth") or stderr:find("auth") or stderr:find("login") then
      emit_err(firing_id, "jira: not authenticated — ask user to run `dp auth login`")
    else
      emit_err(firing_id, string.format("jira: dp exited %d: %s", out.code, stderr))
    end
    return
  end

  local data, err = parse_response(tostring(out.stdout or ""))
  if not data then
    emit_err(firing_id, "jira: " .. tostring(err))
    return
  end

  local result, fmt_err = format_issue(data)
  if not result then
    emit_err(firing_id, "jira: " .. tostring(fmt_err))
    return
  end

  emit_ok(firing_id, result)
end

local TOOL_HANDLERS = { jira = tool_jira }

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
        name = "jira",
        description =
          "Fetch a Jira issue by key. Returns status, type, priority, " ..
          "story points, epic, assignee, full description, and comments. " ..
          "Use when the user or a task references a Jira ticket like ITAL-1234.",
        parameters = {
          type = "object",
          properties = {
            key = { type = "string", description = "Issue key, e.g. 'ITAL-1234'." },
          },
          required = { "key" },
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
