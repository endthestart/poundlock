"""A deterministic OpenAI-compatible model server for the end-to-end suite.

Standard library only. Each model ID is one behaviour, so a test picks what the
upstream does by naming a model:

- mock-chat: answers "pong: <last user text>"; streams when asked.
- mock-tools: returns one tool call to the first offered tool, with nested,
  non-ASCII arguments that must arrive unchanged.
- mock-error: a provider error whose message carries an address and a key.
- mock-slow: waits longer than the suite's timeout before answering.
- mock-cut: starts a stream, then drops the connection without finishing.
- mock-coder: the durable worker's model. The first turn writes greeting.txt
  with the `write` tool; any later turn says it is finished.

GET /_requests returns every request body received, newest last, so a test can
check what Poundlock actually sent upstream.
"""

import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODELS = ("mock-chat", "mock-tools", "mock-error", "mock-slow", "mock-cut", "mock-coder")
TOOL_ARGUMENTS = {"path": "notes/übersicht.md", "lines": [1, 2], "meta": {"quote": "a \"b\" c"}}
SLOW_SECONDS = 30
seen = []
lock = threading.Lock()


def chunk(model, delta, finish=None):
    return json.dumps({"id": "mock", "object": "chat.completion.chunk", "created": 1,
                       "model": model, "choices": [{"index": 0, "delta": delta,
                                                    "finish_reason": finish}]})


def reply(body):
    """The assistant message and finish reason for one request."""
    model = body.get("model")
    messages = body.get("messages") or []
    if model == "mock-tools":
        name = (body.get("tools") or [{"function": {"name": "none"}}])[0]["function"]["name"]
        call = {"id": "call-1", "type": "function",
                "function": {"name": name, "arguments": json.dumps(TOOL_ARGUMENTS, ensure_ascii=False)}}
        return {"role": "assistant", "content": None, "tool_calls": [call]}, "tool_calls"
    if model == "mock-coder":
        if not any(m.get("role") == "tool" for m in messages):
            call = {"id": "write-1", "type": "function", "function": {
                "name": "write", "arguments": json.dumps({"path": "greeting.txt", "content": "Hello, reader.\n"})}}
            return {"role": "assistant", "content": None, "tool_calls": [call]}, "tool_calls"
        return {"role": "assistant", "content": "Finished: greeting.txt now greets the reader."}, "stop"
    text = ""
    for message in reversed(messages):
        if message.get("role") == "user":
            content = message.get("content")
            text = content if isinstance(content, str) else json.dumps(content)
            break
    return {"role": "assistant", "content": f"pong: {text}"}, "stop"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send_json(self, status, value):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/v1/models":
            return self.send_json(200, {"object": "list", "data": [
                {"id": m, "object": "model", "owned_by": "mock"} for m in MODELS]})
        if self.path == "/_requests":
            with lock:
                return self.send_json(200, list(seen))
        if self.path in ("/health", "/healthz"):
            return self.send_json(200, {"status": "ok"})
        self.send_json(404, {"error": {"message": "not found"}})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length) or b"{}")
        with lock:
            seen.append({"path": self.path, "body": body,
                         "authorization": self.headers.get("Authorization")})
        if self.path != "/v1/chat/completions":
            return self.send_json(404, {"error": {"message": "not found"}})
        model = body.get("model")
        if model not in MODELS:
            return self.send_json(404, {"error": {"message": f"model {model} not found"}})
        if model == "mock-error":
            return self.send_json(500, {"error": {"type": "server_error", "message":
                "upstream failed at http://10.20.0.66:8080/v1 with key sk-mock-secret-0123456789"}})
        if model == "mock-slow":
            time.sleep(SLOW_SECONDS)
        message, finish = reply(body)
        if not body.get("stream"):
            return self.send_json(200, {
                "id": "mock", "object": "chat.completion", "created": 1, "model": model,
                "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}})
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(f"data: {chunk(model, {'role': 'assistant'})}\n\n".encode())
        if model == "mock-cut":
            self.wfile.write(f"data: {chunk(model, {'content': 'partial '})}\n\n".encode())
            self.wfile.flush()
            self.close_connection = True
            return
        delta = {k: v for k, v in message.items() if k != "role" and v is not None}
        if "tool_calls" in delta:
            delta["tool_calls"] = [{"index": 0, **call} for call in delta["tool_calls"]]
        self.wfile.write(f"data: {chunk(model, delta)}\n\n".encode())
        self.wfile.write(f"data: {chunk(model, {}, finish)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.close_connection = True


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
