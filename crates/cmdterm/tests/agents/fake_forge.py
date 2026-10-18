#!/usr/bin/env python3
"""A git forge and npm registry for the sandbox tests of the agent harness.

It serves the bare repositories under FORGE_ROOT over git's smart HTTP
protocol (through `git http-backend`) and takes `npm publish` uploads.
Every request needs the real credentials: `Basic` with
FORGE_USER:FORGE_GIT_TOKEN for git, `Bearer FORGE_NPM_TOKEN` for npm. A
request without credentials gets 401, one with others (such as a fake a
sandbox holds) 403.

Every request is appended to $FORGE_STATE/forge.jsonl with its method,
path, the credentials it carried and the status it got, so a test can
check what reached the forge. The port goes to $FORGE_STATE/port once the
server listens.

Usage: fake_forge.py ADDRESS (such as 127.0.0.2)
"""

import base64
import json
import os
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.environ["FORGE_ROOT"]
STATE = os.environ["FORGE_STATE"]
USER = os.environ["FORGE_USER"]
GIT_TOKEN = os.environ["FORGE_GIT_TOKEN"]
NPM_TOKEN = os.environ["FORGE_NPM_TOKEN"]
LOCK = threading.Lock()


def record(entry):
    with LOCK:
        with open(os.path.join(STATE, "forge.jsonl"), "a") as log:
            log.write(json.dumps(entry) + "\n")


def basic_user(header):
    """The `user:password` of a Basic header, or None."""
    if not header.lower().startswith("basic "):
        return None
    try:
        return base64.b64decode(header[6:].strip()).decode()
    except ValueError:
        return None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def body(self):
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            data = b""
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip() or b"0", 16)
                if size == 0:
                    self.rfile.readline()
                    return data
                data += self.rfile.read(size)
                self.rfile.readline()
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def reply(self, status, body=b"", headers=()):
        self.send_response(status)
        for key, value in headers:
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def handle_any(self):
        body = self.body()
        auth = self.headers.get("Authorization", "")
        npm = self.headers.get("npm-command") is not None or "npm" in self.headers.get(
            "User-Agent", "")
        entry = {"method": self.command, "path": self.path, "auth": auth,
                 "basic": basic_user(auth), "npm": npm}
        if npm:
            ok = auth == f"Bearer {NPM_TOKEN}"
        else:
            ok = entry["basic"] == f"{USER}:{GIT_TOKEN}"
        if not auth:
            entry["status"] = 401
            record(entry)
            return self.reply(401, b"credentials needed\n",
                              [("WWW-Authenticate", 'Basic realm="forge"')])
        if not ok:
            entry["status"] = 403
            record(entry)
            return self.reply(403, b"wrong credentials\n")
        if npm:
            entry["status"] = 200 if self.command == "PUT" else 404
            record(entry)
            if self.command == "PUT":
                return self.reply(200, b'{"ok":true}', [("Content-Type", "application/json")])
            return self.reply(404, b'{"error":"not found"}', [("Content-Type", "application/json")])
        status = self.git(body)
        entry["status"] = status
        record(entry)

    def git(self, body):
        path, _, query = self.path.partition("?")
        env = dict(os.environ,
                   GIT_PROJECT_ROOT=ROOT,
                   GIT_HTTP_EXPORT_ALL="1",
                   REMOTE_USER=USER,
                   REQUEST_METHOD=self.command,
                   PATH_INFO=path,
                   QUERY_STRING=query,
                   CONTENT_TYPE=self.headers.get("Content-Type", ""),
                   CONTENT_LENGTH=str(len(body)),
                   HTTP_CONTENT_ENCODING=self.headers.get("Content-Encoding", ""),
                   GIT_PROTOCOL=self.headers.get("Git-Protocol", ""))
        out = subprocess.run(["git", "http-backend"], input=body, env=env,
                             capture_output=True).stdout
        head, _, rest = out.partition(b"\r\n\r\n")
        if not rest and b"\n\n" in out:
            head, _, rest = out.partition(b"\n\n")
        status = 200
        headers = []
        for line in head.decode(errors="replace").splitlines():
            key, _, value = line.partition(":")
            if key.lower() == "status":
                status = int(value.split()[0])
            elif key:
                headers.append((key, value.strip()))
        self.reply(status, rest, headers)
        return status

    do_GET = do_POST = do_PUT = do_DELETE = handle_any


if __name__ == "__main__":
    os.makedirs(STATE, exist_ok=True)
    server = ThreadingHTTPServer((sys.argv[1], 0), Handler)
    with open(os.path.join(STATE, "port.tmp"), "w") as f:
        f.write(str(server.server_address[1]))
    os.replace(os.path.join(STATE, "port.tmp"), os.path.join(STATE, "port"))
    server.serve_forever()
