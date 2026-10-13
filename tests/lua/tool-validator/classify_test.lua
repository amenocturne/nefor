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

local validator = tv.build { classifier = FAKE_CLASSIFIER }

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

-- Each policy has its own flags: the wide Default stack approves a commit,
-- the strict ReadOnly stack does not.
eq(classify(validator, "Default", "git commit -m x"), "safe", "Default uses the wide stack")
eq(classify(validator, "ReadOnly", "git commit -m x"), "unsure", "ReadOnly uses the strict stack")

-- The model flag is a configuration value and is added to Default only.
do
  eq(classify(validator, "Default", "MODEL decides"), "unsure", "without a configured model flag nothing asks the model")
  local with_model = tv.build { classifier = FAKE_CLASSIFIER, model_flag = "--model" }
  eq(classify(with_model, "Default", "MODEL decides"), "safe", "Default gets the configured model flag")
  eq(classify(with_model, "ReadOnly", "MODEL decides"), "unsure", "ReadOnly never gets the model flag")
  eq(classify(with_model, "Default", "git commit -m x"), "safe", "the flag is added to the wide stack, not instead of it")
end

-- A configuration may replace a policy's flags outright.
do
  local custom = tv.build {
    classifier = FAKE_CLASSIFIER,
    classifier_args = { ReadOnly = { "--git", "read,commit" } },
    model_flag = "--model",
  }
  eq(classify(custom, "ReadOnly", "git commit -m x"), "safe", "configured flags replace the default stack")
  eq(classify(custom, "Default", "MODEL decides"), "safe", "an untouched policy keeps its default stack")
  eq(pcall(tv.build, { classifier = FAKE_CLASSIFIER, classifier_args = { Rules = {} } }), false,
    "flags for an unknown policy are a configuration error")
  eq(pcall(tv.build, { classifier = FAKE_CLASSIFIER, model_flag = "" }), false,
    "an empty model flag is a configuration error")
end

-- A classifier that fails denies with an actionable reason, under every policy.
for _, policy in ipairs({ "ReadOnly", "Default" }) do
  local result, reason = classify(validator, policy, "CRASH now")
  eq(result, "denied", "an unexpected exit status is a denial under " .. policy)
  assert(reason:find("tool_classifier_unavailable", 1, true), "the reason is recognisable: " .. reason)
  assert(reason:find("exit code 70", 1, true), "it carries the exit status: " .. reason)
  assert(reason:find("classifier exploded", 1, true), "and the classifier's own message: " .. reason)
  assert(reason:find("Recovery:", 1, true), "and what to do: " .. reason)
end
do
  local missing = tv.build { classifier = "/nonexistent/classifier" }
  local result, reason = classify(missing, "Default", "ls")
  eq(result, "denied", "an unusable classifier is a denial")
  assert(reason:find("tool_classifier_unavailable", 1, true), "the reason is recognisable: " .. reason)
  assert(reason:find("/nonexistent/classifier", 1, true), "it names the configured executable: " .. reason)
  eq(pcall(tv.build, {}), false, "a classifier executable is required")
end

-- No text, an unknown policy, and fast paths.
eq(classify(validator, "Default", ""), "unsure", "an empty command is unsure")
eq(classify(validator, "ReadOnly", nil), "unsure", "a missing command is unsure")
eq(pcall(validator.classify_command, "Rules", "ls"), false, "an unknown policy raises")
do
  local seen = {}
  local fast = tv.build { classifier = "/nonexistent/classifier", command_fastpaths = {
    function(command, policy)
      seen[#seen + 1] = policy .. ":" .. command
      return policy == "Default" and command == "make lint"
    end,
  } }
  eq(classify(fast, "Default", "make lint"), "safe", "a fast path approves before the classifier is consulted")
  eq(classify(fast, "ReadOnly", "make lint"), "denied", "a fast path decides per policy")
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
  local result, reason = validator.classify_argv("Default", {})
  eq(result, "denied", "an empty vector cannot run")
  eq(reason, "argv is empty", "and says so")
end

return true
