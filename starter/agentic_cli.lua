-- starter/agentic_cli.lua — agentic-cli plugin (pure-Lua, virtual).
--
-- Surfaces agentic_workflow as a stdin/stdout CLI: single-shot prompt or
-- interactive REPL. Behaviour parity with the TUI is by construction —
-- agentic_workflow is the single source of truth and this module only
-- changes the surface (stdin/stdout vs. chat-contract → nefor-chat →
-- nefor-tui).
--
-- ### Lifecycle
--
-- Engine entry point: `nefor plugin agentic-cli [args...]`. The engine
-- invokes `M.run(argv)` synchronously on the main thread, holding the
-- Lua VM mutex. While this runs, plugin lines queue in the broker but
-- step is NOT being driven — so the cli function must register its
-- callbacks and RETURN to let the broker enter its run loop. Subsequent
-- bus traffic flows through agentic_workflow's transforms; observers
-- registered here fire as turns complete.
--
-- ### Output formats
--
--   text (default)   — stream chat.stream.delta to stdout in real time;
--                      tool-call one-liners to stderr; trailing newline
--                      on the orchestrator-run tool.result close.
--   json             — single JSON line per turn on completion:
--                      { answer, tool_calls, duration_ms }.
--   stream-json      — passthrough: every chat.* / graph.* envelope as
--                      one JSON line on stdout (NCP wire format). The
--                      user's prompt is NOT emitted as a chat.input.submit
--                      envelope here because agentic_workflow.submit
--                      dispatches directly to the orchestrator rather
--                      than going through the bus (the chat-plugin path
--                      that produces chat.input.submit isn't on the wire
--                      in CLI mode). Reconstruct the prompt from the
--                      process argv if a transcript replay needs it.
--
-- ### REPL design (event-driven via callbacks)
--
-- The known caveat with `nefor.io.read_line` is that it blocks the Lua
-- VM. That makes a `while line do submit; wait end` loop impossible —
-- there's no way to wait for events while holding the VM. We sidestep
-- that: in REPL mode, on_complete reads the next line and submits it.
-- The "loop" is a chain of in-process callbacks. EOF on read_line ends
-- the session via `nefor.engine.exit(0)`.
--
-- See module's `--help` text for the full flag table.

local M = {}

-- The orchestrator is now exposed by the agentic-loop actor. Public
-- API (on_stream / on_tool_start / on_tool_end / on_complete / submit
-- / set_model / set_yolo) is preserved verbatim from the prior
-- agentic_workflow module.
local agentic_workflow = require("agentic-loop")
local json = nefor.json

-- ------------------------------------------------------------------
-- argv parser
-- ------------------------------------------------------------------
--
-- Hand-rolled, no external dep. Recognises:
--
--   <prompt>                       single positional → single-shot mode
--   -m / --model <model>           switch model on the active provider
--   --yolo                         set yolo mode
--   --format text|json|stream-json output format
--   -f / --file <path>             prepend file contents to prompt
--   --debug                        log-level hint (no-op for v1)
--   -h / --help                    print usage and exit 0
--
-- Multiple positionals concatenate with spaces (so the user can pass an
-- unquoted prompt). `--` ends flag parsing; everything after is treated
-- as positional. Unknown flags are a hard error.

local USAGE = [[Usage: nefor [--config <DIR>] plugin agentic-cli [--] [OPTIONS] [PROMPT]

OPTIONS:
  -m, --model <MODEL>          Switch model on the active provider before the first turn.
      --yolo                   Enable yolo mode (placeholder; tool-gate not yet wired).
      --format <FMT>           Output format: text (default) | json | stream-json.
  -f, --file <PATH>            Read PATH and prepend its contents to the prompt.
      --debug                  No-op for v1.
  -h, --help                   Show this help and exit.

NOTE: clap consumes `-h` / `--help` at the engine level. To see this
help, pass `--` first, e.g.:
  nefor --config <DIR> plugin agentic-cli -- --help

When PROMPT is given (one positional, optionally quoted), runs single-shot.
Without PROMPT, enters an interactive REPL reading prompts from stdin until
EOF (Ctrl-D). The REPL drives the same agentic_workflow as the TUI, so
behaviour parity is by construction.

OUTPUT FORMATS:
  text          stream chat.stream.delta to stdout; tool one-liners to
                stderr; trailing newline on completion. Default.
  json          one JSON line per turn at completion:
                { "answer": "...", "tool_calls": [...], "duration_ms": ... }
  stream-json   passthrough: every chat.*/graph.* envelope as one JSON
                line on stdout. Matches NCP wire format.
]]

local VALID_FORMATS = { text = true, json = true, ["stream-json"] = true }

local function parse_argv(argv)
  local opts = {
    model = nil,
    yolo = false,
    format = "text",
    file = nil,
    debug = false,
    help = false,
    prompt = nil,
  }
  local positional = {}
  local i = 1
  local end_of_flags = false
  while i <= #argv do
    local a = argv[i]
    if end_of_flags then
      positional[#positional + 1] = a
    elseif a == "--" then
      end_of_flags = true
    elseif a == "-h" or a == "--help" then
      opts.help = true
      return opts
    elseif a == "--yolo" then
      opts.yolo = true
    elseif a == "--debug" then
      opts.debug = true
    elseif a == "-m" or a == "--model" then
      i = i + 1
      if argv[i] == nil then
        return nil, "missing value for " .. a
      end
      opts.model = argv[i]
    elseif a == "--format" then
      i = i + 1
      if argv[i] == nil then
        return nil, "missing value for --format"
      end
      if not VALID_FORMATS[argv[i]] then
        return nil, "invalid --format value: " .. tostring(argv[i]) ..
                    " (expected: text | json | stream-json)"
      end
      opts.format = argv[i]
    elseif a == "-f" or a == "--file" then
      i = i + 1
      if argv[i] == nil then
        return nil, "missing value for " .. a
      end
      opts.file = argv[i]
    elseif a:sub(1, 1) == "-" then
      return nil, "unknown flag: " .. a
    else
      positional[#positional + 1] = a
    end
    i = i + 1
  end

  if #positional > 0 then
    opts.prompt = table.concat(positional, " ")
  end
  return opts, nil
end

-- ------------------------------------------------------------------
-- file prepend
-- ------------------------------------------------------------------

local function read_file(path)
  local fh, err = io.open(path, "r")
  if not fh then
    return nil, "could not open file `" .. path .. "`: " .. tostring(err)
  end
  local contents = fh:read("*a")
  fh:close()
  return contents, nil
end

local function build_prompt_with_file(prompt, file_path)
  local contents, err = read_file(file_path)
  if not contents then return nil, err end
  local header = "### File: " .. file_path .. "\n```\n" .. contents .. "\n```\n\n"
  return header .. (prompt or ""), nil
end

-- ------------------------------------------------------------------
-- output handlers
-- ------------------------------------------------------------------
--
-- `text` format streams via on_stream / on_tool_*. `json` accumulates
-- final state and prints once on on_complete. `stream-json` registers
-- nefor.bus.on_event handlers for every chat.*/graph.* kind and prints
-- each envelope as a JSON line.

local function write_stdout(s) io.stdout:write(s); io.stdout:flush() end
local function write_stderr(s) io.stderr:write(s); io.stderr:flush() end

-- Milliseconds-since-midnight (UTC) parsed from `nefor.engine.now()`'s
-- ISO-8601 ms-precision string. Lua 5.4's `os.time()` is whole-seconds,
-- so a sub-1s mock turn rounds to 0; this preserves precision.
-- Wraps at midnight UTC — acceptable for sub-day CLI sessions; if a
-- session genuinely spans midnight the duration_ms field reads negative.
-- A real wall-clock binding (`nefor.engine.now_ms()`) would close that.
local function now_ms()
  local ts = nefor.engine.now()
  local h, m, s, ms = ts:match("T(%d+):(%d+):(%d+)%.(%d+)Z")
  if not h then return 0 end
  return (((tonumber(h) * 60) + tonumber(m)) * 60 + tonumber(s)) * 1000 + tonumber(ms)
end

local function install_text_format(gate)
  agentic_workflow.on_stream(function(text)
    if gate and gate.suppress_stream then return end
    if type(text) == "string" and #text > 0 then
      write_stdout(text)
    end
  end)
  agentic_workflow.on_tool_start(function(_id, name, input)
    -- One-liner to stderr keeps stdout pipe-clean.
    local input_preview
    if type(input) == "table" then
      local ok, encoded = pcall(json.encode, input)
      input_preview = ok and encoded or "?"
      if #input_preview > 80 then
        input_preview = input_preview:sub(1, 77) .. "..."
      end
    else
      input_preview = tostring(input)
    end
    write_stderr("[tool: " .. tostring(name) .. "(" .. input_preview .. ")]\n")
  end)
  agentic_workflow.on_tool_end(function(_id, _output, err)
    if err then
      write_stderr("[tool error]\n")
    end
  end)
end

local function install_json_format(state)
  -- JSON mode: suppress streaming, accumulate tool calls + final text,
  -- print on completion.
  state.tool_calls = {}
  state.answer_acc = {}
  agentic_workflow.on_stream(function(text)
    if type(text) == "string" and #text > 0 then
      state.answer_acc[#state.answer_acc + 1] = text
    end
  end)
  agentic_workflow.on_tool_start(function(id, name, input)
    state.tool_calls[#state.tool_calls + 1] = {
      id = id, name = name, input = input,
    }
  end)
end

local function install_stream_json_format()
  -- Passthrough: subscribe to chat.* and graph.* kinds; emit each
  -- envelope as one JSON line. The handler receives a log-entry table
  -- (`{ ts, origin, target, payload }`); `payload` is the already-
  -- serialised envelope JSON, so we emit it verbatim — no re-encoding,
  -- no decode-then-encode round trip. Matches NCP's wire format.
  local function emit_env(entry)
    local payload = type(entry) == "table" and entry.payload or nil
    if type(payload) == "string" and #payload > 0 then
      write_stdout(payload)
      if payload:sub(-1) ~= "\n" then write_stdout("\n") end
    end
  end
  nefor.bus.on_event("chat.*", emit_env)
  nefor.bus.on_event("graph.*", emit_env)
  -- Reasoner-graph close envelopes ride the canonical tool contract:
  -- `tool.result { id=<run_id|firing_id>, result | error }`. Include
  -- the family so stream-json transcripts retain run-close visibility.
  nefor.bus.on_event("tool.*", emit_env)
end

-- ------------------------------------------------------------------
-- run modes
-- ------------------------------------------------------------------
--
-- Both modes defer the first action until all plugins are ready. The
-- cli function itself runs synchronously on the main thread BEFORE the
-- broker enters its run loop — meaning any envelopes we emit at this
-- point are dropped (target plugins haven't sent their `ready` yet, so
-- their NCP layer ignores incoming traffic). We wait for the last
-- plugin in the spawn chain (`basic-tools`) to fire `basic-tools.ready`
-- via `nefor.bus.on_event`; that guarantees every upstream plugin
-- (combinators, provider, reasoner-graph, tool-gate) is also up. Only
-- then do we submit / read_line.

-- Sentinel kind whose arrival means "every plugin is up". Tied to the
-- spawn order in cli-config/init.lua: basic-tools is last in the chain
-- and emits `basic-tools.hello` immediately after its NCP handshake,
-- so by the time we see it everyone upstream is ready too. Switching
-- providers / shuffling spawn order MUST update this.
local READY_SENTINEL = "basic-tools.hello"

-- Single-shot: register on_complete, wait for ready sentinel, submit
-- once, return. on_complete prints the final output (text or JSON)
-- and exits.
--
-- Async spawn_graph caveat: when the orchestrator turn calls
-- spawn_graph the first on_complete fires WHILE the sub-graph is still
-- running. agentic_workflow then queues the sub-graph's eventual
-- result and re-submits a relay turn. We need to wait through that
-- second turn to print the actual final answer. Heuristic: if any tool
-- call this turn was spawn_graph, suppress the first on_complete and
-- wait for the next.
local function run_single_shot(prompt, format, json_state, turn_start_ms, gate)
  local spawn_graph_inflight = false
  local already_exited = false

  agentic_workflow.on_tool_start(function(_id, name, _input)
    if name == "spawn_graph" then
      spawn_graph_inflight = true
      -- Suppress text streaming for the first turn — the user wants the
      -- final relayed answer, not the transitional "Started the sub-graph"
      -- ack. The relay turn re-opens the gate.
      if gate then gate.suppress_stream = true end
    end
  end)

  local function emit_completion(status)
    if already_exited then return end
    already_exited = true
    if format == "json" then
      local answer = table.concat(json_state.answer_acc or {})
      local duration_ms = now_ms() - turn_start_ms
      local payload = {
        answer = answer,
        tool_calls = json_state.tool_calls or {},
        duration_ms = duration_ms,
        status = status,
      }
      local ok, encoded = pcall(json.encode, payload)
      if ok then write_stdout(encoded .. "\n") end
    elseif format == "text" then
      write_stdout("\n")
    end
    -- stream-json: nothing extra; the run-close tool.result already
    -- passed through the bus subscription.
    nefor.engine.exit(0)
  end

  agentic_workflow.on_complete(function(_run_id, status)
    if spawn_graph_inflight then
      -- Suppress the first complete; reset the flag so the next turn
      -- (relay of the deferred sub-graph result) is the one we exit on.
      spawn_graph_inflight = false
      -- Re-open the stream gate so the relay turn's content reaches
      -- stdout.
      if gate then gate.suppress_stream = false end
      -- Reset accumulated answer text so the JSON payload only carries
      -- the relay turn's content (matches user expectation: "the answer"
      -- is the final user-facing text). Tool calls accumulate across both
      -- firings — spawn_graph from the first turn belongs in the report
      -- alongside any tool calls in the relay turn.
      if format == "json" then
        json_state.answer_acc = {}
      end
      return
    end
    emit_completion(status)
  end)

  local fired = false
  nefor.bus.on_event(READY_SENTINEL, function(_env)
    if fired then return end
    fired = true
    agentic_workflow.submit(prompt)
  end)
end

-- REPL: wait for ready sentinel, then read first line, submit;
-- on_complete reads the next line and submits again. EOF on read_line
-- → exit(0).
--
-- Async spawn_graph caveat (same as single-shot): if a turn calls
-- spawn_graph, we get TWO on_complete events back to back — one for
-- the orchestrator's first turn (returns the transitional ack) and
-- one for the deferred-result relay turn. We only want to read the
-- next user line after the relay turn lands.
local function run_repl(format, json_state, gate)
  local spawn_graph_inflight = false

  local function reset_json_state()
    if format == "json" then
      json_state.tool_calls = {}
      json_state.answer_acc = {}
    end
  end

  local function emit_completion_output()
    if format == "json" then
      local answer = table.concat(json_state.answer_acc or {})
      local payload = {
        answer = answer,
        tool_calls = json_state.tool_calls or {},
      }
      local ok, encoded = pcall(json.encode, payload)
      if ok then write_stdout(encoded .. "\n") end
    elseif format == "text" then
      write_stdout("\n")
    end
  end

  agentic_workflow.on_tool_start(function(_id, name, _input)
    if name == "spawn_graph" then
      spawn_graph_inflight = true
      if gate then gate.suppress_stream = true end
    end
  end)

  local function read_and_submit()
    write_stderr("> ")
    local line = nefor.io.read_line()
    -- Skip blank lines so users can press Enter without firing an
    -- empty submit.
    while line ~= nil and #line == 0 do
      write_stderr("> ")
      line = nefor.io.read_line()
    end
    if line == nil then
      -- EOF — exit cleanly.
      write_stderr("\n")
      nefor.engine.exit(0)
      return
    end
    reset_json_state()
    agentic_workflow.submit(line)
  end

  agentic_workflow.on_complete(function(_run_id, _status)
    if spawn_graph_inflight then
      spawn_graph_inflight = false
      if gate then gate.suppress_stream = false end
      -- Don't print or prompt yet — wait for the deferred relay turn.
      -- Reset answer_acc so the JSON payload only carries the relay
      -- turn's text; keep tool_calls so spawn_graph (from this firing)
      -- stays in the final report.
      if format == "json" then
        json_state.answer_acc = {}
      end
      return
    end
    emit_completion_output()
    read_and_submit()
  end)

  -- Kick off the loop on the ready sentinel.
  local fired = false
  nefor.bus.on_event(READY_SENTINEL, function(_env)
    if fired then return end
    fired = true
    read_and_submit()
  end)
end

-- ------------------------------------------------------------------
-- entry point
-- ------------------------------------------------------------------

function M.run(argv)
  argv = argv or {}
  local opts, parse_err = parse_argv(argv)
  if parse_err then
    write_stderr("agentic-cli: " .. parse_err .. "\n\n")
    write_stderr(USAGE)
    nefor.engine.exit(2)
    return 2
  end
  if opts.help then
    write_stdout(USAGE)
    nefor.engine.exit(0)
    return 0
  end

  -- File prepend (works in single-shot only — REPL doesn't have a
  -- prompt yet).
  if opts.file ~= nil and opts.prompt == nil then
    write_stderr("agentic-cli: -f/--file requires a positional PROMPT\n")
    nefor.engine.exit(2)
    return 2
  end
  if opts.file ~= nil then
    local prompt, err = build_prompt_with_file(opts.prompt, opts.file)
    if err then
      write_stderr("agentic-cli: " .. err .. "\n")
      nefor.engine.exit(1)
      return 1
    end
    opts.prompt = prompt
  end

  -- Apply pre-turn config overrides.
  if opts.model ~= nil then
    -- Provider name comes from agentic_workflow's setup. We don't
    -- expose a getter, so we ask the workflow to use the same provider
    -- it was configured with — passing nil means "keep the configured
    -- provider". Fall back to a sentinel that set_model treats as
    -- no-op.
    agentic_workflow.set_model(nil, opts.model)
  end
  if opts.yolo then
    agentic_workflow.set_yolo(true)
  end

  local state = {}
  -- Stream-suppression gate; mutated by run_single_shot / run_repl when
  -- async spawn_graph deferral demands holding back the first turn.
  local gate = { suppress_stream = false }

  -- Wire output observers based on format.
  if opts.format == "text" then
    install_text_format(gate)
  elseif opts.format == "json" then
    install_json_format(state)
  elseif opts.format == "stream-json" then
    install_stream_json_format()
  end

  -- Branch on mode.
  if opts.prompt ~= nil then
    run_single_shot(opts.prompt, opts.format, state, now_ms(), gate)
  else
    run_repl(opts.format, state, gate)
  end
  return 0
end

-- ------------------------------------------------------------------
-- test-only exports
-- ------------------------------------------------------------------

function M._parse_argv(argv) return parse_argv(argv) end
function M._usage() return USAGE end

return M
