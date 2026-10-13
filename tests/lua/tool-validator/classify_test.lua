-- Command classification per approval policy, against the fake classifier
-- beside this file, and the quoting that turns an argument vector into the
-- command text that is classified. Driven by
-- engine/tests/starter_tool_validator_test.rs, which sets FAKE_CLASSIFIER and
-- backs `nefor.process.run` with real processes.

local tv = require("libs.tool-validator")
local shell = require("core.shell")

local function eq(actual, expected, label)
  if actual ~= expected then
    error(string.format("%s:\n  expected %s\n  got      %s", label,
      tostring(expected), tostring(actual)), 2)
  end
end

local function classify(validator, policy, command)
  local result, reason = validator.classify_command(policy, command)
  return result, reason
end

-- The flags of the configuration under test: one stack per policy.
local ARGS = {
  ReadOnly = { "--read-only", "--git", "read" },
  Default = { "--read-only", "--git", "read,add,commit", "--model" },
}
local function build(overrides)
  local opts = { classifier = FAKE_CLASSIFIER, classifier_args = ARGS, command_fastpaths = {} }
  for key, value in pairs(overrides or {}) do opts[key] = value end
  return tv.build(opts)
end
local validator = build()

-- A failed classification raises with this message.
local function fails(v, policy, command)
  local ok, err = pcall(v.classify_command, policy, command)
  eq(ok, false, "classifying must fail: " .. tostring(command))
  return tostring(err)
end

-- The exit contract: 0 approve, 1 defer, 2 deny.
eq(classify(validator, "ReadOnly", "ls -la"), "safe", "exit 0 is safe")
do
  local result, reason = classify(validator, "ReadOnly", "frobnicate")
  eq(result, "unsure", "exit 1 is unsure")
  eq(reason, "not shown to be read-only", "ReadOnly words what could not be shown")
  result, reason = classify(validator, "Default", "frobnicate")
  eq(result, "unsure", "exit 1 is unsure under Default too")
  eq(reason, "not shown to be safe", "Default words what could not be shown")
  result, reason = classify(validator, "Default", "rm -rf build")
  eq(result, "denied", "exit 2 is denied")
  eq(reason, "forbidden by the command classifier", "a denial says who forbade it")
  eq(classify(validator, "ReadOnly", "rm -rf build"), "denied", "exit 2 is denied under ReadOnly")
end

-- Each policy gets exactly its configured flags: Default's stack approves a
-- commit and carries the model flag, ReadOnly's does neither.
eq(classify(validator, "Default", "git commit -m x"), "safe", "Default uses its stack")
eq(classify(validator, "ReadOnly", "git commit -m x"), "unsure", "ReadOnly uses its stack")
eq(classify(validator, "Default", "MODEL decides"), "safe", "Default's flags include the model flag")
eq(classify(validator, "ReadOnly", "MODEL decides"), "unsure", "ReadOnly's do not")

-- Nothing is supplied for what the configuration left out.
for _, case in ipairs({
  { { classifier_args = false }, "`classifier_args` is not defined; it maps every approval policy (ReadOnly, Default)" },
  { { classifier_args = { ReadOnly = {} } }, "`classifier_args` does not define the Default policy" },
  { { classifier_args = { Default = {} } }, "`classifier_args` does not define the ReadOnly policy" },
  { { classifier_args = { ReadOnly = {}, Default = {}, Rules = {} } }, "names unknown approval policy `Rules`" },
  { { classifier_args = { ReadOnly = { 1 }, Default = {} } }, "`classifier_args.ReadOnly` must be a list of strings" },
  { { command_fastpaths = false }, "`command_fastpaths` is not defined; pass a list of predicates, or {} for none" },
  { { classifier = false }, "requires a non-empty `classifier` executable path" },
}) do
  local overrides = {}
  for key, value in pairs(case[1]) do overrides[key] = value end
  local opts = { classifier = FAKE_CLASSIFIER, classifier_args = ARGS, command_fastpaths = {} }
  for key, value in pairs(overrides) do opts[key] = value or nil end
  local ok, err = pcall(tv.build, opts)
  eq(ok, false, "an incomplete configuration fails at build: " .. case[2])
  assert(tostring(err):find(case[2], 1, true), tostring(err))
end

-- Failing to classify is an error, never a decision.
for _, policy in ipairs({ "ReadOnly", "Default" }) do
  local message = fails(validator, policy, "CRASH now")
  assert(message:find("the command could not be classified", 1, true), message)
  assert(message:find("unknown status 70", 1, true), "it carries the exit status: " .. message)
  assert(message:find("classifier exploded", 1, true), "and the classifier's own message: " .. message)
  assert(message:find("Recovery:", 1, true), "and what to do: " .. message)
end
do
  local message = fails(build({ classifier = "/nonexistent/classifier" }), "Default", "ls")
  assert(message:find("the command could not be classified", 1, true), message)
  assert(message:find("/nonexistent/classifier", 1, true), "it names the configured executable: " .. message)
  eq(fails(validator, "Default", ""), "there is no command text to classify", "an empty command is an error")
  eq(fails(validator, "ReadOnly", nil), "there is no command text to classify", "a missing command is an error")
  assert(fails(validator, "Rules", "ls"):find("unknown approval policy `Rules`", 1, true), "an unknown policy is an error")
end

-- Fast paths.
do
  local seen = {}
  local fast = build { classifier = "/nonexistent/classifier", command_fastpaths = {
    function(command, policy)
      seen[#seen + 1] = policy .. ":" .. command
      return policy == "Default" and command == "make lint"
    end,
  } }
  eq(classify(fast, "Default", "make lint"), "safe", "a fast path approves before the classifier is consulted")
  eq(pcall(fast.classify_command, "ReadOnly", "make lint"), false, "a fast path decides per policy")
  eq(seen[1], "Default:make lint", "the predicate sees the command and the policy")
end

-- Argument vectors: every element is single-quoted, the program included.
local function quoted(argv) return (shell.quote_argv(argv)) end
eq(quoted({ "ls", "-la" }), "'ls' '-la'", "plain arguments")
eq(quoted({ "cat", "my file.txt" }), "'cat' 'my file.txt'", "spaces stay inside one word")
eq(quoted({ "echo", "it's" }), [['echo' 'it'\''s']], "a quote is closed, escaped and reopened")
eq(quoted({ "echo", "a;rm -rf /" }), "'echo' 'a;rm -rf /'", "a separator is data")
eq(quoted({ "echo", "$(reboot)", "`id`", "$HOME" }), "'echo' '$(reboot)' '`id`' '$HOME'", "expansions are data")
eq(quoted({ "FOO=bar", "env" }), "'FOO=bar' 'env'", "a leading assignment is a program name, not an assignment")
eq(quoted({ "printf", "" }), "'printf' ''", "an empty argument stays an argument")
eq(quoted({ "printf", "a\nb" }), "'printf' 'a\nb'", "a newline stays inside its word")
eq(quoted({ "-n" }), "'-n'", "a lone program")
for _, case in ipairs({
  { {}, "argv is empty" },
  { "ls -la", "argv must be a list of strings" },
  { { "ls", 3 }, "argv[2] must be a string" },
  { { [1] = "ls", [3] = "x" }, "argv[2] must be a string" },
  { { "ls", "a\0b" }, "argv[2] contains a NUL byte" },
  { { name = "ls" }, "argv must be a list of strings" },
}) do
  local command, problem = shell.quote_argv(case[1])
  eq(command, nil, "a malformed vector has no command text")
  eq(problem, case[2], "and says why")
end

-- The shell reads the quoted text back as exactly the original arguments.
do
  local argv = { "printf", "%s|", "my file", "it's", "a;b", "$(echo no)", "FOO=bar", "", "x\ny", "*" }
  local result = nefor.process.run { cmd = "/bin/sh", args = { "-c", quoted(argv) } }
  eq(result.code, 0, "the quoted command runs")
  eq(result.stdout, "my file|it's|a;b|$(echo no)|FOO=bar||x\ny|*|", "every argument arrives unchanged")
  local assigned = nefor.process.run { cmd = "/bin/sh", args = { "-c", quoted({ "FOO=bar", "env" }) } }
  eq(assigned.code, 127, "a leading NAME=value is run as a program, which does not exist")
end

-- classify_argv classifies that text.
eq(validator.classify_argv("ReadOnly", { "ls", "-la" }), "safe", "a read-only program is safe")
eq(validator.classify_argv("ReadOnly", { "rm", "-rf", "build" }), "denied", "a forbidden program is denied")
eq(validator.classify_argv("Default", { "frobnicate", "; ls" }), "unsure", "quoted separators do not make a second command")
do
  local ok, err = pcall(validator.classify_argv, "Default", {})
  eq(ok, false, "an empty vector cannot be classified")
  eq(tostring(err), "the argument vector cannot be classified: argv is empty", "and says so")
end

return true
