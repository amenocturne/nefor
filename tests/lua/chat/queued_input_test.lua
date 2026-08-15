local queued_input = require("libs.chat.queued_input")

local function eq(actual, expected, message)
  if actual ~= expected then
    error((message or "values differ") .. ": expected " .. tostring(expected)
      .. ", got " .. tostring(actual), 2)
  end
end

local initial = { entries = {}, input_value = "", in_flight = nil }
local direct = queued_input.submit(initial, "first", false, "submit-first")
eq(#direct.entries, 1)
eq(direct.pending_submission_ids[1], "submit-first")
eq(direct.pending_user_echo_id, direct.entries[1].local_id)

local observed_cli = queued_input.observe_external_submit(initial, "from cli", "submit-cli")
eq(#observed_cli.entries, 1, "bus submit becomes visible before durable projection")
eq(observed_cli.entries[1].text, "from cli")
eq(observed_cli.pending_submission_ids[1], "submit-cli")
local observed_interactive = queued_input.observe_external_submit(
  direct, "first", "submit-first")
eq(#observed_interactive.entries, 1, "interactive optimistic entry is not duplicated")

eq(queued_input.is_queued_entry({ queued_entry_id = nil }, { role = "assistant" }), false,
  "entries without local identity cannot match an absent queue owner")
eq(queued_input.is_queued_entry({ queued_entry_id = "queued-owner" },
  { local_id = "queued-owner" }), true, "queue ownership requires matching string identities")

local interleaved = {
  entries = { direct.entries[1], { role = "system", text = "result" } },
  pending_submission_ids = direct.pending_submission_ids,
  pending_user_echo_id = direct.pending_user_echo_id,
}
local unrelated, unrelated_match = queued_input.reconcile_echo(
  interleaved, { "submit-other" }, "other-message", "other-turn")
eq(unrelated_match, false, "equal text cannot acknowledge a different submission identity")
eq(unrelated.entries[1].message_id, nil)
local reconciled, matched = queued_input.reconcile_echo(
  interleaved, { "submit-first" }, "message-first", "turn-first")
eq(matched, true)
eq(#reconciled.entries, 2, "identity ownership survives non-tail interleaving")
eq(reconciled.pending_submission_ids, nil)
eq(reconciled.entries[2].message_id, "message-first",
  "canonical acknowledgement moves the owned row to the causal tail")

local queued = queued_input.submit(
  { entries = direct.entries, pending = true }, "queued", true, "submit-queued")
eq(#queued.entries, 2)
local queued_id = queued.queued_entry_id
eq(queued_id, queued.entries[2].local_id)
queued.entries[3] = { role = "system", text = "interleaved" }
queued.in_flight = 3
local accepted, changed = queued_input.accept_steered(queued)
eq(changed, true)
eq(#accepted.entries, 3, "promotion keeps the optimistic row")
eq(accepted.entries[2].text, "interleaved")
eq(accepted.entries[3].text, "queued", "promotion moves the row to the causal tail")
eq(accepted.entries[3].local_id, queued_id, "promotion preserves stable ownership")
eq(accepted.in_flight, 2)
eq(accepted.queued_entry_id, nil)
eq(accepted.pending_user_echo_id, queued_id)

local after_accept, echo_owned = queued_input.reconcile_echo(
  accepted, { "submit-queued" }, "message-queued", "turn-queued")
eq(echo_owned, true, "canonical append acknowledges the promoted optimistic owner")
eq(#after_accept.entries, 3, "acknowledgement must not delete the promoted row")
eq(after_accept.entries[3].text, "queued")
eq(after_accept.entries[3].message_id, "message-queued")

local identified = queued_input.submit(
  { entries = {} }, "identified", false, "submit-identified")
identified = select(1, queued_input.reconcile_echo(
  identified, { "submit-identified" }, "message-identified", "turn-identified"))
eq(identified.entries[1].message_id, "message-identified")
eq(identified.entries[1].turn_id, "turn-identified")

local duplicate = queued_input.submit(
  after_accept, "queued", false, "submit-duplicate")
eq(#duplicate.entries, 4, "identical text creates a distinct optimistic entry")
eq(duplicate.entries[3].local_id == duplicate.entries[4].local_id, false)
local duplicate_state, duplicate_ack = queued_input.reconcile_echo(
  duplicate, { "submit-queued" }, "message-wrong", "turn-wrong")
eq(duplicate_ack, false, "an earlier equal-text submission cannot claim the new owner")
eq(#duplicate_state.entries, 4)

local restore_source = queued_input.submit({
  entries = { { role = "user", text = "lead" }, { role = "system", text = "tail" } },
  input_value = "draft",
}, "queued", true, "submit-restore")
local restored, did_restore = queued_input.restore(restore_source)
eq(did_restore, true)
eq(#restored.entries, 2)
eq(restored.entries[2].text, "tail")
eq(restored.input_value, "queued draft")

local restore_empty_source = queued_input.submit({
  entries = { { role = "user", text = "lead" } }, input_value = "",
}, "queued", true, "submit-empty")
local restored_empty = queued_input.restore(restore_empty_source)
eq(restored_empty.input_value, "queued ",
  "restoring into an empty prompt leaves a space ready for continued typing")

print("queued_input_test: all assertions passed")
