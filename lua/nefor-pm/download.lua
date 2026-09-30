local M = {}

function M.download(url, destination)
  local request = require("http.request")
  local stream, file
  local ok, err = pcall(function()
    local query = request.new_from_uri(url)
    -- A single archive transfer uses HTTP/1.1 so an unfinished stream can be detached.
    query.version = 1.1
    local headers, response, request_error = query:go()
    if not headers then error("HTTP request failed: " .. tostring(response or request_error), 0) end
    stream = response
    local status = tonumber(headers:get(":status"))
    if not status or status < 200 or status >= 300 then
      error("HTTP request returned status " .. tostring(headers:get(":status")), 0)
    end
    local open_error
    file, open_error = io.open(destination, "wb")
    if not file then error("cannot open download destination: " .. tostring(open_error), 0) end
    local saved, save_error = stream:save_body_to_file(file)
    if not saved then error("HTTP body download failed: " .. tostring(save_error), 0) end
    local length_header = headers:get("content-length")
    local length = tonumber(length_header)
    if length and file:seek() ~= length then error("HTTP body length mismatch", 0) end
    local closed, close_error = file:close()
    file = nil
    if not closed then error("cannot close download destination: " .. tostring(close_error), 0) end
  end)
  if file then
    local closed, close_error = file:close()
    if not closed then err = tostring(err) .. "\nCannot close download destination: " .. tostring(close_error) end
  end
  if stream then
    -- lua-http closes completed requests. Detaching an unfinished HTTP/1 stream
    -- avoids its shutdown loop on an early EOF with an outstanding Content-Length.
    local closed, close_error = pcall(function()
      if stream.state == "closed" then
        stream:shutdown()
      else
        local socket = stream.connection:take_socket()
        if socket then socket:close() end
      end
    end)
    if not closed then
      err = tostring(err or "HTTP stream cleanup failed") .. "\n" .. tostring(close_error)
      ok = false
    end
  end
  if not ok then error(err, 0) end
end

return M
