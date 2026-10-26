local json = nefor.json
local original = json.decode
local parses = 0
json.decode = function(payload)
  parses = parses + 1
  return original(payload)
end
package.loaded["core.json_decode"] = nil
local decode = require("core.json_decode").owned

-- Replay-shaped traffic includes multi-megabyte immutable strings, canonical
-- facts, targeting metadata, nulls and empty containers. Every consumer must
-- receive the same value as an independent parse, even after another mutates it.
local large = string.rep("quote\" slash\\ newline\n música 😀", 32768)
local payloads = {
  json.encode({ type = "event", from = "tool-gate", body = {
    kind = "tool.result", result = large, containers = original('[[],{},null]'),
  } }),
  '{"type":"event","from":"conversation-manager","body":{"kind":"conversation.fact.recorded","event":{"sequence":1,"kind":"created","conversation_id":"c"}}}',
  '{"type":"event","from":"mag","body":{"kind":"mag.arrival","value":[1,true,null,{},[]]}}',
  'null', 'false', '42', '"text"', '{}', '[]',
}
local function same(a, b)
  if type(a) ~= type(b) then return false end
  if type(a) ~= "table" then return a == b end
  if json.is_array(a) ~= json.is_array(b) then return false end
  for k, v in pairs(a) do if not same(v, b[k]) then return false end end
  for k in pairs(b) do if a[k] == nil then return false end end
  return true
end
for _, payload in ipairs(payloads) do
  local expected = original(payload)
  local before = parses
  for _ = 1, 9 do
    local value = decode(payload)
    assert(same(value, expected), "consumer differs from uncached decoder")
    if type(value) == "table" then
      if value.body then value.body.kind = "mutated"; value.body.result = "changed" end
      value["extra"] = { mutation = true }
    end
  end
  assert(parses == before + 1, "adjacent consumers parse once")
end
-- Cache replacement is bounded to one payload, not all historical events.
local before = parses
decode(payloads[1]); decode(payloads[2]); decode(payloads[1])
assert(parses == before + 3)
for _, invalid in ipairs({ '{', '{"body":}', '[1,]', 'true trailing' }) do
  local expected_ok = pcall(original, invalid)
  for _ = 1, 2 do
    assert(pcall(decode, invalid) == expected_ok, "invalid JSON must still fail")
  end
end
json.decode = original
