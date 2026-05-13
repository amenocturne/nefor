-- starter/agentic_cli_test.lua — unit tests for agentic_cli's argv parser.
--
-- Loaded by `crates/nefor/tests/starter_agentic_cli_test.rs`. The Rust
-- harness installs a stub `nefor` surface (json + bus.on_event +
-- engine.exit + io.read_line + log) so `require("agentic_cli")`
-- succeeds; this file then drives the parser directly without
-- spawning anything on the bus.
--
-- We test only the parser because the full run flow needs the broker;
-- those scenarios live in the smoke-test path (Phase 3 e2e suite).

local agentic_cli = require("agentic_cli")

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected), tostring(actual)), 2)
  end
end

local function parse(...)
  return agentic_cli._parse_argv({ ... })
end

-- Empty argv → REPL mode (no prompt), text format default, no flags.
do
  local opts, err = parse()
  assert(opts ~= nil, "expected opts; got error: " .. tostring(err))
  assert_eq(err, nil, "no error on empty argv")
  assert_eq(opts.prompt, nil, "no prompt for REPL")
  assert_eq(opts.format, "text", "default format")
  assert_eq(opts.yolo, false, "yolo off by default")
  assert_eq(opts.help, false, "help off by default")
end

-- Single positional → single-shot prompt.
do
  local opts, err = parse("hello world")
  assert_eq(err, nil, "no error")
  assert_eq(opts.prompt, "hello world", "single positional")
end

-- Multiple positionals → joined with space.
do
  local opts, err = parse("hello", "world", "foo")
  assert_eq(err, nil, "no error")
  assert_eq(opts.prompt, "hello world foo", "joined positionals")
end

-- -m / --model.
do
  local opts, err = parse("-m", "gpt-5", "test prompt")
  assert_eq(err, nil, "no error")
  assert_eq(opts.model, "gpt-5", "short model flag")
  assert_eq(opts.prompt, "test prompt", "prompt parsed after flag")

  opts, err = parse("--model", "claude", "x")
  assert_eq(err, nil, "no error")
  assert_eq(opts.model, "claude", "long model flag")
end

-- --yolo.
do
  local opts, err = parse("--yolo", "do dangerous things")
  assert_eq(err, nil, "no error")
  assert_eq(opts.yolo, true, "yolo set")
  assert_eq(opts.prompt, "do dangerous things", "prompt after --yolo")
end

-- --format with each valid value.
for _, fmt in ipairs({ "text", "json", "stream-json" }) do
  local opts, err = parse("--format", fmt, "p")
  assert_eq(err, nil, "no error for format=" .. fmt)
  assert_eq(opts.format, fmt, "format=" .. fmt)
end

-- --format invalid value.
do
  local opts, err = parse("--format", "yaml", "p")
  assert_eq(opts, nil, "nil opts on bad format")
  assert(err ~= nil and string.find(err, "yaml"),
    "error mentions invalid value; got: " .. tostring(err))
end

-- Missing value for flag that requires one.
do
  local opts, err = parse("--model")
  assert_eq(opts, nil, "nil opts on missing value")
  assert(err ~= nil and string.find(err, "missing value"),
    "error mentions missing value; got: " .. tostring(err))

  opts, err = parse("-f")
  assert_eq(opts, nil, "nil opts on missing -f value")
  assert(err ~= nil and string.find(err, "missing value"),
    "error mentions missing value; got: " .. tostring(err))
end

-- -h / --help short-circuits.
do
  local opts, err = parse("-h")
  assert_eq(err, nil, "no error")
  assert_eq(opts.help, true, "help set by -h")

  opts, err = parse("--help")
  assert_eq(err, nil, "no error")
  assert_eq(opts.help, true, "help set by --help")
end

-- --debug is recognised (no-op for v1).
do
  local opts, err = parse("--debug", "x")
  assert_eq(err, nil, "no error")
  assert_eq(opts.debug, true, "debug set")
  assert_eq(opts.prompt, "x", "prompt parsed")
end

-- Unknown flag rejected.
do
  local opts, err = parse("--unknown")
  assert_eq(opts, nil, "nil opts on unknown flag")
  assert(err ~= nil and string.find(err, "unknown flag"),
    "error mentions unknown flag; got: " .. tostring(err))
end

-- `--` ends flag parsing; subsequent args are positional even if they
-- look like flags.
do
  local opts, err = parse("--", "--not-a-flag", "other")
  assert_eq(err, nil, "no error")
  assert_eq(opts.prompt, "--not-a-flag other", "positional after --")
end

-- -f / --file just stores the path (file IO happens later).
do
  local opts, err = parse("-f", "/tmp/x", "prompt")
  assert_eq(err, nil, "no error")
  assert_eq(opts.file, "/tmp/x", "file path stored")
  assert_eq(opts.prompt, "prompt", "prompt also parsed")
end

-- Combined: --yolo --format json -m gpt -f path "the prompt"
do
  local opts, err = parse("--yolo", "--format", "json", "-m", "gpt", "-f", "/p", "the prompt")
  assert_eq(err, nil, "no error on combined")
  assert_eq(opts.yolo, true, "yolo")
  assert_eq(opts.format, "json", "format")
  assert_eq(opts.model, "gpt", "model")
  assert_eq(opts.file, "/p", "file")
  assert_eq(opts.prompt, "the prompt", "prompt")
end

-- Usage text exists and mentions key flags.
do
  local usage = agentic_cli._usage()
  assert(type(usage) == "string" and #usage > 0, "usage non-empty")
  assert(string.find(usage, "%-%-format"), "usage mentions --format")
  assert(string.find(usage, "%-%-yolo"), "usage mentions --yolo")
  assert(string.find(usage, "%-%-model"), "usage mentions --model")
end

print("agentic_cli_test: all assertions passed")
