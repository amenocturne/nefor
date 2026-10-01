local sessions = require("libs.sessions")
local state = sessions._internals.state
local original_open = io.open
local json = nefor.json
local row = json.encode({ origin = "step", payload = json.encode({
  from = "test", body = { kind = "fixture.replayed" },
}) })

for _, fault in ipairs({ "open", "initial_seek", "size", "reset", "writer_open", "step_seek", "loop_seek", "after_seek", "final_seek", "read", "eof", "stale" }) do
  sessions._internals.reset_state()
  sessions.configure { root = os.getenv("NEFOR_DATA_DIR") .. "/sessions" }
  sessions.init()
  local old_id = sessions.current_id()
  local sent, deferred, handles = {}, {}, {}
  local opens, reads = 0, 0
  nefor.engine.send = function(payload) sent[#sent + 1] = json.decode(payload).body end
  nefor.engine.defer = function(callback) deferred[#deferred + 1] = callback end
  io.open = function(path, mode)
    if not path:find("fault-target", 1, true) then return original_open(path, mode) end
    if mode == "a" then
      if fault == "writer_open" then return nil, "fixture writer EIO" end
      return { close = function() end, flush = function() return true end,
        write = function() return true end }
    end
    opens = opens + 1
    if opens == 2 and fault == "open" then return nil, "fixture reopen EIO" end
    local is_reader = opens == 2
    local handle = { offset = 0, closed = 0, seeks = 0 }
    handles[#handles + 1] = handle
    function handle:close() self.closed = self.closed + 1 end
    function handle:seek(whence, offset)
      assert(self.closed == 0, "closed reader used")
      self.seeks = self.seeks + 1
      if is_reader then
        if fault == "initial_seek" and self.seeks == 1
          or fault == "size" and whence == "end"
          or fault == "reset" and whence == "set"
          or fault == "step_seek" and self.seeks == 4
          or fault == "loop_seek" and self.seeks == 5
          or fault == "after_seek" and self.seeks == 6
          or fault == "final_seek" and self.seeks == 133 then
          return nil, "fixture seek EIO"
        end
      end
      if whence == "end" then self.offset = (#row + 1) * 65 end
      if whence == "set" then self.offset = offset end
      return self.offset
    end
    function handle:read()
      assert(self.closed == 0, "closed reader used")
      if not is_reader then return "header" end
      reads = reads + 1
      -- Fail after one delivered cooperative chunk, not just on first read.
      if reads == 65 and fault ~= "stale" then
        return nil, fault == "read" and "fixture read EIO" or nil
      end
      self.offset = self.offset + #row + 1
      return row
    end
    return handle
  end
  local target = "fault-target"
  local result = sessions.resume(target, true, "fault-request")
  local preactivation = fault == "open" or fault == "initial_seek" or fault == "size" or fault == "reset" or fault == "writer_open"
  assert(sessions.current_id() == (preactivation and old_id or target), fault)
  assert((result == nil) == preactivation, fault)
  local callbacks = 0
  local stale
  while #deferred > 0 do
    callbacks = callbacks + 1
    assert(callbacks <= 3, "unbounded continuation: " .. fault)
    local callback = table.remove(deferred, 1)
    if fault == "stale" then
      stale = callback
      sessions.new()
    else
      stale = callback
    end
    callback()
  end
  -- A duplicate already queued callback must be inert after settlement.
  local count = #sent
  if stale then stale() end
  assert(#sent == count, "stale continuation emitted: " .. fault)
  assert(state.replay_session_id == nil and state.replay_file == nil, fault)
  assert(not state.in_replay_window and not require("core.replay_window").active(), fault)
  for _, handle in ipairs(handles) do assert(handle.closed == 1, fault) end
  local failures = 0
  for _, body in ipairs(sent) do
    assert(body.kind ~= "sessions.resume_done", "false success: " .. fault)
    if body.kind == "sessions.transition_failed" then
      failures = failures + 1
      assert(body.operation == "resume" and body.request_id == "fault-request"
        and body.session_id == target, fault)
      if not preactivation then assert(body.message:find("incomplete", 1, true), fault) end
      if fault == "read" or fault == "open" then assert(body.message:find("EIO", 1, true), fault) end
    end
  end
  assert(failures == (fault == "stale" and 0 or 1), fault)
  io.open = original_open
  -- Settlement does not poison a subsequent ordinary replay.
  local file = assert(io.open(sessions.current_path(), "w"))
  file:write(row .. "\n"); file:close()
  sent = {}
  sessions.resume(sessions.current_id(), true, "next-request")
  while #deferred > 0 do table.remove(deferred, 1)() end
  assert(sent[#sent].kind == "sessions.resume_done" and sent[#sent].replayed == 1, fault)
  assert(sessions.ready(), fault)
end
io.open = original_open
sessions._internals.reset_state()
