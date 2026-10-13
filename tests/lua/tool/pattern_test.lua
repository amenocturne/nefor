-- Pattern subset used by tool schemas. Driven by engine/tests/tool_constructor_test.rs.

local pattern = require("libs.tool.pattern")

local function matches(source, subject)
  local matcher = assert(pattern.compile(source))
  return matcher(subject)
end

local cases = {
  { "\\.(mag|magl)$", "flow.mag", true },
  { "\\.(mag|magl)$", "dir/flow.magl", true },
  { "\\.(mag|magl)$", "flow.mag.txt", false },
  { "\\.(mag|magl)$", "flowmag", false },
  { "^(.*/)?[^/]+\\.(mag|magl)$", "flow.mag", true },
  { "^(.*/)?[^/]+\\.(mag|magl)$", "dir/sub/flow.magl", true },
  { "^(.*/)?[^/]+\\.(mag|magl)$", ".mag", false },
  { "^(.*/)?[^/]+\\.(mag|magl)$", "dir/.mag", false },
  { "^(.*/)?[^/]+\\.(mag|magl)$", "flow.mag.txt", false },
  { "^[a-z][a-z0-9_-]*$", "skill-1", true },
  { "^[a-z][a-z0-9_-]*$", "1skill", false },
  { "^\\d{2,3}$", "12", true },
  { "^\\d{2,3}$", "1234", false },
  { "^\\d{2}$", "12", true },
  { "^a{2,}$", "aaaa", true },
  { "^a{2,}$", "a", false },
  { "^(?:ab)+$", "ababab", true },
  { "^(?:ab)+$", "aba", false },
  { "colou?r", "my color", true },
  { "^\\w+\\s\\S*$", "hello world", true },
  { "^[^/]+$", "a/b", false },
  { "^[^/]+$", "ab", true },
  { "^.*$", "", true },
  { "x*", "", true },
  { "^(a|ab)c$", "abc", true },
  { "^[\\d.]+$", "1.2.3", true },
  { "^\\D+$", "abc", true },
  { "^\\D+$", "a1", false },
  { "^a.c$", "a\nc", false },
  { "^(a*)*$", "aaaa", true },
}
for _, case in ipairs(cases) do
  local got = matches(case[1], case[2])
  assert(got == case[3], string.format("pattern %q on %q: expected %s, got %s",
    case[1], case[2], tostring(case[3]), tostring(got)))
end

local rejected = {
  "(?=a)", "(?<n>a)", "a+?", "a**", "\\b", "\\1", "\\p{L}", "[a", "(a", "a)",
  "*a", "a{2", "[]", "\\u0041", "[z-a]", "a{3,2}", "^*",
}
for _, source in ipairs(rejected) do
  local matcher, err = pattern.compile(source)
  assert(matcher == nil and type(err) == "string",
    "unsupported pattern is rejected at compile time: " .. source)
end

return true
