local queued_input = require("libs.chat.queued_input")

local function eq(actual, expected, message)
  if actual ~= expected then
    error((message or "values differ") .. ": expected " .. tostring(expected)
      .. ", got " .. tostring(actual), 2)
  end
end

local initial = { entries = {}, input_value = "", in_flight = nil }
local direct = queued_input.submit(initial, "first", false)
eq(#direct.entries, 1)
eq(direct.pending_user_echo, "first")
eq(direct.pending_user_echo_idx, 1)

local observed_cli = queued_input.observe_external_submit(initial, "from cli")
eq(#observed_cli.entries, 1, "bus submit becomes visible before durable projection")
eq(observed_cli.entries[1].text, "from cli")
eq(observed_cli.pending_user_echo, "from cli")
local observed_interactive = queued_input.observe_external_submit(direct, "first")
eq(#observed_interactive.entries, 1, "interactive optimistic entry is not duplicated")

local interleaved = {
  entries = { direct.entries[1], { role = "system", text = "result" } },
  pending_user_echo = direct.pending_user_echo,
  pending_user_echo_idx = direct.pending_user_echo_idx,
}
local reconciled, matched = queued_input.reconcile_echo(interleaved, "first")
eq(matched, true)
eq(#reconciled.entries, 2, "indexed ownership survives non-tail interleaving")
eq(reconciled.pending_user_echo, nil)

local queued = queued_input.submit({ entries = direct.entries, pending = true }, "queued", true)
eq(#queued.entries, 2)
eq(queued.queued_entry_idx, 2)
queued.entries[3] = { role = "system", text = "interleaved" }
queued.in_flight = 3
local accepted, changed = queued_input.accept_steered(queued)
eq(changed, true)
eq(#accepted.entries, 2)
eq(accepted.entries[2].text, "interleaved")
eq(accepted.in_flight, 2)

local after_accept, echo_owned = queued_input.reconcile_echo(accepted, "queued")
eq(echo_owned, false, "accepted queue leaves the durable append unclaimed")
eq(#after_accept.entries, 2, "queue acceptance only removes the optimistic owner")

local restored, did_restore = queued_input.restore({
  entries = { { role = "user", text = "lead" }, { role = "user", text = "queued" } },
  queued_entry_idx = 2,
  input_value = "draft",
})
eq(did_restore, true)
eq(#restored.entries, 1)
eq(restored.input_value, "queued draft")

local restored_empty = queued_input.restore({
  entries = { { role = "user", text = "lead" }, { role = "user", text = "queued" } },
  queued_entry_idx = 2,
  input_value = "",
})
eq(restored_empty.input_value, "queued ",
  "restoring into an empty prompt leaves a space ready for continued typing")

print("queued_input_test: all assertions passed")
