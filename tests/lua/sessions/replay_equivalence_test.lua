local json = nefor.json
local sessions = require("libs.sessions")
local decoder = require("core.json_decode")
local owned = decoder.owned
local root = os.getenv("NEFOR_DATA_DIR") .. "/sessions"
nefor.fs.mkdir_p(root)
sessions.configure { root = root }
local path = root .. "/equivalence.jsonl"
local file = assert(io.open(path, "w"))
file:write(json.encode({ _session = true, session_id = "equivalence" }), "\n")
local expected = {}
local function record(body, from, target)
  local payload = json.encode({ type = "event", from = from or "fixture", body = body })
  expected[#expected + 1] = { payload = payload, target = target }
  file:write(json.encode({ origin = "step", payload = payload, target = target }), "\n")
end
local facts = {
  { kind = "created", conversation_id = "c", provenance = { surface = "lead" } },
  { kind = "active_selected", conversation_id = "c" },
  { kind = "message_started", conversation_id = "c", message_id = "m", role = "user" },
  { kind = "content_chunk_appended", conversation_id = "c", message_id = "m",
    chunk = { kind = "text", data = "retained canonical text" } },
  { kind = "message_completed", conversation_id = "c", message_id = "m" },
}
for index, fact in ipairs(facts) do
  fact.sequence = index
  fact.event_id = "fact-" .. index
  record({ kind = "conversation.fact.recorded", session_id = "equivalence",
    duplicate = false, event = fact }, "conversation-manager")
end
for index = 1, 140 do
  record({ kind = index % 2 == 0 and "mag.arrival" or "tool.result", index = index,
    result = index % 35 == 0 and string.rep("escaped\"\\\n🌊", 131072) or "small",
    containers = json.decode('[[],{},null]'),
  }, "fixture", index % 3 == 0 and "archive" or nil)
end
file:write("invalid JSON\n", json.encode({ origin = "plugin", payload = "{}" }), "\n")
file:close()
local initial_file = assert(io.open(path, "r"))
local initial = initial_file:read("*a")
initial_file:close()
local function run(decode)
  decoder.owned = decode
  sessions._internals.reset_state()
  local manager = require("libs.conversation-manager.runtime").build()
  local sent, replayed = {}, {}
  nefor.engine.send = function(payload, target)
    sent[#sent + 1] = { payload = payload, target = target }
  end
  sessions.init("equivalence", "request")
  local cursor = 1
  while cursor <= #sent do
    local entry = sent[cursor]
    local env = json.decode(entry.payload)
    if env.from == "fixture" or env.body.kind == "conversation.fact.recorded" then
      replayed[#replayed + 1] = entry
    end
    sessions.receive_msg(entry)
    manager.receive_msg(entry)
    cursor = cursor + 1
  end
  assert(sessions.ready())
  assert(#replayed == #expected)
  for index, entry in ipairs(replayed) do
    assert(entry.payload == expected[index].payload, "raw recorded event order/value changed")
    assert(entry.target == expected[index].target, "recorded target changed")
  end
  local state = manager._internals.get("c")
  assert(state, "canonical conversation must reconstruct")
  return sent, json.encode(state)
end
local baseline, baseline_state = run(json.decode)
local optimized, optimized_state = run(owned)
assert(#baseline == #optimized)
for index, entry in ipairs(baseline) do
  assert(entry.payload == optimized[index].payload, "replay framing or output changed")
  assert(entry.target == optimized[index].target)
end
-- Encoding table key order is unspecified; compare the canonical values.
local function same(a, b)
  if type(a) ~= type(b) then return false end
  if type(a) ~= "table" then return a == b end
  if json.is_array(a) ~= json.is_array(b) then return false end
  for k, v in pairs(a) do if not same(v, b[k]) then return false end end
  for k in pairs(b) do if a[k] == nil then return false end end
  return true
end
assert(same(json.decode(baseline_state), json.decode(optimized_state)))
sessions._internals.reset_state()
decoder.owned = owned
local final_file = assert(io.open(path, "r"))
assert(final_file:read("*a") == initial, "replay must not persist duplicate traffic")
final_file:close()
