"""A stand-in for llama-server that speaks just enough HTTP for the runtime's gates.

Never llama.cpp. Like llama-server, `--api-key-file FILE` makes every request need
`Authorization: Bearer <key>` (each non-empty line of FILE is a key). It is stricter than
llama-server, which leaves /health and /v1/models open, so a probe that forgets the key fails.
`--no-slots` turns /slots off (501). The rest is set through the environment the manifest passes:

  FAKE_API_KEY_FILE   keys read as from --api-key-file, required although no flag names them
  FAKE_STARTUP_DELAY  seconds /health answers 503 before it turns 200 (default 0)
  FAKE_EXIT_CODE      exit with this code right after start (a crashing server)
  FAKE_MARKERS        "|"-separated lines printed to stdout at start
  FAKE_TPS            predicted_per_second reported by completions (default 60)
  FAKE_REASONING      "1" adds reasoning_content to completions
  FAKE_CTX_DELTA      added to the n_ctx /props reports (a wrong context)
  FAKE_REQUEST_LOG    file that receives one "METHOD path" line per request
"""

import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERSION = "version: 11200 (abcdef012)"


def option(name, default=None):
    argv = sys.argv
    return argv[argv.index(name) + 1] if name in argv else default


def read_keys(path):
    with open(path, encoding="utf-8") as stream:
        return {line for line in stream.read().split("\n") if line}


def main():
    if "--version" in sys.argv:
        print(VERSION)
        return 0
    keys = set()
    for path in (option("--api-key-file"), os.environ.get("FAKE_API_KEY_FILE")):
        if path:
            keys |= read_keys(path)
    slots = "--no-slots" not in sys.argv
    started = time.monotonic()
    delay = float(os.environ.get("FAKE_STARTUP_DELAY", "0"))
    alias = option("--alias")
    context = int(option("--ctx-size", "0"))
    request_log = os.environ.get("FAKE_REQUEST_LOG")
    tps = float(os.environ.get("FAKE_TPS", "60"))
    reasoning = os.environ.get("FAKE_REASONING") == "1"
    for line in os.environ.get("FAKE_MARKERS", "offloaded 33/33 layers to GPU").split("|"):
        print(line, flush=True)
    if "FAKE_EXIT_CODE" in os.environ:
        return int(os.environ["FAKE_EXIT_CODE"])

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reply(self, status, body):
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def note(self):
            if request_log:
                with open(request_log, "a", encoding="utf-8") as stream:
                    stream.write(f"{self.command} {self.path}\n")

        def authorized(self):
            header = self.headers.get("Authorization", "")
            if not keys or (header.startswith("Bearer ") and header[7:] in keys):
                return True
            self.reply(401, {"error": {"code": 401, "message": "Invalid API Key"}})
            return False

        def do_GET(self):
            self.note()
            if not self.authorized():
                return
            if self.path == "/slots":
                self.reply(200 if slots else 501, [] if slots else {})
            elif self.path == "/health":
                loading = time.monotonic() - started < delay
                self.reply(503 if loading else 200, {"status": "loading" if loading else "ok"})
            elif self.path == "/v1/models":
                self.reply(200, {"object": "list", "data": [{"id": alias}]})
            elif self.path == "/props":
                n_ctx = context + int(os.environ.get("FAKE_CTX_DELTA", "0"))
                self.reply(200, {"default_generation_settings": {"n_ctx": n_ctx}})
            else:
                self.reply(404, {})

        def do_POST(self):
            self.note()
            self.rfile.read(int(self.headers.get("Content-Length", "0")))
            if not self.authorized():
                return
            message = {"role": "assistant", "content": "def fib(n): ..."}
            if reasoning:
                message["reasoning_content"] = "thinking..."
            self.reply(
                200,
                {"choices": [{"message": message}], "timings": {"predicted_per_second": tps}},
            )

    server = ThreadingHTTPServer((option("--host", "127.0.0.1"), int(option("--port"))), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    while True:
        time.sleep(1)


if __name__ == "__main__":
    raise SystemExit(main())
