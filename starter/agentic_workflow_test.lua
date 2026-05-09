-- starter/agentic_workflow_test.lua — unit tests for the agentic-loop
-- actor. The Rust harness (`crates/nefor/tests/starter_agentic_workflow_test.rs`)
-- installs a stub `nefor.*` surface (json + engine.* + log.* + bus.on_event)
-- so `require("agentic-loop")` succeeds, then loads this file. Tests
-- drive the actor's behaviour by:
--
--   * calling its public API directly (configure, submit, set_model,
--     cancel_all, build_template) — these short-circuit the bus and
--     test the orchestrator state machine in isolation;
--   * fabricating wire envelopes and feeding them to receive_msg
--     (replacement for the prior for_chat / for_reasoner_graph
--     factory tests).
--
-- The test surface is the same as before — `_test.fire_bus`,
-- `_test.calls`, `_test.set_plugins`, `_test.calls_clear` — so the
-- Rust harness needs no modifications.

local agentic_loop = require("agentic-loop")
local json = nefor.json

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected), tostring(actual)), 2)
  end
end

-- Build a wire-shaped log entry the actor's receive_msg accepts.
-- Mirrors the engine's broker output: { ts, origin, payload } where
-- payload is JSON-encoded { type, body }.
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

-- Configure with a known model so we can detect a runtime switch.
agentic_loop.configure {
  provider = "ollama",
  model    = "initial-model",
}

-- Sanity: configure() seeds the live config; build_template does NOT bake
-- provider/model into wrap_args (the picker is the source of truth — every
-- reasoner firing falls through to cfg.provider / cfg.model live).
do
  assert_eq(agentic_loop.config().provider, "ollama", "configure seeds provider")
  assert_eq(agentic_loop.config().model,    "initial-model", "configure seeds model")
  local g = agentic_loop.build_template("hi")
  local saw = false
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      saw = true
      assert_eq(n.args.provider, nil,
        "wrap node must NOT bake provider into args (picker is source of truth)")
      assert_eq(n.args.model, nil,
        "wrap node must NOT bake model into args (picker is source of truth)")
    end
  end
  assert(saw, "wrap node found in template")
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

-- A second switch updates config.model again.
do
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "ollama", model = "another-model" })
  assert_eq(agentic_loop.config().model, "another-model", "second switch sticks on live config")
end

-- Bug B1 regression — picker is the source of truth for ALL reasoner
-- firings. The orchestrator graph builder must NOT bake provider/model
-- into wrap_args, so wrap and sub-graph responder nodes alike fall
-- through to the live cfg in `provider_run_node`. Per-node routing is
-- still opt-in via explicit args.provider / args.model.
do
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "qwen-provider", model = "qwen-model" })
  assert_eq(agentic_loop.config().provider, "qwen-provider",
    "chat.model.set with new provider updates config.provider")
  assert_eq(agentic_loop.config().model, "qwen-model",
    "chat.model.set with new model updates config.model")
  local g = agentic_loop.build_template("hi")
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      assert_eq(n.args.provider, nil,
        "after picker switch, wrap_args.provider must remain nil — picker drives via cfg")
      assert_eq(n.args.model, nil,
        "after picker switch, wrap_args.model must remain nil — picker drives via cfg")
    end
  end
end

-- ------------------------------------------------------------------
-- tool_allowlist plumbing: configure -> state.config -> wrap_args.
--
-- Pre-fix the lead's chat saw the entire wire-advertised tool catalog
-- including reasoner-graph internals like `spawn_graph`, so the model
-- could call `spawn_graph` directly and bottom out in `reasoner '<role>'
-- not connected`. The fix wires lead_role.ORCHESTRATION_TOOLS through
-- agentic_loop.configure { tool_allowlist = ... } → state.config →
-- build_orchestrator_graph → wrap_args.tool_allowlist → provider-wrapper
-- emits as chat.create.tools. These tests pin the Lua-side plumbing.
-- The Rust openai-provider has its own per-turn-filtering test
-- (chat_create_tools_string_array_filters_per_turn_tools_array).
-- ------------------------------------------------------------------
do
  -- Orchestrator wrap_args.tool_allowlist absent by default (no filter).
  agentic_loop._internals.reset()
  agentic_loop.configure { provider = "ollama", model = "m" }
  local g = agentic_loop.build_template("hi")
  for _, n in ipairs(g.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      assert_eq(n.args.tool_allowlist, nil,
        "without configure { tool_allowlist }, wrap_args.tool_allowlist stays nil (no filter)")
    end
  end

  -- Configure with an allowlist; wrap_args.tool_allowlist now carries it.
  local allowlist = { "read_file", "dispatch-graph", "write-review", "await-approval" }
  agentic_loop.configure { tool_allowlist = allowlist }
  assert(type(agentic_loop.config().tool_allowlist) == "table",
    "configure { tool_allowlist } stores the list on state.config")
  assert_eq(#agentic_loop.config().tool_allowlist, #allowlist,
    "configure-stored allowlist preserves length")

  local g2 = agentic_loop.build_template("hi")
  local saw_wrap = false
  for _, n in ipairs(g2.nodes or {}) do
    if n.id == "wrap" and type(n.args) == "table" then
      saw_wrap = true
      assert(type(n.args.tool_allowlist) == "table",
        "configure-set tool_allowlist surfaces on wrap_args")
      -- Spot-check contents: every input name landed.
      local seen = {}
      for _, name in ipairs(n.args.tool_allowlist) do seen[name] = true end
      for _, expected in ipairs(allowlist) do
        assert(seen[expected],
          "wrap_args.tool_allowlist must contain `" .. expected .. "`")
      end
      -- Most important: spawn_graph (a reasoner-graph internal not in
      -- ORCHESTRATION_TOOLS) is NOT on the lead's allowlist. Without
      -- this filter the lead can call spawn_graph directly and produces
      -- the runtime error this fix is designed to prevent.
      assert(seen["spawn_graph"] == nil,
        "wrap_args.tool_allowlist must NOT include `spawn_graph` — that's a reasoner-graph internal the lead reaches via dispatch-graph, not directly")
    end
  end
  assert(saw_wrap, "wrap node found in template")
end

-- ------------------------------------------------------------------
-- session lifecycle (session_end is local-state teardown only)
-- ------------------------------------------------------------------

-- sessions.session_end (delivered via the bus) is internal teardown
-- bookkeeping for the agentic-loop actor: clears current_state, the
-- pending-input queue, and any in-flight run id. It does NOT broadcast
-- chat.reset — chat.reset translates to <provider>.reset on the wire,
-- which providers handle as reset_all() (every chat history wiped, not
-- just the active one). With reset_all in this path, any later
-- /resume of a chat under the same provider lands on a chat_id whose
-- history the provider no longer holds — model replies with no
-- context. See commit 5042a06 for the full rationale. The test below
-- pins the new contract: session_end produces no chat.reset egress.
do
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })
  _test.calls_clear()

  _test.fire_bus("sessions.session_end", { session_id = "old-id" })

  for _, c in ipairs(_test.calls()) do
    local ok, decoded = pcall(json.decode, c.payload)
    if ok and type(decoded) == "table" and type(decoded.body) == "table" then
      assert(decoded.body.kind ~= "chat.reset",
        "session_end must NOT broadcast chat.reset — would wipe sibling chat histories on the provider, breaking later /resume")
    end
  end
end

-- Replay-window gating now lives in `lib/replay_window`, driven by
-- `sessions.replay.start` / `sessions.replay.end` framing markers.
-- agentic-loop short-circuits inside `receive_msg` based on the
-- module's `active()` getter; the test asserts the flip end-to-end by
-- firing the markers and probing the gate.
do
  local replay_window = require("lib.replay_window")
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })

  _test.fire_bus("sessions.replay.start", { session_id = "new-id", count = 0 })
  assert_eq(replay_window.active(), true,
    "after replay.start, replay_window is active")

  _test.fire_bus("sessions.replay.end", { session_id = "new-id" })
  assert_eq(replay_window.active(), false,
    "after replay.end, replay_window lifts")
end

-- ------------------------------------------------------------------
-- Issue 1 + Issue 3 — user message echo + busy-submit queue
-- ------------------------------------------------------------------

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

local function fresh_loop()
  agentic_loop._internals.reset()
  agentic_loop.configure { provider = "ollama", model = "test-model" }
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })
  _test.calls_clear()
end

-- (1.A) chat.input.submit emits chat.message.append role=user.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first prompt" })
  local calls = decode_calls()
  local user_echo = find_call(calls, "chat.message.append", "user", "first prompt")
  assert(user_echo ~= nil,
    "chat.input.submit must emit chat.message.append role=user (Issue 1 echo)")
  assert_eq(user_echo.target, "nefor-tui",
    "user echo must target nefor-tui specifically")
end

-- (3.A) Busy submit is queued, not rejected.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first" })
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "second" })
  local calls = decode_calls()
  local echo = find_call(calls, "chat.message.append", "user", "second")
  assert(echo ~= nil,
    "queued submit must still echo a chat.message.append role=user")
  local queued_note = find_call(calls, "chat.message.append", "system", "queued")
  assert(queued_note ~= nil,
    "queued submit must surface a [queued ...] system message; got " ..
    json.encode(calls))
  local busy_msg = find_call(calls, "chat.message.append", "system", "orchestrator busy")
  assert_eq(busy_msg, nil,
    "the rejected '[orchestrator busy ...]' must no longer appear")
end

-- (3.B) Two messages submitted back-to-back BOTH dispatch as the
-- orchestrator frees up. We drive the canonical run-close
-- `tool.result { id=run_id, result: { status, results } }` to flush.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "alpha" })
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "beta" })

  -- Submission rides on the canonical tool contract:
  -- `tool.invoke { id=run_id, name="spawn_graph", args: { graph, on_node_failure } }`.
  local function find_run_emit(calls)
    for _, c in ipairs(calls) do
      if c.body.kind == "tool.invoke" and c.body.name == "spawn_graph" then
        return c.body.id
      end
    end
    return nil
  end
  local first_run = find_run_emit(decode_calls())
  assert(first_run ~= nil, "first submit dispatched a spawn_graph tool.invoke")
  do
    local count = 0
    for _, c in ipairs(decode_calls()) do
      if c.body.kind == "tool.invoke" and c.body.name == "spawn_graph" then
        count = count + 1
      end
    end
    assert_eq(count, 1, "second submit must NOT dispatch while first is in flight")
  end
  _test.calls_clear()
  -- Drive the canonical run-close for the first run.
  send_to_loop("reasoner-graph", {
    kind   = "tool.result",
    id     = first_run,
    result = { status = "success", results = {} },
  })
  local second_run = find_run_emit(decode_calls())
  assert(second_run ~= nil,
    "queued submit must dispatch on tool.result run-close; sends were " ..
    json.encode(_test.calls()))
  assert(second_run ~= first_run,
    "second run must be a fresh run_id, got " .. tostring(second_run))
end

-- (3.C) chat.reset clears the pending-input queue.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first" })
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "queued-2" })
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "queued-3" })
  send_to_loop("nefor-tui", { kind = "chat.reset" })
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "fresh" })
  local function find_run_user_text(calls)
    for _, c in ipairs(calls) do
      if c.body.kind == "tool.invoke"
          and c.body.name == "spawn_graph"
          and type(c.body.args) == "table"
          and type(c.body.args.graph) == "table" then
        for _, node in ipairs(c.body.args.graph.nodes or {}) do
          if node.id == "wrap" and type(node.args) == "table" then
            return node.args.prompt
          end
        end
      end
    end
    return nil
  end
  local user_text = find_run_user_text(decode_calls())
  assert_eq(user_text, "fresh",
    "after chat.reset, queue must be empty: dispatched user_text was '"
    .. tostring(user_text) .. "', expected 'fresh'")
end

-- (3.D) session_end teardown clears the queue.
do
  fresh_loop()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first" })
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "stranded" })
  _test.fire_bus("sessions.session_end", { session_id = "old" })
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "post-swap" })
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "tool.invoke"
        and c.body.name == "spawn_graph"
        and type(c.body.args) == "table"
        and type(c.body.args.graph) == "table" then
      for _, node in ipairs(c.body.args.graph.nodes or {}) do
        if node.id == "wrap" and type(node.args) == "table" then
          local txt = node.args.prompt
          assert(txt ~= "stranded",
            "session_end must clear pending_user_inputs; saw stranded dispatch")
        end
      end
    end
  end
end

-- ------------------------------------------------------------------
-- Issue 2 — cancel_all no longer emits the [interrupted: chat=...] line
-- ------------------------------------------------------------------
do
  agentic_loop._internals.reset()
  agentic_loop.configure { provider = "ollama", model = "test-model" }
  _test.set_plugins({ "ollama", "reasoner-graph", "nefor-tui" })
  _test.calls_clear()
  agentic_loop.cancel_all()
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "chat.message.append" and type(c.body.text) == "string" then
      assert(string.find(c.body.text, "interrupted: chat=", 1, true) == nil,
        "cancel_all must not emit '[interrupted: chat=...]' to the user; saw "
        .. c.body.text)
      assert(string.find(c.body.text, "sub-graphs=", 1, true) == nil,
        "cancel_all must not emit 'sub-graphs=' counter; saw " .. c.body.text)
    end
  end
end

-- ------------------------------------------------------------------
-- Bug 3 regression — chat.input.submit during replay must not
-- re-spawn an orchestrator graph. Sessions replays the user's
-- original submit envelope when a session resumes; agentic-loop
-- already saw the answer in the prior run, so re-firing the
-- handler would spawn a fresh graph and re-invoke the model on
-- exactly the same prompt. State is rebuilt by pure-Lua actors
-- watching the bus markers; replayed wire envelopes are observation
-- only, not new orchestration triggers.
-- ------------------------------------------------------------------
do
  fresh_loop()
  _test.fire_bus("sessions.replay.start", { session_id = "resumed", count = 0 })
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first prompt" })
  for _, c in ipairs(decode_calls()) do
    assert(not (c.body.kind == "tool.invoke" and c.body.name == "spawn_graph"),
      "chat.input.submit during replay must NOT dispatch a fresh spawn_graph; got "
      .. json.encode(c.body))
    assert(not (c.body.kind == "chat.message.append" and c.body.role == "user"),
      "chat.input.submit during replay must NOT echo a user message; got "
      .. json.encode(c.body))
  end
  _test.fire_bus("sessions.replay.end", { session_id = "resumed" })

  -- After replay ends, the actor returns to live behaviour: a fresh
  -- submit dispatches normally. Locks in that the gate is window-
  -- scoped, not a hard mute.
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "post-replay" })
  local saw_dispatch = false
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "tool.invoke" and c.body.name == "spawn_graph" then
      saw_dispatch = true
    end
  end
  assert(saw_dispatch,
    "after replay.end, live chat.input.submit must dispatch a spawn_graph again")
end

-- chat.reset / chat.interrupt_all / chat.model.set during replay are
-- also gated. Same reasoning: they would mutate state that's already
-- being rebuilt from the recorded log.
do
  fresh_loop()
  _test.fire_bus("sessions.replay.start", { session_id = "resumed", count = 0 })
  send_to_loop("nefor-tui", { kind = "chat.reset" })
  send_to_loop("nefor-tui", { kind = "chat.interrupt_all" })
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "ollama", model = "should-not-stick" })
  _test.fire_bus("sessions.replay.end", { session_id = "resumed" })

  -- model.set was gated, so config.model must not have changed to the
  -- replay-injected value.
  assert(agentic_loop.config().model ~= "should-not-stick",
    "chat.model.set during replay must not mutate config.model; saw "
    .. tostring(agentic_loop.config().model))
end

-- ------------------------------------------------------------------
-- Bug 4 regression — sub-graph completion emits the literal terminal
-- output as a chat.message.append so the user can see what the
-- sub-graph produced. The deferred relay text (verbose, model-facing)
-- still rides as the next orchestrator-turn user message; the visible
-- emit lands in the transcript before that.
-- ------------------------------------------------------------------
do
  fresh_loop()
  -- Drive the sub-graph dispatch path the way tool-gate's wrapper
  -- does: queue_sub_graph + tool.result keyed by the minted run_id.
  local terminal_text = "octopuses are eight-armed cephalopods; lighthouses are coastal sentinels."
  local run_id = agentic_loop.queue_sub_graph(
    { graph = { nodes = { { id = "terminal", reasoner = "terminal", args = {} } }, edges = {} } },
    "gate-inner-1"
  )
  assert(type(run_id) == "string", "queue_sub_graph must return a run_id")
  _test.calls_clear()

  -- Drive the canonical sub-graph completion: a tool.result with id
  -- == run_id and result.status == success carrying the terminal node
  -- output table.
  send_to_loop("reasoner-graph", {
    kind   = "tool.result",
    id     = run_id,
    result = {
      status  = "success",
      results = { terminal = { output = { text = terminal_text } } },
    },
  })

  local calls = decode_calls()
  local visible = find_call(calls, "chat.message.append", "system", "octopuses are eight-armed")
  assert(visible ~= nil,
    "sub-graph completion must emit a chat.message.append carrying the literal terminal output; got "
    .. json.encode(_test.calls()))
  assert_eq(visible.target, "nefor-tui",
    "visible sub-graph output must target nefor-tui")
end

-- Sub-graph failure surfaces an [spawn_graph errored] visible message.
do
  fresh_loop()
  local run_id = agentic_loop.queue_sub_graph(
    { graph = { nodes = { { id = "terminal", reasoner = "terminal", args = {} } }, edges = {} } },
    "gate-inner-2"
  )
  _test.calls_clear()
  send_to_loop("reasoner-graph", {
    kind   = "tool.result",
    id     = run_id,
    result = { status = "error", results = { terminal = { error = "boom" } } },
  })
  local calls = decode_calls()
  local err_msg = find_call(calls, "chat.message.append", "system", "spawn_graph errored")
  assert(err_msg ~= nil,
    "sub-graph failure must emit a [spawn_graph errored] system message; got "
    .. json.encode(_test.calls()))
end

-- ------------------------------------------------------------------
-- Bug 6 regression — cancel_all interrupts the in-flight provider
-- stream, not just the orchestrator graph. Without this, /new
-- (chat.interrupt_all) lets the prior turn's `<provider>.stream.
-- delta` envelopes keep arriving and paint into the freshly-cleared
-- transcript. graph.cancel on the reasoner-graph side is
-- accept-and-drop today, and the chat.reset that sessions emits
-- later in the /new path only resets chat history — neither tears
-- down the live provider turn.
-- ------------------------------------------------------------------
do
  fresh_loop()
  -- Prime an in-flight orchestrator run so cancel_all has something
  -- to abort.
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "first" })
  _test.calls_clear()
  agentic_loop.cancel_all()
  local saw_provider_interrupt = false
  local saw_graph_cancel = false
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "ollama.interrupt" and c.target == "ollama" then
      saw_provider_interrupt = true
    end
    if c.body.kind == "graph.cancel" and c.target == "reasoner-graph" then
      saw_graph_cancel = true
    end
  end
  assert(saw_provider_interrupt,
    "cancel_all must emit `<provider>.interrupt` so the binary aborts the in-flight chat completion; got "
    .. json.encode(_test.calls()))
  assert(saw_graph_cancel,
    "cancel_all must still emit graph.cancel for the orchestrator run; got "
    .. json.encode(_test.calls()))
end

-- cancel_all with no in-flight run is a no-op — must NOT spuriously
-- emit a provider interrupt. The symmetric single-Esc `cancel()`
-- already gates on current_run_id; cancel_all matches that gate.
do
  fresh_loop()
  agentic_loop.cancel_all()
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "ollama.interrupt" then
      error("cancel_all with no in-flight run must NOT emit a provider interrupt; got "
            .. json.encode(c.body))
    end
  end
end

-- ------------------------------------------------------------------
-- Bug 5 regression — replay_window exposes a public synchronous
-- setter that ncp.dispatch toggles inline as it walks the framing
-- markers. The bus.on_event subscriber alone fires too late for the
-- per-entry to_plugin loop in the same drain batch (vm.rs
-- `drain_pending_dispatch` runs invoke_dispatch BEFORE
-- dispatch_subscriptions across the entire batch). The integration
-- coverage lives in ncp_test.lua's
-- replay_window_suppresses_replayed_tool_invoke_in_same_batch /
-- replay_window_does_not_starve_nefor_tui pair; here we just pin the
-- public-API contract.
-- ------------------------------------------------------------------
do
  local replay_window = require("lib.replay_window")
  replay_window.set(true)
  assert_eq(replay_window.active(), true, "replay_window.set(true) must take effect")
  replay_window.set(false)
  assert_eq(replay_window.active(), false, "replay_window.set(false) must take effect")
end

-- ------------------------------------------------------------------
-- Bug B2 follow-up — set_model with a NEW provider walks the on-disk
-- session log for the prior chat and re-emits its history into the
-- new provider so conversation continuity holds across the switch.
-- Prior contract (clear current_state, mint fresh chat) was a
-- knowingly-lossy fallback; this is the eager rebuild path the brief
-- prefers.
--
-- The fallback (clearing current_state) still applies when the
-- session log is unreachable or has no chat.create entry for the
-- prior chat_id.
-- ------------------------------------------------------------------

-- Helper: monkey-patch the sessions module to point at a tempfile we
-- pre-populate with synthetic prior-session content.
local function with_session_log(jsonl_lines, fn)
  local tmp_path = os.tmpname()
  local fh = assert(io.open(tmp_path, "w"))
  fh:write(json.encode({ _session = true, session_id = "test", started_at = "2026-01-01T00:00:00Z" }))
  fh:write("\n")
  for _, line in ipairs(jsonl_lines) do
    fh:write(line)
    fh:write("\n")
  end
  fh:close()

  local sessions = require("sessions")
  local prior = sessions.current_path
  sessions.current_path = function() return tmp_path end
  local ok, err = pcall(fn, tmp_path)
  sessions.current_path = prior
  os.remove(tmp_path)
  if not ok then error(err, 0) end
end

local function step_entry(target, body)
  local row = {
    ts      = "2026-01-01T00:00:00.000Z",
    origin  = "step",
    payload = json.encode({ type = "event", from = "engine", body = body }),
  }
  if target ~= nil then row.target = target end
  return json.encode(row)
end

local function step_entry_from(from, body)
  return json.encode({
    ts      = "2026-01-01T00:00:00.000Z",
    origin  = "step",
    payload = json.encode({ type = "event", from = from, body = body }),
  })
end

-- Provider switch with prior chat: history is rebuilt on the new
-- provider, current_state holds the new chat_id, next submit seeds
-- it.
do
  fresh_loop()
  -- Use a chat_id that won't collide with the per-test next_id("chat")
  -- counter (it starts at 0 in fresh_loop → first mint = "chat-1").
  -- "chat-prior" picks a name outside that namespace so the assertion
  -- can prove the new id is freshly-minted, not a coincidence.
  agentic_loop._internals.state.current_state = { chat_id = "chat-prior" }

  with_session_log({
    step_entry("ollama", { kind = "ollama.chat.create", chat_id = "chat-prior", model = "qwen3" }),
    step_entry("ollama", {
      kind    = "ollama.chat.append",
      chat_id = "chat-prior",
      message = { role = "system", content = "you are helpful" },
    }),
    step_entry("ollama", {
      kind    = "ollama.chat.append",
      chat_id = "chat-prior",
      message = { role = "user", content = "the secret word is sphinx; remember it" },
    }),
    step_entry_from("provider-wrapper", {
      kind   = "tool.result",
      id     = "firing-prior",
      result = {
        text       = "got it — the secret word is sphinx.",
        next_state = { chat_id = "chat-prior" },
      },
    }),
  }, function()
    _test.calls_clear()
    send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "another-provider", model = "qwen-other" })

    -- current_state must now point at a fresh chat_id under the new
    -- provider — NOT nil (legacy lossy fallback), NOT the prior id
    -- (would collide with the old provider's binary on /resume).
    local cs = agentic_loop._internals.state.current_state
    assert(type(cs) == "table" and type(cs.chat_id) == "string" and cs.chat_id ~= "chat-prior",
      "cross-provider /model switch must mint a fresh chat_id and store it in current_state; got "
      .. json.encode(cs or {}))
    local new_chat_id = cs.chat_id

    -- Bus traffic must include <new>.chat.create + 3 chat.append
    -- (system, user, assistant from synthesised tool.result).
    local kinds = {}
    local appends_for_new = {}
    for _, c in ipairs(decode_calls()) do
      kinds[#kinds + 1] = c.body.kind
      if c.body.kind == "another-provider.chat.append"
          and c.body.chat_id == new_chat_id then
        appends_for_new[#appends_for_new + 1] = c.body.message
      end
    end

    local saw_create = false
    local create_model
    for _, c in ipairs(decode_calls()) do
      if c.body.kind == "another-provider.chat.create" then
        saw_create = true
        create_model = c.body.model
      end
    end
    assert(saw_create,
      "set_model rebuild must emit <new>.chat.create on the bus; got kinds=" .. json.encode(kinds))
    -- The new chat.create must carry the NEW model the user just selected,
    -- NOT the model recorded on the source provider's chat.create. The
    -- source log's model name lives in the OLD provider's namespace; using
    -- it in the new provider is the exact bug a cross-provider switch is
    -- trying to avoid (e.g., switching from mock to ollama would otherwise
    -- ask ollama to spin up "mock-model", which it doesn't have, so the
    -- next chat.complete returns an API error).
    assert_eq(create_model, "qwen-other",
      "<new>.chat.create must carry the NEW model from chat.model.set, not the source-log model; got "
      .. tostring(create_model))
    assert_eq(#appends_for_new, 3,
      "set_model rebuild must re-feed 3 messages (system/user + synthesised assistant); got "
      .. tostring(#appends_for_new))
    assert_eq(appends_for_new[1].role,    "system",    "first re-fed message is system")
    assert_eq(appends_for_new[2].role,    "user",      "second re-fed message is user")
    assert_eq(appends_for_new[3].role,    "assistant", "third re-fed message is synthesised assistant turn")
    assert_eq(appends_for_new[3].content, "got it — the secret word is sphinx.",
      "synthesised assistant content comes from result.text")

    -- Next submit's wrap node must seed_chat_id with the new chat_id —
    -- so reasoners.lua takes the no-create branch and the rebuild
    -- isn't undone by a fresh chat.create on a fresh id.
    _test.calls_clear()
    send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "what was the secret word?" })
    local seen_seed
    for _, c in ipairs(decode_calls()) do
      if c.body.kind == "tool.invoke"
          and c.body.name == "spawn_graph"
          and type(c.body.args) == "table"
          and type(c.body.args.graph) == "table" then
        for _, node in ipairs(c.body.args.graph.nodes or {}) do
          if node.id == "wrap" and type(node.args) == "table" then
            seen_seed = node.args.seed_chat_id
          end
        end
      end
    end
    assert_eq(seen_seed, new_chat_id,
      "post-switch submit's wrap node must seed_chat_id with the new chat_id; got " .. tostring(seen_seed))
  end)
end

-- Provider switch with NO prior chat: the lossy-fallback path. We
-- can't rebuild what we don't have.
do
  fresh_loop()
  -- No current_state pre-set.
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "another-provider", model = "qwen-other" })
  assert_eq(agentic_loop._internals.state.current_state, nil,
    "cross-provider switch with no prior chat: current_state stays nil (no rebuild possible)")
end

-- Same-provider model-only switch keeps current_state intact.
-- Conversation continuity holds because the chat_id still lives on
-- the same provider; the wrapper's chat.model.set translation
-- carries the active chat_id to the provider so it learns the new
-- model for that chat.
do
  fresh_loop()
  agentic_loop._internals.state.current_state = { chat_id = "ollama-chat-id" }
  -- fresh_loop configures provider="ollama" — sending the SAME
  -- provider with a different model is the model-only path.
  send_to_loop("nefor-tui", { kind = "chat.model.set", provider = "ollama", model = "qwen2.5-coder:32b" })
  assert(agentic_loop._internals.state.current_state ~= nil,
    "model-only switch (same provider) must NOT clear current_state")
  assert_eq(agentic_loop._internals.state.current_state.chat_id, "ollama-chat-id",
    "model-only switch must preserve the active chat_id")
end

-- ------------------------------------------------------------------
-- Cross-process /resume — agentic-loop restores state.current_state
-- chat_id from replayed wrap-firing tool.result. Without this, the
-- next live submit mints a fresh chat_id, and the openai-provider
-- wrapper's painstakingly-rebuilt history (on the prior chat_id)
-- is unused — the model replies with no memory of prior turns.
--
-- The replay window flips to active at sessions.replay.start; sessions
-- replays each step-origin envelope through the bus; agentic-loop
-- observes them. The wrap firing's tool.result carries
-- result.next_state.chat_id — the canonical chat-continuity signal.
-- ------------------------------------------------------------------
do
  fresh_loop()
  -- Fresh process: state.current_state is nil at boot.
  assert_eq(agentic_loop._internals.state.current_state, nil,
    "post-reset, current_state is nil (fresh-process equivalent)")

  -- Sessions replay opens.
  _test.fire_bus("sessions.replay.start", { session_id = "old-session", count = 0 })
  -- Replayed envelopes flow through the bus. The wrap firing's
  -- tool.result is the canonical close envelope sessions persisted —
  -- replay re-sends it. agentic-loop's firing_to_node table is empty
  -- (fresh process boot), so the existing live firing-close handler
  -- returns early. Replay path must capture chat_id independently.
  send_to_loop("provider-wrapper", {
    kind   = "tool.result",
    id     = "firing-replayed-1",
    result = {
      text          = "Hello! How can I help?",
      finish_reason = "stop",
      next_state    = { chat_id = "chat-resumed-1" },
    },
  })
  _test.fire_bus("sessions.replay.end", { session_id = "old-session" })

  assert(agentic_loop._internals.state.current_state ~= nil,
    "replayed wrap-firing tool.result must restore state.current_state on cross-process /resume")
  assert_eq(agentic_loop._internals.state.current_state.chat_id, "chat-resumed-1",
    "current_state.chat_id must come from result.next_state.chat_id of the replayed wrap firing")

  -- Sanity: the next live submit seeds the wrap node with the
  -- restored chat_id. Without that, reasoners.lua mints a fresh
  -- chat-N and the wrapper's history rebuild on the OLD chat_id is
  -- orphaned (the user-visible "no memory" symptom).
  _test.calls_clear()
  send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "what was the secret word?" })
  local seeded
  for _, c in ipairs(decode_calls()) do
    if c.body.kind == "tool.invoke"
        and c.body.name == "spawn_graph"
        and type(c.body.args) == "table"
        and type(c.body.args.graph) == "table" then
      for _, node in ipairs(c.body.args.graph.nodes or {}) do
        if node.id == "wrap" and type(node.args) == "table" then
          seeded = node.args.seed_chat_id
        end
      end
    end
  end
  assert_eq(seeded, "chat-resumed-1",
    "post-/resume submit must seed the wrap node with the recovered chat_id")
end

-- Multiple replayed tool.results in one window — latest chat_id wins.
-- A session may carry several wrap-firings (one per turn) plus one
-- /new mid-session that mints a fresh chat_id. The most recent
-- recorded chat_id is the active one at /resume time.
do
  fresh_loop()
  _test.fire_bus("sessions.replay.start", { session_id = "old-session", count = 0 })
  send_to_loop("provider-wrapper", {
    kind   = "tool.result",
    id     = "firing-replayed-old",
    result = {
      text       = "first turn reply",
      next_state = { chat_id = "chat-old-1" },
    },
  })
  send_to_loop("provider-wrapper", {
    kind   = "tool.result",
    id     = "firing-replayed-newer",
    result = {
      text       = "second turn reply",
      next_state = { chat_id = "chat-new-1" },
    },
  })
  _test.fire_bus("sessions.replay.end", { session_id = "old-session" })
  assert_eq(agentic_loop._internals.state.current_state.chat_id, "chat-new-1",
    "the LATEST replayed wrap-firing chat_id wins (covers /new mid-session)")
end

-- tool.result envelopes WITHOUT result.next_state.chat_id (run-close,
-- terminal close, sub-graph synth) must NOT clobber current_state.
-- The orchestrator-run close shape carries result.results (a table of
-- per-node outputs), not result.next_state.
do
  fresh_loop()
  -- Pre-seed something so the negative assertion has a baseline.
  agentic_loop._internals.state.current_state = { chat_id = "pre-existing" }
  _test.fire_bus("sessions.replay.start", { session_id = "old-session", count = 0 })
  send_to_loop("reasoner-graph", {
    kind   = "tool.result",
    id     = "run-close-id",
    result = {
      status  = "success",
      results = { wrap = { output = { text = "x" } } },
    },
  })
  _test.fire_bus("sessions.replay.end", { session_id = "old-session" })
  assert_eq(agentic_loop._internals.state.current_state.chat_id, "pre-existing",
    "run-close tool.result (no top-level next_state) must NOT clobber current_state during replay")
end

-- ------------------------------------------------------------------
-- Cross-provider /resume — restores active provider+model from the
-- session log so the next live submit dispatches against the chat_id's
-- owning provider, not whatever provider the LIVE session happened to
-- have selected at the moment of /resume.
--
-- User repro that motivates this:
--   1. boot on default provider (mock-plugin) + send a turn → chat-1
--      lives on mock-plugin
--   2. /new (mints fresh session)
--   3. /model qwen → state.config.provider = "ollama"
--   4. /resume back to the chat-1 session
--   5. send a new message → dispatches against state.config.provider
--      (still "ollama") with state.current_state.chat_id = "chat-1"
--      → ollama doesn't have chat-1 → "[Error: chat 'chat-1' not found]"
--
-- The fix: on `sessions.replay.start`, walk the resumed session's log
-- and derive the active (provider, model) — latest `chat.model.set` if
-- any, otherwise the prefix + model on the latest `<prefix>.chat.create`
-- — and update state.config so the next live submit dispatches through
-- the right provider.
-- ------------------------------------------------------------------

-- (a) Default-provider session (no /model switch): provider+model derived
-- from the latest `<prefix>.chat.create`.
do
  fresh_loop()
  -- Simulate the user's step-3 state: live config switched to ollama via
  -- /model BEFORE the /resume.
  agentic_loop.configure { provider = "ollama", model = "qwen-other" }

  with_session_log({
    -- Original session was created on mock-plugin and never saw /model.
    step_entry("mock-plugin", {
      kind = "mock-plugin.chat.create", chat_id = "chat-1", model = "mock-model",
    }),
    step_entry("mock-plugin", {
      kind    = "mock-plugin.chat.append",
      chat_id = "chat-1",
      message = { role = "user", content = "hello" },
    }),
    step_entry_from("provider-wrapper", {
      kind   = "tool.result",
      id     = "firing-original",
      result = {
        text       = "hi there",
        next_state = { chat_id = "chat-1" },
      },
    }),
  }, function()
    _test.fire_bus("sessions.replay.start", { session_id = "old", count = 0 })
    -- The replayed wrap-firing tool.result restores chat_id (per 91d49ef).
    send_to_loop("provider-wrapper", {
      kind   = "tool.result",
      id     = "firing-original",
      result = {
        text       = "hi there",
        next_state = { chat_id = "chat-1" },
      },
    })
    _test.fire_bus("sessions.replay.end", { session_id = "old" })

    assert_eq(agentic_loop.config().provider, "mock-plugin",
      "post-/resume, state.config.provider must be derived from the resumed session's "
      .. "chat.create prefix, not the live-pre-resume value; got "
      .. tostring(agentic_loop.config().provider))
    assert_eq(agentic_loop.config().model, "mock-model",
      "post-/resume, state.config.model must be derived from the resumed session's "
      .. "chat.create model, not the live-pre-resume value; got "
      .. tostring(agentic_loop.config().model))
    assert_eq(agentic_loop._internals.state.current_state.chat_id, "chat-1",
      "post-/resume, current_state.chat_id is restored from the replayed wrap-firing")

    -- Step 5: live submit. The dispatched wrap node MUST seed_chat_id with
    -- chat-1 AND the dispatch must ride state.config.provider = mock-plugin.
    _test.calls_clear()
    send_to_loop("nefor-tui", { kind = "chat.input.submit", text = "follow up" })
    local seeded_chat_id
    local seeded_provider
    for _, c in ipairs(decode_calls()) do
      if c.body.kind == "tool.invoke"
          and c.body.name == "spawn_graph"
          and type(c.body.args) == "table"
          and type(c.body.args.graph) == "table" then
        for _, node in ipairs(c.body.args.graph.nodes or {}) do
          if node.id == "wrap" and type(node.args) == "table" then
            seeded_chat_id = node.args.seed_chat_id
            seeded_provider = node.args.provider
          end
        end
      end
    end
    assert_eq(seeded_chat_id, "chat-1",
      "post-/resume submit must seed wrap with the resumed chat_id; got "
      .. tostring(seeded_chat_id))
    -- The wrap node intentionally doesn't bake provider into args; the live
    -- cfg.provider drives dispatch (Bug B1). The cfg-level assertion above
    -- already pins the user-visible bug — that's the dispatch path.
    assert_eq(seeded_provider, nil,
      "wrap node still doesn't bake provider into args (picker is source of truth)")
  end)
end

-- (b) /model-switched session: latest `chat.model.set` wins over
-- `chat.create` model.
do
  fresh_loop()
  agentic_loop.configure { provider = "ollama", model = "qwen-other" }

  with_session_log({
    step_entry("mock-plugin", {
      kind = "mock-plugin.chat.create", chat_id = "chat-1", model = "mock-model",
    }),
    -- Mid-session /model switch (canonical user-facing envelope).
    step_entry(nil, { kind = "chat.model.set", provider = "anthropic", model = "claude-test" }),
    step_entry_from("provider-wrapper", {
      kind   = "tool.result",
      id     = "firing-original",
      result = {
        text       = "hi",
        next_state = { chat_id = "chat-1" },
      },
    }),
  }, function()
    _test.fire_bus("sessions.replay.start", { session_id = "old", count = 0 })
    send_to_loop("provider-wrapper", {
      kind   = "tool.result",
      id     = "firing-original",
      result = {
        text       = "hi",
        next_state = { chat_id = "chat-1" },
      },
    })
    _test.fire_bus("sessions.replay.end", { session_id = "old" })

    assert_eq(agentic_loop.config().provider, "anthropic",
      "post-/resume, state.config.provider must be derived from the LATEST chat.model.set, not chat.create; got "
      .. tostring(agentic_loop.config().provider))
    assert_eq(agentic_loop.config().model, "claude-test",
      "post-/resume, state.config.model must be derived from the LATEST chat.model.set, not chat.create; got "
      .. tostring(agentic_loop.config().model))
  end)
end

-- (c) Same-provider /resume: config stays at original values (no
-- observable change, but the path still runs cleanly).
do
  fresh_loop()
  -- Live config matches the resumed session's provider — no change expected.
  agentic_loop.configure { provider = "mock-plugin", model = "mock-model" }

  with_session_log({
    step_entry("mock-plugin", {
      kind = "mock-plugin.chat.create", chat_id = "chat-1", model = "mock-model",
    }),
  }, function()
    _test.fire_bus("sessions.replay.start", { session_id = "old", count = 0 })
    _test.fire_bus("sessions.replay.end", { session_id = "old" })
    assert_eq(agentic_loop.config().provider, "mock-plugin",
      "same-provider /resume keeps provider")
    assert_eq(agentic_loop.config().model, "mock-model",
      "same-provider /resume keeps model")
  end)
end

-- (d) Empty session log (no chat.create yet): config stays at the live
-- pre-resume values. There's nothing in the log to restore from, and
-- there's no chat_id to dispatch against either, so this is a soft fail
-- that doesn't clobber state.
do
  fresh_loop()
  agentic_loop.configure { provider = "ollama", model = "qwen-other" }
  with_session_log({}, function()
    _test.fire_bus("sessions.replay.start", { session_id = "empty", count = 0 })
    _test.fire_bus("sessions.replay.end", { session_id = "empty" })
    assert_eq(agentic_loop.config().provider, "ollama",
      "empty-log /resume: provider unchanged")
    assert_eq(agentic_loop.config().model, "qwen-other",
      "empty-log /resume: model unchanged")
  end)
end

print("agentic_workflow_test: ok")
