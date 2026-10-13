-- POSIX shell quoting.
--
-- `quote` writes one value as a single shell word; `quote_argv` writes an
-- argument vector as the command line a POSIX shell would split back into
-- exactly those arguments. Single quotes are the only quoting used: nothing
-- inside them is special to the shell, and a quote character itself is
-- written as '\'' (close, escaped quote, reopen).

local M = {}

function M.quote(value)
  local quoted = tostring(value):gsub("'", "'\\''")
  return "'" .. quoted .. "'"
end

-- The command text for `argv`, with every element quoted, the program name
-- included, so that no element can be read as an assignment, an operator or
-- an expansion. Returns nil and a message for a vector that is not a
-- non-empty dense list of strings, or that holds a NUL byte (no process
-- argument can).
function M.quote_argv(argv)
  if type(argv) ~= "table" then return nil, "argv must be a list of strings" end
  local count = 0
  for key in pairs(argv) do
    if math.type(key) ~= "integer" or key < 1 then return nil, "argv must be a list of strings" end
    count = count + 1
  end
  if count == 0 then return nil, "argv is empty" end
  local words = {}
  for index = 1, count do
    local argument = argv[index]
    if type(argument) ~= "string" then
      return nil, "argv[" .. index .. "] must be a string"
    end
    if argument:find("\0", 1, true) then
      return nil, "argv[" .. index .. "] contains a NUL byte"
    end
    words[index] = M.quote(argument)
  end
  return table.concat(words, " ")
end

return M
