local dispatch = require("libs.chat.dispatch")
local controller = require("libs.chat.controller")

local function submit(msg, state)
  local value = msg.value or ""
  return { value = value, seen = (state.seen or 0) + 1 }, {
    { kind = "emit", body = { kind = "house.submitted", value = value } },
  }
end

local function own_resume_done(_msg, state)
  return { value = state.value, seen = state.seen, lifecycle = "replaced" }, {}
end

local update = controller.build {
  duplicate_handlers = "replace",
  handler_groups = {
    dispatch.group("independent house", {
      ["input.submit"] = submit,
      ["sessions.resume_done"] = own_resume_done,
    }),
  },
}

tui.start {
  initial_state = { value = "independent", seen = 0 },
  view = function(state)
    return tui.widget.text { content = state.value }
  end,
  update = update,
}
