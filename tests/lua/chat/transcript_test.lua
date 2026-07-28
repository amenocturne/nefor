tui = { now_ms = function() return 1000 end }
package.preload["nefor-tui"] = function()
  local nil_sentinel = {}
  return {
    util = {
      NIL = nil_sentinel,
      shallow_merge = function(base, patch)
        local merged = {}
        for k, v in pairs(base) do merged[k] = v end
        for k, v in pairs(patch) do
          if v == nil_sentinel then merged[k] = nil else merged[k] = v end
        end
        return merged
      end,
    },
  }
end

local transcript = require("libs.chat.transcript")

local function eq(actual, expected, message)
  if actual ~= expected then
    error((message or "values differ") .. ": expected " .. tostring(expected)
      .. ", got " .. tostring(actual), 2)
  end
end

local initial = {
  entries = {},
  in_flight = nil,
  pending = true,
  turn_started_at = 900,
}
local ended, handled = transcript.reduce_assistant_event(initial, {
  kind = "chat.stream.end",
  text = "",
  model = "structured-model",
  duration_ms = 75,
})
eq(handled, true)
eq(#ended.entries, 1)
eq(ended.entries[1].text, "")
eq(ended.entries[1].model, "structured-model")
eq(ended.pending_assistant_projection, 1)

local with_stats = transcript.reduce_assistant_event(ended, {
  kind = "chat.session.stats",
  last_turn_output_tokens = 42,
  last_turn_duration_ms = 80,
})
eq(with_stats.entries[1].output_tokens, 42)
eq(with_stats.entries[1].duration_ms, 80)

local projected = transcript.reduce_assistant_event(with_stats, {
  kind = "chat.message.append",
  role = "assistant",
  text = "validated answer",
})
eq(#projected.entries, 1, "durable answer reuses the provider-owned entry")
eq(projected.entries[1].text, "validated answer")
eq(projected.entries[1].model, "structured-model")
eq(projected.entries[1].output_tokens, 42)
eq(projected.entries[1].duration_ms, 80)
eq(projected.pending_assistant_projection, nil)

local ordinary = transcript.reduce_assistant_event({ entries = {} }, {
  kind = "chat.message.append",
  role = "assistant",
  text = "ordinary answer",
})
eq(#ordinary.entries, 1, "append without projection creates a new entry")
eq(ordinary.entries[1].text, "ordinary answer")
eq(ordinary.entries[1].kind, "text")

print("transcript_test: all assertions passed")
