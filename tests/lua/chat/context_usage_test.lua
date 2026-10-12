package.preload["nefor-tui"] = function()
  local nil_sentinel = {}
  return {
    widget = {},
    util = {
      NIL = nil_sentinel,
      shallow_merge = function(base, patch)
        local merged = {}
        for key, value in pairs(base or {}) do merged[key] = value end
        for key, value in pairs(patch or {}) do
          if value == nil_sentinel then merged[key] = nil else merged[key] = value end
        end
        return merged
      end,
      bordered_box = function() end,
    },
  }
end

tui = { now_ms = function() return 0 end }

local context_usage = require("libs.chat.context_usage")

local function eq(actual, expected, label)
  assert(actual == expected,
    label .. ": expected " .. tostring(expected) .. ", got " .. tostring(actual))
end


local function measured(total)
  return { kind = "completed_response", total_tokens = total }
end
eq(context_usage.total(measured(0)), 0, "measured zero is valid")
for _, value in ipairs({ -1, 1.5, math.huge, 0/0 }) do
  eq(context_usage.total(measured(value)), nil, "invalid measurement")
end
eq(context_usage.total({ input_tokens = 80 }), nil, "aggregates are not occupancy")
eq(context_usage.total({ context_input_tokens = 123 }), nil, "estimates are not occupancy")
eq(context_usage.from_response({ prompt_tokens = 80, completion_tokens = 20 }).total_tokens,
  100, "generic per-response exact sum")
eq(context_usage.from_response({ total_tokens = 12, billing_components = {} }), nil,
  "operation billing totals are not responses")
local controller = require("libs.chat.controller")
local handlers = controller.default_handlers()
local state = { provider = "p", model = "same", conversation_id = "c",
  current_context_tokens = 80 }
local raw = handlers["conversation.provider.context_usage"]({ provider = "p",
  conversation_id = "c", latest_response_usage = measured(999) }, state)
eq(raw.current_context_tokens, 80, "unrecorded observations cannot bypass canonical correlation")
local active = handlers["conversation.active.changed"]({ kind = "conversation.active.changed", conversation_id = "c" }, state)
local updated = handlers["conversation.projection.delta"]({ kind = "conversation.projection.delta", conversation_id = "c", change = {
  kind = "response_usage_recorded", response_usage = { provider = "p", model = "same",
    latest_response_usage = measured(123) } } }, active)
eq(updated.current_context_tokens, 123, "recorded applicable native response updates total")
local stale = handlers["conversation.projection.delta"]({ kind = "conversation.projection.delta", conversation_id = "other", change = {
  kind = "response_usage_recorded", response_usage = { provider = "p", model = "same",
    latest_response_usage = measured(999) } } }, updated)
eq(stale.current_context_tokens, 123, "another conversation cannot replace measurement")

updated.turn_discarded_assistant = true
local missing = handlers["conversation.projection.delta"]({ kind = "conversation.projection.delta",
  conversation_id = "c", change = { kind = "turn_completed", turn_id = "t",
    terminal = { model = "same", usage = { input_tokens = 999999, output_tokens = 88888 } } } }, updated)
eq(missing.current_context_tokens, 123, "turn aggregate does not overwrite successful round")
local other_model = handlers["conversation.projection.delta"]({ kind = "conversation.projection.delta",
  conversation_id = "c", change = { kind = "response_usage_recorded", response_usage = {
    provider = "p", model = "old", latest_response_usage = measured(999) } } }, updated)
eq(other_model.current_context_tokens, 123, "stale model observation is ignored")
local resumed = handlers["conversation.snapshot"]({ kind = "conversation.snapshot", conversation_id = "c",
  found = true, projection = { messages = {}, response_usage = { provider = "p", model = "same",
    latest_response_usage = measured(54) } } }, updated)
eq(resumed.current_context_tokens, 54, "snapshot restores applicable exact measurement")
local unknown = handlers["conversation.snapshot"]({ kind = "conversation.snapshot", conversation_id = "c",
  found = true, projection = { messages = {} } }, resumed)
eq(unknown.current_context_tokens, nil, "snapshot lacking measurement is explicitly unknown")
local catalog = handlers["chat.models.listed"]({ provider = "p", models = { "same" },
  context_windows = { same = 272000 }, max_context_windows = { same = 872000 } }, updated)
eq(catalog.max_tokens, 272000, "default is distinct from advertised max")
eq(catalog.max_context_tokens, 872000, "advertised maximum retained")
local other = handlers["chat.models.listed"]({ provider = "q", models = { "same" },
  context_windows = { same = 100 }, max_context_windows = { same = 200 } }, catalog)
eq(other.max_tokens, 272000, "same slug in another provider cannot replace default")
eq(other.max_context_tokens, 872000, "same slug cannot replace max")
local cleared = handlers["chat.models.listed"]({ provider = "p", models = { "same" } }, other)
eq(cleared.max_tokens, nil, "new catalog without default clears old capacity")
eq(cleared.max_context_tokens, nil, "new catalog without maximum clears old capacity")
local selection = require("libs.chat.model_selection")
local requested = selection.request(other, "q", "same")
local switched = handlers["chat.model.set_ack"]({ provider = "q", model = "same" }, requested)
eq(switched.max_tokens, 100, "switch to same slug adopts the other provider default")
eq(switched.max_context_tokens, 200, "switch adopts the other provider max")
eq(switched.current_context_tokens, nil, "provider change invalidates prior response")
-- A model selection can finish while a request for the old provider is still
-- running. Its canonical completion must still finalize the transcript.
local function delta(current, change)
  return handlers["conversation.projection.delta"]({
    kind = "conversation.projection.delta", conversation_id = "c", change = change,
  }, current)
end
local racing = handlers["conversation.active.changed"]({
  kind = "conversation.active.changed", conversation_id = "c",
}, { provider = "p", model = "same", conversation_id = "c",
  entries = {}, current_context_tokens = 80 })
racing = delta(racing, { kind = "turn_started", turn_id = "late-turn" })
racing = delta(racing, { kind = "message_started", message = {
  id = "late-message", turn_id = "late-turn", role = "assistant", status = "open",
} })
racing = handlers["chat.model.set_ack"]({ provider = "q", model = "same" },
  selection.request(racing, "q", "same"))
eq(racing.current_context_tokens, nil, "in-flight provider switch clears occupancy")
racing = delta(racing, { kind = "response_usage_recorded", response_usage = {
  provider = "p", model = "same", latest_response_usage = measured(999),
} })
eq(racing.current_context_tokens, nil, "old provider's recorded usage is ignored")
racing = delta(racing, { kind = "message_completed", message = {
  id = "late-message", turn_id = "late-turn", role = "assistant", status = "completed",
  text = "Old provider finished", terminal = { provider = "p", model = "same",
    latest_response_usage = measured(999), usage = { output_tokens = 20 } },
} })
eq(racing.current_context_tokens, nil, "old provider's assistant completion cannot restore occupancy")
eq(racing.entries[#racing.entries].text, "Old provider finished",
  "late completion still finalizes transcript")
racing = delta(racing, { kind = "turn_completed", turn_id = "late-turn",
  terminal = { provider = "p", model = "same", latest_response_usage = measured(999) },
})
eq(racing.current_context_tokens, nil, "old provider's whole-turn completion leaves occupancy unknown")
local applicable = delta(racing, { kind = "message_completed", message = {
  id = "new-message", turn_id = "new-turn", role = "assistant", status = "completed",
  text = "Current provider finished", terminal = { provider = "q", model = "same",
    latest_response_usage = measured(0) },
} })
eq(applicable.current_context_tokens, 0, "applicable completed-response zero remains valid")
local wrong_model = delta(applicable, { kind = "message_completed", message = {
  id = "wrong-model", role = "assistant", status = "completed", text = "Another model",
  terminal = { provider = "q", model = "old", latest_response_usage = measured(777) },
} })
eq(wrong_model.current_context_tokens, 0, "completion for another model is ignored")
print("context_usage_test: all assertions passed")
