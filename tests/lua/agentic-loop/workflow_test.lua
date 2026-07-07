-- tests/lua/agentic-loop/workflow_test.lua — unit tests for the
-- agentic-loop turn spawner. The Rust harness
-- (`engine/tests/starter_agentic_workflow_test.rs`) installs a stub
-- `nefor.*` surface (json + engine.* + log.* + bus.on_event) so
-- `require("libs.agentic-loop")` succeeds, then loads this file. Tests drive
-- the actor by:
--
--   * calling its public API directly (configure, submit, set_model,
--     cancel_all) — the orchestrator state machine in isolation;
--   * fabricating wire envelopes and feeding them to receive_msg — the
--     mag plugin side of the turn-program contract (mag.loaded /
--     mag.run_started / mag.run_result) is impersonated this way.
--
-- The test surface is `_test.fire_bus`, `_test.calls`,
-- `_test.set_plugins`, `_test.calls_clear`.

local agentic_loop = require("libs.agentic-loop")
local json = nefor.json

-- Seed an active session so the ambient MAG-workspace block can anchor a
-- workspace dir (mirrors a booted session in the live runtime). Set once —
-- the sessions module is separate state the loop's reset() doesn't touch.
require("sessions")._internals.state.current_session_id = "wf-mag-session"

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected), tostring(actual)), 2)
  end
end

-- Build a wire-shaped log entry the actor's receive_msg accepts.
local function make_entry(origin, body)
  return {
    ts      = "2026-05-04T00:00:00.000Z",
    origin  = origin,
    payload = json.encode({ type = "event", from = origin, body = body }),
  }
end

local function send_to_loop(origin, body)
  agentic_loop.receive_msg(make_entry(origin, body))
end

local function decode_calls()
  local out = {}
  for _, c in ipairs(_test.calls()) do
    local ok, decoded = pcall(json.decode, c.payload)
    if ok and type(decoded) == "table" and type(decoded.body) == "table" then
      out[#out + 1] = { body = decoded.body, target = c.target }
    end
  end
  return out
end

local function find_call(calls, kind, role, text_substr)
  for _, c in ipairs(calls) do
    if c.body.kind == kind
       and (role == nil or c.body.role == role)
       and (text_substr == nil
            or (type(c.body.text) == "string"
                and string.find(c.body.text, text_substr, 1, true) ~= nil)) then
      return c
    end
  end
  return nil
end

local function find_kind(calls, kind)
  for _, c in ipairs(calls) do
    if c.body.kind == kind then return c end
  end
  return nil
end

-- ------------------------------------------------------------------
-- configure / chat.model.set — live config plumbing
-- ------------------------------------------------------------------

agentic_loop.configure {
  provider = "ollama",
  model    = "initial-model",
}

do
  assert_eq(agentic_loop.config().provider, "ollama", "configure seeds provider")
  assert_eq(agentic_loop.config().model,    "initial-model", "configure seeds model")
end

-- chat.model.set with a non-empty model updates the live config.model.
do
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "ollama", model = "new-model" })
  assert_eq(agentic_loop.config().model, "new-model", "config.model updated by chat.model.set")
end

-- chat.model.set with an empty model is a no-op (no crash, no update).
do
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "ollama", model = "" })
  assert_eq(agentic_loop.config().model, "new-model", "empty-model set did not clobber config.model")
end

-- chat.model.set with the model field absent is also a no-op.
do
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "ollama" })
  assert_eq(agentic_loop.config().model, "new-model", "missing-model set did not clobber config.model")
end

-- A cross-provider switch just updates the live config: history is
-- canonical in the spawner and reseeded per turn, so no provider-side
-- rebuild happens (and none is needed for continuity).
do
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "qwen-provider", model = "qwen-model" })
  assert_eq(agentic_loop.config().provider, "qwen-provider",
    "chat.model.set with new provider updates config.provider")
  assert_eq(agentic_loop.config().model, "qwen-model",
    "chat.model.set with new model updates config.model")
end

-- ------------------------------------------------------------------
-- session lifecycle (session_end is local-state teardown only)
-- ------------------------------------------------------------------

do
  local replay_window = require("core.history_replay")
  replay_window.install()
  _test.set_plugins({ "ollama", "mag", "nefor-tui" })
  _test.calls_clear()
  _test.fire_bus("sessions.session_end", {})
  for _, c in ipairs(_test.calls()) do
    local ok, decoded = pcall(json.decode, c.payload)
    if ok and type(decoded) == "table" and type(decoded.body) == "table" then
      assert(decoded.body.kind ~= "chat.reset",
        "session_end must NOT broadcast chat.reset — would wipe sibling chat histories on the provider, breaking later /resume")
    end
  end
end

-- Replay-window gating flips on the framing markers.
do
  local replay_window = require("core.history_replay")
  _test.fire_bus("sessions.replay.start", { session_id = "new-id", count = 0 })
  assert_eq(replay_window.active(), true,
    "after replay.start, replay_window is active")
  _test.fire_bus("sessions.replay.end", { session_id = "new-id" })
  assert_eq(replay_window.active(), false,
    "after replay.end, replay_window lifts")
end

-- ------------------------------------------------------------------
-- the turn-program contract
-- ------------------------------------------------------------------

-- The compiled lead-turn.mag shape the mag plugin's `mag.loaded` reply
-- carries (entry adapter → lead llm → sink; the spawner derives its
-- seams — entry actor, llm actor — from this, never hardcodes them).
local function lead_modification()
  return {
    actors = {
      {
        id = "lead.entry", factory = "adapter",
        params = { seed = "provider-in" },
        routes = { ["generic-provider.ProviderOut"] = { "lead.llm" } },
      },
      {
        id = "lead.llm", factory = "llm",
        params = { tools = { "read_file", "mag" } },
        routes = {
          ["generic-tool.ToolCalls"] = { "lead.run-tool" },
          ["generic-provider.FinalAnswer"] = { "sink" },
        },
      },
      { id = "sink", factory = "sink", params = {}, routes = {} },
    },
    messages = {
      { to = "lead.entry", content = { kind = "task", prompt = "<initial task text>" } },
    },
    kills = {},
    rules = {},
  }
end

local function fresh_loop()
  agentic_loop._internals.reset()
  agentic_loop.configure {
    provider = "mock", model = "test-model",
    reasoning_effort = "high", system = "lead system prompt",
    -- Point the lead-program / mag-lib source dir at the real starter tree
    -- so the ambient MAG-context block can read mag/lib/*.
    lead_program = { source_dir = _starter_dir },
  }
  _test.set_plugins({ "mock", "mag", "nefor-tui" })
  _test.calls_clear()
end

-- Submit `text`; drive the load handshake when the program isn't cached
-- yet; return the emitted mag.execute call.
local function begin_turn(text)
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = text })
  local calls = decode_calls()
  local load = find_kind(calls, "mag.load")
  if load ~= nil then
    assert_eq(load.target, "mag", "mag.load targets the mag plugin")
    assert_eq(load.body.entry, "agentic-loop/lead-turn.mag",
      "the shipped turn-program is the load entry")
    _test.calls_clear()
    send_to_loop("mag", {
      kind = "mag.loaded",
      in_reply_to = load.body.id,
      hash = "sha256:test",
      modification = lead_modification(),
    })
    calls = decode_calls()
  end
  local exec = find_kind(calls, "mag.execute")
  assert(exec ~= nil, "submit produces a mag.execute")
  assert_eq(exec.target, "mag", "mag.execute targets the mag plugin")
  return exec
end

-- Convenience: start a turn and bind its run (mag.run_started).
local function begin_bound_turn(text, scope)
  local exec = begin_turn(text)
  _test.calls_clear()
  send_to_loop("mag", {
    kind = "mag.run_started",
    run_id = exec.body.run_id,
    run_name = "lead",
    scope = scope,
  })
  return exec
end

-- (echo) chat.input.submit emits chat.message.append role=user.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first prompt" })
  local calls = decode_calls()
  local user_echo = find_call(calls, "chat.message.append", "user", "first prompt")
  assert(user_echo ~= nil,
    "chat.input.submit must emit chat.message.append role=user")
  assert_eq(user_echo.target, "nefor-tui",
    "user echo must target nefor-tui specifically")
end

-- (turn) a submit clones the program with the user text as the task,
-- overlays config + history onto the derived llm actor, and executes.
do
  fresh_loop()
  local exec = begin_turn("hello lead")
  assert(type(exec.body.run_id) == "string" and #exec.body.run_id > 0,
    "execute carries a minted run_id")
  assert_eq(exec.body.run_name, "lead", "the lead's run is named")
  local mod = exec.body.modification
  assert(type(mod) == "table", "the modification rides inline on the execute")
  assert_eq(mod.messages[1].to, "lead.entry", "task targets the program's entry actor")
  assert_eq(mod.messages[1].content.prompt, "hello lead",
    "the initial mag.Task payload is the user message")
  local overlay = exec.body.params_overlay["lead.llm"]
  assert(type(overlay) == "table", "params overlay keys the derived llm actor")
  -- The base system prompt leads, with the ambient MAG-workspace block
  -- appended (workspace dir + inlined patterns).
  assert(type(overlay.system) == "string"
    and overlay.system:sub(1, #"lead system prompt") == "lead system prompt",
    "the configured system prompt leads the overlay")
  assert(string.find(overlay.system, "## MAG workspace", 1, true) ~= nil,
    "the ambient MAG workspace block is appended to the system overlay")
  assert(string.find(overlay.system, "workspace dir:", 1, true) ~= nil,
    "the block names the session workspace dir")
  assert(string.find(overlay.system, "wf-mag-session", 1, true) ~= nil,
    "the workspace dir is anchored to the active session")
  assert(string.find(overlay.system, "MAG patterns", 1, true) ~= nil,
    "patterns.md is inlined into the block")
  assert_eq(overlay.provider, "mock", "live provider overlays the llm actor")
  assert_eq(overlay.model, "test-model", "live model overlays the llm actor")
  assert_eq(overlay.reasoning_effort, "high", "reasoning effort overlays the llm actor")
  assert(type(overlay.history) == "table" and next(overlay.history) == nil,
    "first turn seeds empty history")
  -- The runtime-state envelope for the statusline fired.
  local calls = decode_calls()
  assert(find_kind(calls, "agentic_loop.run_start") ~= nil,
    "run_start runtime state emitted")

  -- The run began: the spawner binds the transcript to the run's scoped
  -- chat prefix (scope token + llm actor id).
  send_to_loop("mag", {
    kind = "mag.run_started", run_id = exec.body.run_id,
    run_name = "lead", scope = "r7",
  })
  calls = decode_calls()
  local bound = find_kind(calls, "chat.lead.bound")
  assert(bound ~= nil, "run start binds the lead transcript")
  assert_eq(bound.body.chat_prefix, "r7/lead.llm@",
    "the binding is prefix-form: scope + llm actor id")

  -- A lead-scoped gated tool invocation surfaces as transcript tool
  -- events (the gate keeps the kernel's scope-prefixed correlation id).
  _test.calls_clear()
  send_to_loop("tool-gate", {
    kind = "tool-gate.tool.invoke", id = "r7/cap-1",
    name = "read_file", args = { path = "README.md" },
  })
  calls = decode_calls()
  local tool_start = find_kind(calls, "chat.tool.start")
  assert(tool_start ~= nil, "lead-scoped gate invoke emits chat.tool.start")
  assert_eq(tool_start.body.id, "r7/cap-1", "tool start keyed by the correlation id")
  assert_eq(tool_start.body.name, "read_file", "tool start names the tool")

  _test.calls_clear()
  send_to_loop("tool-gate", { kind = "tool.result", id = "r7/cap-1", output = "# nefor" })
  calls = decode_calls()
  local tool_end = find_kind(calls, "chat.tool.end")
  assert(tool_end ~= nil, "lead-scoped tool.result emits chat.tool.end")
  assert_eq(tool_end.body.id, "r7/cap-1", "tool end keyed by the correlation id")
  assert_eq(tool_end.body.error, false, "successful tool result is not an error")

  -- A foreign-scoped invoke (a dispatched sub-run) stays out of the
  -- lead transcript.
  _test.calls_clear()
  send_to_loop("tool-gate", {
    kind = "tool-gate.tool.invoke", id = "r9/cap-1", name = "bash", args = {},
  })
  assert_eq(find_kind(decode_calls(), "chat.tool.start"), nil,
    "foreign-scope gate invoke must not emit lead tool events")

  -- The answer streams under the bound prefix…
  send_to_loop("mock", { kind = "chat.stream.delta", chat_id = "r7/lead.llm@r2", text = "the answer" })
  send_to_loop("mock", { kind = "chat.stream.end",   chat_id = "r7/lead.llm@r2", text = "the answer" })

  -- …so the terminal close records history but does NOT re-append the
  -- already-streamed answer.
  _test.calls_clear()
  send_to_loop("mag", {
    kind = "mag.run_result", run_id = exec.body.run_id,
    status = "completed", result = { text = "the answer" },
  })
  calls = decode_calls()
  assert_eq(find_call(calls, "chat.message.append", "assistant"), nil,
    "streamed answer must not double-render on run close")
  local recorded = find_kind(calls, "agentic_loop.turn_recorded")
  assert(recorded ~= nil, "completed turn emits its turn_recorded marker")
  assert_eq(recorded.body.user, "hello lead", "marker carries the user message")
  assert_eq(recorded.body.answer, "the answer", "marker carries the answer text")
  assert(find_kind(calls, "agentic_loop.idle") ~= nil,
    "completed turn with empty queues goes idle")

  local history = agentic_loop.history()
  assert_eq(#history, 2, "one completed turn appends one {user, answer} pair")
  assert_eq(history[1].role, "user")
  assert_eq(history[1].content, "hello lead")
  assert_eq(history[2].role, "assistant")
  assert_eq(history[2].content, "the answer")

  -- Turn 2 seeds the canonical history (program already cached: no
  -- second mag.load).
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "and more?" })
  calls = decode_calls()
  assert_eq(find_kind(calls, "mag.load"), nil,
    "the cached program is not re-loaded per turn")
  local exec2 = find_kind(calls, "mag.execute")
  assert(exec2 ~= nil, "second turn executes")
  assert_eq(exec2.body.modification.messages[1].content.prompt, "and more?")
  local seeded = exec2.body.params_overlay["lead.llm"].history
  assert_eq(#seeded, 2, "second turn's llm seeds the prior turn's pair")
  assert_eq(seeded[1].content, "hello lead")
  assert_eq(seeded[2].content, "the answer")
end

-- (ambient MAG context caching) the static section (inventory + patterns +
-- types + template signatures + prompt roster) is read once and reused
-- across turns; only the per-session workspace dir varies.
do
  fresh_loop()
  local exec1 = begin_turn("first ambient")
  assert(string.find(exec1.body.params_overlay["lead.llm"].system,
    "## MAG workspace", 1, true) ~= nil, "turn 1 carries the MAG workspace block")
  send_to_loop("mag", {
    kind = "mag.run_result", run_id = exec1.body.run_id,
    status = "completed", result = { text = "a1" },
  })
  local exec2 = begin_turn("second ambient")
  assert(string.find(exec2.body.params_overlay["lead.llm"].system,
    "## MAG workspace", 1, true) ~= nil, "turn 2 also carries the block")
  assert_eq(agentic_loop._internals.state.mag_context.static_builds, 1,
    "the static MAG section is read from disk once and cached across turns")
end

-- (no-stream fallback) a completed turn whose answer never streamed
-- appends the text so the transcript is never silently empty.
do
  fresh_loop()
  local exec = begin_bound_turn("quiet one", "r3")
  send_to_loop("mag", {
    kind = "mag.run_result", run_id = exec.body.run_id,
    status = "completed", result = { text = "quiet answer" },
  })
  local calls = decode_calls()
  local appended = find_call(calls, "chat.message.append", "assistant", "quiet answer")
  assert(appended ~= nil,
    "non-streamed answer must append to the transcript on run close")
  assert_eq(appended.target, "nefor-tui", "answer append targets the TUI")
end

-- (failure surfaces + preserves context) a failed run puts the error in chat
-- — no silent nothing — AND records the turn with a placeholder answer so the
-- user's message survives into the next turn's seed (context never vanishes).
do
  fresh_loop()
  local exec = begin_bound_turn("doomed", "r4")
  send_to_loop("mag", {
    kind = "mag.run_result", run_id = exec.body.run_id,
    status = "failed", error = "provider exploded",
  })
  local calls = decode_calls()
  local err_line = find_call(calls, "chat.message.append", "system", "provider exploded")
  assert(err_line ~= nil, "failed turn surfaces the error in chat")
  local history = agentic_loop.history()
  assert_eq(#history, 2, "failed turn records {user, placeholder} so context survives")
  assert_eq(history[1].content, "doomed", "failed turn preserves the user message")
  assert_eq(history[2].content, "[turn failed: provider exploded]",
    "a non-interrupt failure records its error as the placeholder answer")
  local recorded = find_kind(calls, "agentic_loop.turn_recorded")
  assert(recorded ~= nil, "failed turn emits its turn_recorded marker for /resume")
  -- The loop is free again.
  local exec2 = begin_turn("retry")
  assert(exec2 ~= nil, "a failed turn releases the single-flight slot")
end

-- (interrupt preserves context) an interrupted lead turn settles failed with
-- an "interrupted by user" error; it records the honest interrupt placeholder
-- (not the raw error string) so the next turn sees the message was interrupted.
do
  fresh_loop()
  local exec = begin_bound_turn("I'm testing interrupts", "r4b")
  send_to_loop("mag", {
    kind = "mag.run_result", run_id = exec.body.run_id,
    status = "failed", error = "interrupted by user",
  })
  local history = agentic_loop.history()
  assert_eq(#history, 2, "interrupted turn records {user, placeholder}")
  assert_eq(history[1].content, "I'm testing interrupts",
    "interrupted turn preserves the user message — no amnesia")
  assert_eq(history[2].content, "[interrupted by user]",
    "an interrupt-origin failure records the interrupt marker, not the raw error")
end

-- (single-Esc kill preserves context) Esc kills the active run via the kernel
-- kill machinery; the killed terminal reply aborts the turn but STILL records
-- the user's message with an interrupt placeholder — a killed turn must not
-- seed the next turn blind (the amnesia the user hit on a real interrupt).
do
  fresh_loop()
  local exec = begin_bound_turn("kill me", "r5")
  send_to_loop("nefor-tui", { kind = "chat.interrupt" })
  local calls = decode_calls()
  local kill = find_kind(calls, "mag.kill_run")
  assert(kill ~= nil, "chat.interrupt emits mag.kill_run for the active run")
  assert_eq(kill.target, "mag", "kill targets the mag plugin")
  assert_eq(kill.body.run_id, exec.body.run_id, "kill names the active run")

  _test.calls_clear()
  send_to_loop("mag", {
    kind = "mag.run_result", run_id = exec.body.run_id, status = "killed",
  })
  calls = decode_calls()
  local history = agentic_loop.history()
  assert_eq(#history, 2, "killed turn records {user, placeholder} so context survives")
  assert_eq(history[1].content, "kill me", "killed turn preserves the user message")
  assert_eq(history[2].content, "[interrupted by user]",
    "killed turn records the interrupt placeholder as the answer")
  assert(find_kind(calls, "agentic_loop.turn_recorded") ~= nil,
    "killed turn emits its turn_recorded marker for /resume")
  -- A killed turn stays quiet in the transcript (the interrupt notice already
  -- rode cancel_all) — only the history store is fed.
  assert_eq(find_call(calls, "chat.message.append", "assistant"), nil,
    "killed turn appends no assistant line to the transcript")
  local idle = find_kind(calls, "agentic_loop.runtime_state")
  assert(idle ~= nil and idle.body.state == "idle" and idle.body.reason == "cancelled",
    "killed turn settles the statusline as cancelled")
  -- The loop is free again, and the next turn seeds the preserved context.
  local exec_after = begin_turn("after kill")
  assert(exec_after ~= nil, "a killed turn releases the slot")
  assert_eq(#exec_after.body.params_overlay["lead.llm"].history, 2,
    "the turn after a kill seeds the preserved {user, placeholder} pair")
end

-- (queued promotion) messages submitted while busy queue, then promote
-- into the next turn once the current one closes.
do
  fresh_loop()
  local exec = begin_bound_turn("first", "r6")
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "second" })
  local calls = decode_calls()
  assert(find_call(calls, "chat.message.append", "user", "second") ~= nil,
    "busy submit still echoes the user line")
  assert_eq(find_kind(calls, "mag.execute"), nil,
    "busy submit must not double-dispatch")

  _test.calls_clear()
  send_to_loop("mag", {
    kind = "mag.run_result", run_id = exec.body.run_id,
    status = "completed", result = { text = "first answer" },
  })
  calls = decode_calls()
  local exec2 = find_kind(calls, "mag.execute")
  assert(exec2 ~= nil, "queued input promotes into a fresh turn on close")
  assert_eq(exec2.body.modification.messages[1].content.prompt, "second",
    "the promoted turn carries the queued text")
  local seeded = exec2.body.params_overlay["lead.llm"].history
  assert_eq(#seeded, 2, "the promoted turn seeds the finished turn's history")
end

-- (interrupt_all = graceful) double-Esc GRACEFULLY interrupts the run (not a
-- kill) and drops the queued inputs. The run SURVIVES and winds down to a
-- completed turn that records its own history — the amnesia is structurally
-- gone, because there is no killed-without-record turn on this path.
do
  fresh_loop()
  local exec = begin_bound_turn("run a long bash", "r8")
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "queued" })
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.interrupt_all" })
  local calls = decode_calls()

  -- graceful interrupt, NOT a kill
  local interrupt = find_kind(calls, "mag.interrupt_run")
  assert(interrupt ~= nil, "interrupt_all emits mag.interrupt_run for the active run")
  assert_eq(interrupt.target, "mag", "interrupt targets the mag plugin")
  assert_eq(interrupt.body.run_id, exec.body.run_id, "interrupt names the active run")
  -- The lead's OWN turn is interrupted GRACEFULLY (no terminate flag): the run
  -- survives, re-fires, and records its history. Contrast a DISPATCHED sub-run,
  -- which lead-workflow terminates (terminate = true).
  assert(not interrupt.body.terminate,
    "the lead's own turn is interrupted gracefully, never terminated")
  assert_eq(find_kind(calls, "mag.kill_run"), nil,
    "double-Esc no longer kills the run")

  -- a transcript notice is rendered
  local notice = find_call(calls, "chat.message.append", "system", "interrupted by user")
  assert(notice ~= nil, "double-Esc renders an interrupt notice in the transcript")

  -- the run is still active — a new submit queues rather than dispatching.
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "post" })
  calls = decode_calls()
  assert_eq(find_kind(calls, "mag.execute"), nil,
    "the interrupted run is still active — a fresh submit queues, not dispatches")

  -- the interrupted turn winds down COMPLETED (the lead re-fired with the
  -- interrupted tool result and produced a final answer): history records
  -- itself and turn_recorded fires. This is the amnesia fix.
  _test.calls_clear()
  send_to_loop("mag", {
    kind = "mag.run_result", run_id = exec.body.run_id,
    status = "completed", result = { text = "stopped as you asked" },
  })
  calls = decode_calls()
  local recorded = find_kind(calls, "agentic_loop.turn_recorded")
  assert(recorded ~= nil, "the interrupted-but-completed turn records its history marker")
  assert_eq(recorded.body.user, "run a long bash",
    "the marker carries the ORIGINAL user message (the interrupted turn IS in history)")
  assert_eq(#agentic_loop.history(), 2,
    "history gains the {user, answer} pair — the interrupted turn is remembered")
end

-- (relay) a dispatched run's completion relays as a fresh turn through
-- the deferred queue (lead-workflow drives relay_run_completion).
do
  fresh_loop()
  -- Prime the program cache with a full turn.
  local exec = begin_bound_turn("prime", "r10")
  send_to_loop("mag", {
    kind = "mag.run_result", run_id = exec.body.run_id,
    status = "completed", result = { text = "primed" },
  })
  _test.calls_clear()
  agentic_loop.relay_run_completion({
    run_id = "mag-sub-1", status = "success", output = "sub answer",
  })
  local calls = decode_calls()
  local exec2 = find_kind(calls, "mag.execute")
  assert(exec2 ~= nil, "an idle lead relays the completion immediately")
  local prompt = exec2.body.modification.messages[1].content.prompt
  assert(string.find(prompt, "mag-sub-1", 1, true) ~= nil,
    "the relay turn names the finished run")
  assert(string.find(prompt, "sub answer", 1, true) ~= nil,
    "the relay turn carries the run output")
end

-- (relay of interruption) an INTERRUPTED dispatched run settles failed
-- "interrupted by user"; the relay must carry that into the lead's next turn
-- so a double-Esc cancellation is never a silent disappearance.
do
  fresh_loop()
  local exec = begin_bound_turn("prime", "r11")
  send_to_loop("mag", {
    kind = "mag.run_result", run_id = exec.body.run_id,
    status = "completed", result = { text = "primed" },
  })
  _test.calls_clear()
  agentic_loop.relay_run_completion({
    run_id = "mag-sub-int", status = "failed", error = "interrupted by user",
  })
  local calls = decode_calls()
  local exec2 = find_kind(calls, "mag.execute")
  assert(exec2 ~= nil, "an idle lead relays the interrupted failure immediately")
  local prompt = exec2.body.modification.messages[1].content.prompt
  assert(string.find(prompt, "FAILED", 1, true) ~= nil,
    "the relay turn marks the interrupted run as FAILED")
  assert(string.find(prompt, "interrupted by user", 1, true) ~= nil,
    "the relay turn carries the interruption reason")
end

-- (replay gating + history rebuild) replayed input envelopes must not
-- re-orchestrate; replayed turn_recorded markers rebuild the canonical
-- history the next live turn seeds.
do
  fresh_loop()
  local replay_window = require("core.history_replay")
  _test.fire_bus("sessions.replay.start", { session_id = "resume-1", count = 2 })
  assert_eq(replay_window.active(), true, "replay window open")

  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "replayed input" })
  assert_eq(find_kind(decode_calls(), "mag.execute"), nil,
    "a replayed chat.input.submit must not spawn a turn")
  assert_eq(find_kind(decode_calls(), "mag.load"), nil,
    "a replayed chat.input.submit must not trigger a program load")

  send_to_loop("engine", {
    kind = "agentic_loop.turn_recorded",
    user = "old question", answer = "old answer",
  })
  _test.fire_bus("sessions.replay.end", { session_id = "resume-1" })

  local history = agentic_loop.history()
  assert_eq(#history, 2, "replayed marker rebuilt the canonical history")
  assert_eq(history[1].content, "old question")
  assert_eq(history[2].content, "old answer")

  local exec = begin_turn("post-resume")
  local seeded = exec.body.params_overlay["lead.llm"].history
  assert_eq(#seeded, 2, "the post-resume turn seeds the rebuilt history")
  assert_eq(seeded[1].content, "old question")
end

-- (/new) chat.reset clears queue + history so the next turn is fresh.
do
  fresh_loop()
  local exec = begin_bound_turn("hi", "r11")
  send_to_loop("mag", {
    kind = "mag.run_result", run_id = exec.body.run_id,
    status = "completed", result = { text = "yo" },
  })
  assert_eq(#agentic_loop.history(), 2, "turn recorded before reset")
  send_to_loop("nefor-tui", { kind = "chat.reset" })
  assert_eq(#agentic_loop.history(), 0, "chat.reset clears canonical history")
  local exec2 = begin_turn("fresh start")
  local seeded = exec2.body.params_overlay["lead.llm"].history
  assert(next(seeded) == nil, "post-/new turn seeds empty history")
end

-- (stream visibility) the active turn's prefix-scoped chats are
-- stream-visible (the provider compositor fires the public observers off
-- this), everything else keeps the tracked-table semantics.
do
  fresh_loop()
  begin_bound_turn("streamy", "r12")
  assert_eq(agentic_loop.stream_visible("r12/lead.llm@r1"), true,
    "the bound turn's scoped chats are stream-visible")
  assert_eq(agentic_loop.stream_visible("r99/other.llm@r1"), false,
    "foreign-scope chats are not stream-visible")
end

-- (load failure) a compile error in the turn-program surfaces in chat
-- and the next submit retries the load.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "hello" })
  local load = find_kind(decode_calls(), "mag.load")
  assert(load ~= nil, "first submit triggers the program load")
  _test.calls_clear()
  send_to_loop("mag", {
    kind = "mag.error", in_reply_to = load.body.id, message = "parse error at line 3",
  })
  local calls = decode_calls()
  assert(find_call(calls, "chat.message.append", "system", "parse error") ~= nil,
    "a turn-program compile failure surfaces in chat")
  -- Retry path: the next submit re-kicks the load.
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "again" })
  assert(find_kind(decode_calls(), "mag.load") ~= nil,
    "the next submit retries the program load")
end
