-- starter/tool-validator — config composition for permission classification.
--
-- The neutral JSON manifest under mag/lib is config-owned data shared by MAG
-- bindings and this Lua composition. Each runtime parses it with its native
-- JSON binding; neither runtime parses the other's programming language.

local root = NEFOR_CONFIG_DIR or "."
local path = root .. "/mag/lib/nefor/toolsets.json"
local file, open_error = io.open(path, "r")
if not file then
  error("starter/tool-validator: cannot read canonical toolsets at " .. path .. ": " .. tostring(open_error))
end
local source = file:read("*a")
file:close()

local ok, manifest = pcall(nefor.json.decode, source)
if not ok then
  error("starter/tool-validator: invalid canonical toolsets JSON at " .. path .. ": " .. tostring(manifest))
end

local function string_array(field)
  local value = type(manifest) == "table" and manifest[field] or nil
  if type(value) ~= "table" then
    error("starter/tool-validator: canonical toolsets field `" .. field .. "` must be an array of strings")
  end
  for index, name in ipairs(value) do
    if type(name) ~= "string" then
      error("starter/tool-validator: canonical toolsets field `" .. field .. "` member " .. index .. " must be a string")
    end
  end
  return value
end

return require("libs.tool-validator").build {
  read_only_tools = string_array("read_only"),
}
