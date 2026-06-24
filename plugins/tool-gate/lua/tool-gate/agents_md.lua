-- Compatibility wrapper for instruction-file discovery.
--
-- The reusable primitive lives in `libs.instruction-files`; this module
-- keeps tool-gate's historical import path available for existing configs.

local instruction_files = require("libs.instruction-files")

local M = {}

M.RESULT_LIMIT = instruction_files.RESULT_LIMIT
M.INSTRUCTION_FILENAMES = instruction_files.INSTRUCTION_FILENAMES

function M.mark_read(...) return instruction_files.mark_read(...) end
function M.discover(...) return instruction_files.discover(...) end
function M.format_discovery(...) return instruction_files.format_discovery(...) end
function M.format_reminder(...) return instruction_files.format_reminder(...) end
function M.record_tool_contexts_from_advertise(...)
  return instruction_files.record_tool_contexts_from_advertise(...)
end
function M.folders_for_tool_call(...) return instruction_files.folders_for_tool_call(...) end
function M.mark_read_for_tool_call(...) return instruction_files.mark_read_for_tool_call(...) end

function M.remind_for_tool_call(chat_id, tool_name, args, emitter)
  local bodies = instruction_files.reminder_bodies_for_tool_call(chat_id, tool_name, args)
  for _, body in ipairs(bodies) do
    emitter(body)
  end
  return #bodies
end

function M._reset() return instruction_files._reset() end
function M._state(...) return instruction_files._state(...) end

M._normalise = instruction_files._normalise
M._parent_dir = instruction_files._parent_dir
M._to_absolute = instruction_files._to_absolute
M._folder_for_path = instruction_files._folder_for_path
M._git_root = instruction_files._git_root

return M
