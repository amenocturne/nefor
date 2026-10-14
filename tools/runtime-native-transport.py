"""Exercise relocated Lua HTTPS with explicit CA trust, not build-host paths."""
import argparse
import http.server
import json
import os
from pathlib import Path
import ssl
import subprocess
import threading


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("engine", type=Path)
    parser.add_argument("runtime", type=Path)
    parser.add_argument("scratch", type=Path)
    parser.add_argument("--public-system-trust", action="store_true")
    args = parser.parse_args()
    engine, runtime, scratch = args.engine.resolve(), args.runtime.resolve(), args.scratch.resolve()
    scratch.mkdir(parents=True, exist_ok=False)
    certificate = scratch / "certificate.pem"
    openssl_config = scratch / "openssl.cnf"
    openssl_config.write_text("[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN=localhost\n[ext]\nsubjectAltName=DNS:localhost\nbasicConstraints=critical,CA:TRUE\n")
    subprocess.run(["/usr/bin/openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-keyout", str(scratch / "key.pem"), "-out", str(certificate), "-config", str(openssl_config)],
                   check=True, capture_output=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certificate, scratch / "key.pem")
    server.socket = context.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    config = scratch / "config"
    config.mkdir()
    tree = runtime / "lua-dependencies"
    config.joinpath("init.lua").write_text(f"""
package.path = {json.dumps(str(tree))} .. '/share/lua/5.4/?.lua;' ..
  {json.dumps(str(tree))} .. '/share/lua/5.4/?/init.lua;' ..
  {json.dumps(str(runtime / 'lua'))} .. '/?.lua;' ..
  {json.dumps(str(runtime / 'lua'))} .. '/?/init.lua'
package.cpath = {json.dumps(str(tree))} .. '/lib/lua/5.4/?.so'
local query = require('http.request').new_from_uri(assert(os.getenv('TRANSPORT_URL')))
query.version = 1.1
local headers, stream, err = query:go(20)
if os.getenv('EXPECT_REJECT') then
  assert(not headers, 'accepted untrusted TLS peer')
  print('untrusted TLS rejected: ' .. tostring(err))
else
  assert(headers, tostring(err))
  assert(headers:get(':status') == '200', headers:get(':status'))
  assert(stream:get_body_as_string())
  print('verified HTTPS passed')
end
function dispatch(_) end
""")
    profile = '(version 1)(allow default)(deny file-read* (subpath "/nix/store") (subpath "/opt/homebrew"))'
    try:
        cases = [(str(certificate), False, f"https://localhost:{server.server_port}/"),
                 (str(scratch / "absent.pem"), True, f"https://localhost:{server.server_port}/")]
        if args.public_system_trust:
            cases.append(("/etc/ssl/cert.pem", False, "https://example.com/"))
        for ca, reject, url in cases:
            environment = {"HOME": str(scratch), "PATH": "/usr/bin:/bin", "SSL_CERT_FILE": ca,
                           "SSL_CERT_DIR": str(scratch / "empty-ca-directory"), "TRANSPORT_URL": url}
            if reject:
                environment["EXPECT_REJECT"] = "1"
            result = subprocess.run(["/usr/bin/sandbox-exec", "-p", profile, str(engine), "--config", str(config),
                                     "--data-dir", str(scratch / "data"), "plugin"], env=environment,
                                    capture_output=True, text=True, timeout=40)
            if result.returncode or ("untrusted TLS rejected" if reject else "verified HTTPS passed") not in result.stdout:
                raise RuntimeError(f"TLS gate failed: {result.stdout}\n{result.stderr}")
            print(f"{url}: {result.stdout.strip()} (Nix/Homebrew reads denied)")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    main()
