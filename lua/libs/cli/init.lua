-- lua/libs/cli/init.lua — agentic-cli plugin (pure-Lua, virtual).
--
-- Submits through agentic-loop and renders conversation-manager's universal
-- projection as either a single-shot command or interactive REPL.
--
-- ### Lifecycle
--
-- Engine entry point: `nefor plugin agentic-cli [args...]`. The engine
-- invokes `M.run(argv)` synchronously on the main thread, holding the
-- Lua VM mutex. While this runs, plugin lines queue in the broker but
-- step is NOT being driven — so the cli function must register its
-- callbacks and RETURN to let the broker enter its run loop. Subsequent
-- bus traffic flows through conversation-manager's projection; observers
-- registered here fire as canonical turns complete.
--
-- ### Output formats
--
--   text (default)   — stream canonical text deltas to stdout in real time;
--                      tool-call one-liners to stderr; trailing newline
--                      on the orchestrator-run tool.result close.
--   json             — single JSON line per turn on completion:
--                      { answer, tool_calls, duration_ms }.
--   stream-json      — passthrough: every conversation.* / mag.* / tool.*
--                      envelope as one JSON line on stdout (NCP wire
--                      format). The
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
-- that: in REPL mode, the canonical terminal callback reads the next line.
-- The "loop" is a chain of in-process callbacks. EOF on read_line ends
-- the session via `nefor.engine.exit(0)`.
--
-- See module's `--help` text for the full flag table.

local M = {}

local agentic_workflow = require("libs.agentic-loop")
local conversation_projection = require("libs.chat.conversation_projection")
local json = nefor.json

-- Argv parser. Hand-rolled, no external dep. Recognises:
--
--   <prompt>                       single positional → single-shot mode
--   -m / --model <model>           switch model on the active provider
--   --think/--reasoning-effort <L> set reasoning effort on the active provider
--   --mode safe|auto|yolo          set permission mode
--   --yolo                         alias for --mode yolo
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
      --think <LEVEL>          Set reasoning effort before the first turn.
      --reasoning-effort <LEVEL>
                               Alias for --think.
      --mode <MODE>            Permission mode: safe | auto | yolo.
      --yolo                   Alias for --mode yolo.
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
  text          stream conversation text deltas to stdout; tool one-liners to
                stderr; trailing newline on completion. Default.
  json          one JSON line per turn at completion:
                { "answer": "...", "tool_calls": [...], "duration_ms": ... }
  stream-json   passthrough: every conversation.*/mag.*/tool.* envelope as one
                JSON line on stdout. Matches NCP wire format.
]]

local VALID_FORMATS = { text = true, json = true, ["stream-json"] = true }

local function parse_argv(argv)
  local opts = {
    model = nil,
    reasoning_effort = nil,
    mode = nil,
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
      opts.mode = "yolo"
    elseif a == "--mode" then
      i = i + 1
      if argv[i] == nil then
        return nil, "missing value for --mode"
      end
      if argv[i] ~= "safe" and argv[i] ~= "auto" and argv[i] ~= "yolo" then
        return nil, "invalid --mode value: " .. tostring(argv[i]) ..
                    " (expected: safe | auto | yolo)"
      end
      opts.mode = argv[i]
    elseif a == "--debug" then
      opts.debug = true
    elseif a == "-m" or a == "--model" then
      i = i + 1
      if argv[i] == nil then
        return nil, "missing value for " .. a
      end
      opts.model = argv[i]
    elseif a == "--think" or a == "--reasoning-effort" then
      i = i + 1
      if argv[i] == nil then
        return nil, "missing value for " .. a
      end
      opts.reasoning_effort = argv[i]
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

-- Output handlers consume the same universal projection as the TUI.

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

local function install_stream_json_format()
  -- Passthrough: subscribe to universal conversation and run/tool kinds.
  -- Each envelope is one JSON line. The handler receives a log-entry table
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
  nefor.bus.on_event("conversation.*", emit_env)
  -- Kernel-run lifecycle (mag.run_started / mag.run_result / actor
  -- spawn-ready events) — the lead's turn-programs and its dispatched
  -- runs both execute on the mag kernel.
  nefor.bus.on_event("mag.*", emit_env)
  -- Run-close envelopes ride the canonical tool contract:
  -- `tool.result { id=<run_id|firing_id>, result | error }`. Include
  -- the family so stream-json transcripts retain run-close visibility.
  nefor.bus.on_event("tool.*", emit_env)
end

local function preview(value)
  if type(value) == "string" then return value end
  local ok, encoded = pcall(json.encode, value)
  return ok and encoded or tostring(value)
end

local function install_conversation_output(format, state, gate)
  state.projection = conversation_projection.new()
  state.tool_calls = {}

  local function consume(entry)
    local payload = type(entry) == "table" and entry.payload or nil
    if type(payload) ~= "string" or payload == "" then return end
    local ok, envelope = pcall(json.decode, payload)
    if not ok or type(envelope) ~= "table" or type(envelope.body) ~= "table" then return end
    local projection, actions = conversation_projection.reduce(state.projection, envelope.body)
    state.projection = projection
    for _, item in ipairs(actions) do
      if item.kind == "text_delta" then
        if format == "text" and not gate.suppress_stream then write_stdout(item.text) end
      elseif item.kind == "tool_started" then
        state.tool_calls[#state.tool_calls + 1] = {
          id = item.exchange_id, name = item.name, input = item.arguments,
        }
        if format == "text" then
          local input = preview(item.arguments)
          if #input > 80 then input = input:sub(1, 77) .. "..." end
          write_stderr("[tool: " .. tostring(item.name) .. "(" .. input .. ")]\n")
        end
        if state.on_tool_started then state.on_tool_started(item.name, item.arguments) end
      elseif item.kind == "tool_completed" and item.error and format == "text" then
        write_stderr("[tool error: " .. preview(item.output) .. "]\n")
      elseif item.kind == "turn_completed" and state.on_terminal then
        state.on_terminal(item.turn_id, "success", item.answer, item.terminal)
      elseif item.kind == "turn_failed" and state.on_terminal then
        state.on_terminal(item.turn_id, "error", nil, item.terminal)
      elseif item.kind == "turn_interrupted" and state.on_terminal then
        state.on_terminal(item.turn_id, "interrupted", nil, item.terminal)
      end
    end
  end

  nefor.bus.on_event("conversation.active.changed", consume)
  nefor.bus.on_event("conversation.projection.delta", consume)
  nefor.bus.on_event("conversation.snapshot", consume)
end

-- Run modes defer the first prompt until startup-readiness has observed
-- every required plugin hello and tool-gate's complete public catalog. This
-- is intentionally independent of spawn and advertisement ordering.

local readiness = require("libs.startup-readiness")
local readiness_config = nil

local function wait_until_ready(on_ready)
  assert(type(readiness_config) == "table",
    "agentic-cli readiness is not configured by the composition")
  readiness.wait {
    required_plugins = readiness_config.required_plugins,
    required_tools = readiness_config.required_tools,
    tool_sources = readiness_config.tool_sources,
    timeout_ms = readiness_config.timeout_ms,
    on_ready = on_ready,
    on_error = function(message)
      write_stderr("agentic-cli: " .. message .. "\n")
      nefor.engine.exit(1)
    end,
  }
end

-- Single-shot: register the canonical turn terminal callback, wait for the
-- readiness barrier, submit once. The callback prints final output
-- and exits.
--
-- Async dispatch caveat: when the orchestrator turn executes a kernel
-- run (the lead's `mag` tool with action=execute) the first terminal projection
-- fires WHILE the run is still going. agentic_workflow then queues the
-- run's eventual result and re-submits a relay turn. We need to wait
-- through that second turn to print the actual final answer —
-- the first on_complete is a transitional ack turn.
local function is_async_dispatch(name, input)
  return name == "mag" and type(input) == "table" and input.action == "execute"
end

local function run_single_shot(prompt, format, state, turn_start_ms, gate)
  local async_dispatch_inflight = false
  local already_exited = false

  state.on_tool_started = function(name, input)
    if is_async_dispatch(name, input) then
      async_dispatch_inflight = true
      -- Suppress text streaming for the first turn — the user wants
      -- the final relayed answer, not the transitional dispatch ack.
      -- The relay turn re-opens the gate.
      if gate then gate.suppress_stream = true end
    end
  end

  local function emit_completion(status, answer, terminal)
    if already_exited then return end
    already_exited = true
    if format == "json" then
      local duration_ms = type(terminal) == "table" and terminal.duration_ms
        or (now_ms() - turn_start_ms)
      local payload = {
        answer = type(answer) == "string" and answer or "",
        tool_calls = state.tool_calls or {},
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

  state.on_terminal = function(_turn_id, status, answer, terminal)
    if async_dispatch_inflight then
      -- Suppress the first complete; reset the flag so the next turn
      -- (relay of the deferred run result) is the one we exit on.
      async_dispatch_inflight = false
      -- Re-open the stream gate so the relay turn's content reaches
      -- stdout.
      if gate then gate.suppress_stream = false end
      -- Tool calls accumulate across both turns — the dispatch call from the
      -- first turn belongs in the report alongside any relay-turn calls.
      return
    end
    emit_completion(status, answer, terminal)
  end

  wait_until_ready(function()
    agentic_workflow.submit(prompt)
  end)
end

-- REPL: wait for ready sentinel, then read first line, submit;
-- the terminal callback reads the next line and submits again. EOF on read_line
-- → exit(0).
--
-- Async dispatch caveat (same as single-shot): if a turn executes a
-- kernel run, we get two terminal turn projections — one for
-- the orchestrator's first turn (returns the transitional ack) and
-- one for the deferred-result relay turn. We only want to read the
-- next user line after the relay turn lands.
local function run_repl(format, state, gate)
  local async_dispatch_inflight = false

  local function reset_json_state()
    if format == "json" then
      state.tool_calls = {}
    end
  end

  local function emit_completion_output(answer)
    if format == "json" then
      local payload = {
        answer = type(answer) == "string" and answer or "",
        tool_calls = state.tool_calls or {},
      }
      local ok, encoded = pcall(json.encode, payload)
      if ok then write_stdout(encoded .. "\n") end
    elseif format == "text" then
      write_stdout("\n")
    end
  end

  state.on_tool_started = function(name, input)
    if is_async_dispatch(name, input) then
      async_dispatch_inflight = true
      if gate then gate.suppress_stream = true end
    end
  end

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

  state.on_terminal = function(_turn_id, _status, answer, _terminal)
    if async_dispatch_inflight then
      async_dispatch_inflight = false
      if gate then gate.suppress_stream = false end
      -- Don't print or prompt yet — wait for the deferred relay turn.
      return
    end
    emit_completion_output(answer)
    read_and_submit()
  end

  wait_until_ready(read_and_submit)
end

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
  if opts.reasoning_effort ~= nil then
    agentic_workflow.set_reasoning_effort(nil, opts.reasoning_effort)
  end
  if opts.mode ~= nil then
    agentic_workflow.set_mode(opts.mode)
  end

  local state = {}
  -- Stream-suppression gate; mutated by run_single_shot / run_repl when
  -- async kernel-dispatch deferral demands holding back the first turn.
  local gate = { suppress_stream = false }

  install_conversation_output(opts.format, state, gate)
  if opts.format == "stream-json" then install_stream_json_format() end

  -- Branch on mode.
  if opts.prompt ~= nil then
    run_single_shot(opts.prompt, opts.format, state, now_ms(), gate)
  else
    run_repl(opts.format, state, gate)
  end
  return 0
end

function M.configure(opts)
  assert(type(opts) == "table", "agentic-cli.configure: options table required")
  readiness_config = opts.readiness
  assert(type(readiness_config) == "table",
    "agentic-cli.configure: readiness table required")
end

function M._parse_argv(argv) return parse_argv(argv) end
function M._usage() return USAGE end

return M
