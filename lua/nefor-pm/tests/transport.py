"""Exercise the installed external Lua HTTP stack inside the real engine."""
import argparse
import http.server
import json
import os
from pathlib import Path
import subprocess
import threading
from urllib.parse import urlsplit


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_GET(self):
        path = urlsplit(self.path).path
        if path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/payload")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if path == "/failure":
            self.send_response(503)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        body = b"external Lua transport"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body) + (100 if path == "/partial" else 0)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()
        if path == "/partial":
            self.close_connection = True


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--engine", required=True, type=Path)
    parser.add_argument("--tree", required=True, type=Path)
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--scratch", required=True, type=Path)
    args = parser.parse_args()
    args.scratch.mkdir(parents=True, exist_ok=True)
    config = args.scratch / "config"
    config.mkdir(exist_ok=True)
    lua = args.repo / "lua"
    tree = args.tree
    config.joinpath("init.lua").write_text(f"""
io.stdout:setvbuf("no")
package.path = {json.dumps(str(lua))} .. "/?.lua;" .. {json.dumps(str(lua))} .. "/?/init.lua;" ..
  {json.dumps(str(tree))} .. "/share/lua/5.4/?.lua;" .. {json.dumps(str(tree))} .. "/share/lua/5.4/?/init.lua;" .. package.path
package.cpath = {json.dumps(str(tree))} .. "/lib/lua/5.4/?.so;" .. package.cpath
local download = require("nefor-pm.download").download
local base = assert(os.getenv("TRANSPORT_BASE"))
local destination = {json.dumps(str(args.scratch / "body"))}
for _, path in ipairs({{"/payload", "/redirect"}}) do
  print("download " .. path)
  download(base .. path, destination)
  local file = assert(io.open(destination, "rb"))
  assert(file:read("*a") == "external Lua transport")
  assert(file:close())
end
for _, path in ipairs({{"/failure", "/partial"}}) do
  print("reject " .. path)
  local ok, err = pcall(download, base .. path, destination)
  assert(not ok, "accepted failed response " .. path)
  assert(type(err) == "string")
  if path == "/failure" then assert(err:find("503", 1, true), err) end
end
function dispatch(_) end
print("external transport passed")
""")
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        base = f"http://127.0.0.1:{server.server_port}"
        for proxied in (False, True):
            env = {key: value for key, value in os.environ.items()
                   if key.lower() not in ("http_proxy", "https_proxy", "all_proxy", "no_proxy")}
            env["TRANSPORT_BASE"] = "http://fixture.invalid" if proxied else base
            if proxied:
                env["http_proxy"] = base
            try:
                result = subprocess.run(
                    [str(args.engine), "--config", str(config), "--data-dir",
                     str(args.scratch / "data"), "plugin"],
                    env=env, capture_output=True, text=True, timeout=45,
                )
            except subprocess.TimeoutExpired as error:
                raise RuntimeError(f"transport timeout (proxy={proxied}): {error.stdout!r} {error.stderr!r}") from error
            if result.returncode or "external transport passed" not in result.stdout:
                raise RuntimeError(f"transport failed (proxy={proxied}): {result.stdout} {result.stderr}")
            print(f"HTTP redirect, status, incomplete body passed (proxy={proxied})")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    main()
