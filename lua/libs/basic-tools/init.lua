-- Model-facing tools over the basic-tools capability plugin.
--
-- The `basic-tools` process executes capabilities; these definitions own what
-- a model sees and calls: names, descriptions, closed parameter schemas,
-- display contracts, and `context` folder metadata. Each handler forwards its
-- validated arguments to the capability of the same name.
--
--   require("libs.basic-tools").tools {
--     max_read_bytes = N,                    -- read_file cap, same as the process flag
--     invoke = function(call, capability, args) end,
--     cancel = function(call) end,           -- process capabilities only
--   } -> { tool definitions }
--
-- `libs.compositors.tools.basic_actor_spec` supplies invoke/cancel for the
-- wrapped process.

local tool = require("libs.tool")
local tool_display = require("libs.chat.tool_display")

local M = {}

local function field(label, source, path, kind, extra)
  local value = { label = label, select = { source = source, path = path }, kind = kind }
  for key, item in pairs(extra or {}) do value[key] = item end
  return value
end

local function display(label, primary, fields, result)
  return {
    compact = { label = label, primary = primary },
    expanded = { label = label, fields = tool_display.fields(fields) },
    result = result,
  }
end

local function file_path_context(cwd_arg)
  return { folders = { { from = "file_path", arg = "path", cwd_arg = cwd_arg } } }
end

local CWD_CONTEXT = { folders = { { from = "cwd", arg = "cwd", default = "." } } }

local function timeout_schema()
  return {
    type = "object",
    properties = {
      present = { type = "boolean" },
      milliseconds = { type = "integer", minimum = 0 },
    },
    required = { "present", "milliseconds" },
    additionalProperties = false,
  }
end

function M.tools(opts)
  if type(opts) ~= "table" then error("basic-tools.tools: options must be a table", 2) end
  local max_read_bytes = opts.max_read_bytes
  if math.type(max_read_bytes) ~= "integer" or max_read_bytes < 1 then
    error("basic-tools.tools: max_read_bytes must be a positive integer", 2)
  end
  local invoke, cancel = opts.invoke, opts.cancel
  if type(invoke) ~= "function" or type(cancel) ~= "function" then
    error("basic-tools.tools: invoke and cancel must be functions", 2)
  end

  local function forward(capability)
    return function(args, call) invoke(call, capability, args) end
  end

  return {
    tool.define {
      name = "read_file",
      description = "Read the UTF-8 contents of a regular file. Returns the file's text content or an error.",
      parameters = {
        type = "object",
        properties = {
          path = { type = "string", description = "Absolute or relative path to the file." },
          cwd = { type = "string", description = "Working directory. Relative paths are resolved against this." },
          offset = { type = "integer", description = "Byte offset to start reading from. Optional; defaults to 0." },
          max_bytes = {
            type = "integer",
            description = "Maximum bytes to read. Optional; maximum " .. max_read_bytes .. " bytes.",
            maximum = max_read_bytes,
          },
        },
        required = { "path" },
      },
      display = display("read file", field("path", "args", "path", "path"), {
        field("offset", "args", "offset", "scalar", { omit = "missing" }),
        field("max bytes", "args", "max_bytes", "bytes", { omit = "missing" }),
      }, { kind = "content", fields = tool_display.fields({
        field("content", "result", "$", "text", { max_lines = 20, max_bytes = 1600 }),
      }) }),
      context = file_path_context("cwd"),
      handler = forward("read_file"),
    },
    tool.define {
      name = "read_image",
      description = "Read a regular image file for visual inspection. Returns image bytes and metadata; only vision-capable models can use the result.",
      parameters = {
        type = "object",
        properties = {
          path = { type = "string", description = "Absolute or relative path to the image file." },
          cwd = { type = "string", description = "Working directory. Relative paths are resolved against this." },
        },
        required = { "path" },
      },
      display = display("read image", field("path", "args", "path", "path"), {},
        { kind = "receipt", text = "image loaded", fields = tool_display.fields({
          field("media type", "result", "media_type", "scalar", { omit = "missing" }),
          field("filename", "result", "filename", "path", { omit = "missing" }),
          field("encoded bytes", "result", "data", "bytes", { omit = "missing" }),
        }) }),
      context = file_path_context("cwd"),
      handler = forward("read_image"),
    },
    tool.define {
      name = "write_file",
      description = "Write a complete text file, or edit an existing text file by replacing one exact unique string. Without old_string, new_string becomes the complete file contents and existing content is overwritten. With old_string, exactly one match is replaced. Empty new_string is allowed.",
      parameters = {
        type = "object",
        properties = {
          path = { type = "string", description = "Absolute or relative path to the destination file." },
          new_string = {
            type = "string",
            description = "New UTF-8 text. Without old_string this is the complete file content; with old_string this is the replacement text. May be empty.",
          },
          old_string = {
            type = "string",
            description = "Optional exact text to replace in an existing UTF-8 file. When present it must be non-empty and occur exactly once.",
          },
        },
        required = { "path", "new_string" },
        additionalProperties = false,
      },
      display = display("write file", field("path", "args", "path", "path"), {},
        { kind = "receipt", text = "file written", fields = tool_display.fields({}) }),
      context = file_path_context(nil),
      handler = forward("write_file"),
    },
    tool.define {
      name = "process.exec",
      description = "Execute a program from a structured argv without shell interpretation.",
      parameters = {
        type = "object",
        properties = {
          argv = {
            type = "array",
            items = { type = "string" },
            minItems = 1,
            description = "Program argv. argv[0] is the executable.",
          },
          cwd = { type = "string", minLength = 1 },
          timeout = timeout_schema(),
          stdin = { type = "string" },
        },
        required = { "argv", "cwd", "timeout" },
      },
      display = display("execute process", field("argv", "args", "argv", "scalar"), {},
        { kind = "content", fields = tool_display.fields({}) }),
      context = CWD_CONTEXT,
      handler = forward("process.exec"),
      cancel = cancel,
    },
    tool.define {
      name = "shell.script",
      description = "Execute a script through /bin/sh -c.",
      parameters = {
        type = "object",
        properties = {
          script = { type = "string" },
          cwd = { type = "string", minLength = 1 },
          timeout = timeout_schema(),
          stdin = { type = "string" },
        },
        required = { "script", "cwd", "timeout" },
      },
      display = display("execute shell script", field("script", "args", "script", "scalar"), {},
        { kind = "content", fields = tool_display.fields({}) }),
      context = CWD_CONTEXT,
      handler = forward("shell.script"),
      cancel = cancel,
    },
  }
end

return M
