-- @path preprocessor. Inlines file references like `@starter/chat.lua`
-- into the user's submitted text BEFORE it reaches the provider. The
-- orchestrator's first turn sees the file contents already inlined;
-- large files truncate with a marker pointing at the existing
-- `read_file` tool for the full contents.
--
-- Scope (intentionally small):
--   * pattern is `@<non-whitespace>`; trailing common punctuation
--     (.,;:!?) is shaved off the captured token because it almost
--     never belongs to the path and the user's prompt-tail punctuation
--     would otherwise turn `@a.lua.` into a missing-file silent no-op.
--   * paths that don't resolve / can't be opened leave the `@<token>`
--     as-is — no error surfaces, the user sees the raw text and the
--     model can ask.
--   * inlined block is a fenced HTML-ish `<file path="…">` … `</file>`
--     wrapper. Code-fence language is inferred from extension; unknown
--     extensions render with a plain ``` fence.

local M = {}

local INLINE_BUDGET = 16 * 1024
local TRUNCATION_MARKER =
  "\n... [truncated; use read_file tool for full contents] ..."

local FENCE_LANG = {
  lua = "lua", rs = "rust", md = "md", json = "json", toml = "toml",
  py = "python", js = "javascript", ts = "typescript", tsx = "tsx",
  sh = "bash", bash = "bash", yaml = "yaml", yml = "yaml",
  html = "html", css = "css", go = "go", rb = "ruby", java = "java",
}

local function fence_lang(path)
  local ext = path:match("%.([%w]+)$")
  if ext == nil then return "" end
  return FENCE_LANG[ext:lower()] or ""
end

local function read_file(path)
  local f = io.open(path, "r")
  if f == nil then return nil end
  local data = f:read(INLINE_BUDGET + 1)
  f:close()
  if data == nil then return "" end
  if #data > INLINE_BUDGET then
    return data:sub(1, INLINE_BUDGET) .. TRUNCATION_MARKER
  end
  return data
end

-- Resolve `@token` against the current cwd via `io.open`. Returns
-- (data, resolved_path) on success, (nil, nil) when the token doesn't
-- resolve. Absolute paths are handled implicitly by io.open — the
-- cwd-relative attempt already covers `/abs/path` because Lua treats
-- the leading `/` as absolute. No separate absolute branch is needed.
local function resolve(token)
  local data = read_file(token)
  if data ~= nil then return data, token end
  return nil, nil
end

function M.expand(text)
  if text == nil or text == "" or text:find("@", 1, true) == nil then
    return text
  end
  return (text:gsub("@([^%s]+)", function(token)
    -- Strip trailing prompt punctuation that almost never belongs to
    -- a path (`.`, `,`, `;`, `:`, `!`, `?`, `)`). One pass — multiple
    -- trailing punctuation chars (e.g. `@file.lua?!`) all peel off.
    local trimmed, _trail_n = token:gsub("[%.%,;:!%?%)]+$", "")
    if trimmed == "" then return nil end
    local data, resolved = resolve(trimmed)
    if data == nil then return nil end
    local trail = token:sub(#trimmed + 1)
    local lang = fence_lang(resolved)
    return string.format(
      "<file path=\"%s\">\n```%s\n%s\n```\n</file>%s",
      resolved, lang, data, trail
    )
  end))
end

return M
