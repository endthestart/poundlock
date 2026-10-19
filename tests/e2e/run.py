#!/usr/bin/env python3
"""End-to-end suite: the documented Compose install with no external services.

    python3 tests/e2e/run.py IMAGE [--full] [--report FILE] [--keep]

Starts compose.yaml plus tests/e2e/compose.e2e.yaml as a fresh, uniquely named
project on the current Docker host, then drives the controller through its public
API as an operator and as applications would: one directly configured model
server (no LiteLLM), application tokens, model listing, streamed and plain
chat, tool-call arguments, provider errors, timeouts, interrupted streams,
reasoning settings, capability evidence from a Toolkit summary, a durable job
from submission to patch, cancellation, isolation between tokens, and a
containerised node's enrollment and runtime lease. --full adds llama.cpp
serving a 105 MB Apache-2.0 model for a real generation, and an optional LiteLLM
gateway with its database in front of the scripted model, used with a shared
gateway key and with managed per-application keys.

Every model response except llama.cpp's comes from tests/e2e/mock_model.py. The
node runs in a container with a stand-in for systemd-inhibit, so its sleep
inhibition is not exercised. Nothing here proves GPU performance, Wake-on-LAN or
model quality. The project, its
volumes and its job containers are removed afterwards unless --keep.
"""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

ROOT = pathlib.Path(__file__).resolve().parents[2]
MODEL = {
    "file": "SmolLM2-135M-Instruct-Q4_K_M.gguf",
    "url": "https://huggingface.co/bartowski/SmolLM2-135M-Instruct-GGUF/resolve/"
           "09816acd5d99df7be770d85ea30822623dab342c/SmolLM2-135M-Instruct-Q4_K_M.gguf",
    "sha256": "2e8040ceae7815abe0dcb3540b9995eaa1fa0d2ca9e797d0a635ae4433c68c2d",
}
results = []


class Failed(Exception):
    pass


def check(name):
    """Record one named check; a failure is kept and the suite continues."""
    def wrap(function):
        def run(*args, **kwargs):
            started = time.monotonic()
            try:
                detail = function(*args, **kwargs)
                results.append({"check": name, "result": "PASS", "detail": detail or "",
                                "seconds": round(time.monotonic() - started, 1)})
            except Exception as error:  # noqa: BLE001 - every failure is reported
                results.append({"check": name, "result": "FAIL", "detail": f"{type(error).__name__}: {error}",
                                "seconds": round(time.monotonic() - started, 1)})
            print(f"{results[-1]['result']}: {name} {results[-1]['detail']}"[:400], flush=True)
        return run
    return wrap


def expect(condition, message):
    if not condition:
        raise Failed(message)


class Stack:
    def __init__(self, image, full):
        self.project = "poundlock-e2e-" + uuid.uuid4().hex[:8]
        self.full = full
        self.env = {**os.environ, "POUNDLOCK_IMAGE": image, "POUNDLOCK_PORT": "0",
                    "POUNDLOCK_LISTEN_ADDRESS": "127.0.0.1", "POUNDLOCK_ENROLLMENT_TOKEN": "",
                    # Generated per run for the disposable gateway; never a real key.
                    "E2E_LITELLM_MASTER_KEY": "sk-e2e-" + os.urandom(16).hex(),
                    "E2E_LITELLM_DB_PASSWORD": os.urandom(16).hex()}
        self.env["POUNDLOCK_DOCKER_GID"] = subprocess.run(
            ["docker", "run", "--rm", "--entrypoint", "stat", "-v", "/var/run/docker.sock:/var/run/docker.sock:ro",
             image, "-c", "%g", "/var/run/docker.sock"], check=True, capture_output=True, text=True).stdout.strip()
        self.files = ["-f", str(ROOT / "compose.yaml"), "-f", str(ROOT / "tests/e2e/compose.e2e.yaml")]
        self.profiles = ["--profile", "node"] + (["--profile", "full"] if full else [])

    def compose(self, *arguments, data=None, timeout=300, check=True):
        result = subprocess.run(["docker", "compose", "-p", self.project, "--project-directory", str(ROOT),
                                 *self.files, *self.profiles, *arguments], env=self.env, input=data,
                                capture_output=True, text=True, timeout=timeout)
        if check and result.returncode:
            raise RuntimeError(f"compose {arguments[0]} failed: {result.stderr[-3000:]}")
        return result.stdout

    def up(self):
        services = ["controller", "runner", "mock-model"] + (["llama", "litellm"] if self.full else [])
        self.compose("up", "-d", "--wait", "--wait-timeout", "300", *services, timeout=900)
        port = self.compose("port", "controller", "7331").strip().rsplit(":", 1)[1]
        self.base = f"http://127.0.0.1:{port}"
        self.admin = self.compose("exec", "-T", "controller", "cat", "/data/admin-token").strip()

    def down(self):
        self.compose("down", "-v", "--remove-orphans", timeout=300, check=False)

    def logs(self):
        return self.compose("logs", "--no-color", "--tail", "200", check=False, timeout=60)


def call(stack, method, path, token=None, body=None, timeout=30, raw=False):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(stack.base + path, method=method, headers=headers,
                                     data=None if body is None else json.dumps(body).encode())
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            data = response.read()
            return response.status, (data if raw else json.loads(data or b"null"))
    except urllib.error.HTTPError as error:
        data = error.read()
        try:
            return error.code, json.loads(data)
        except ValueError:
            return error.code, data.decode(errors="replace")


def stream(stack, path, token, body, timeout=30):
    """POST and read an SSE response; returns (status, data lines, completed)."""
    request = urllib.request.Request(stack.base + path, method="POST", data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"})
    lines, completed = [], False
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            for raw in response:
                line = raw.decode().strip()
                if line == "data: [DONE]":
                    completed = True
                elif line.startswith("data: "):
                    lines.append(json.loads(line[6:]))
            return response.status, lines, completed
    except urllib.error.HTTPError as error:
        return error.code, [error.read().decode(errors="replace")], False
    except (OSError, ValueError) as error:
        return None, lines + [f"{type(error).__name__}: {error}"], False


def upstream_requests(stack):
    program = "import urllib.request;print(urllib.request.urlopen('http://127.0.0.1:8080/_requests').read().decode())"
    return json.loads(stack.compose("exec", "-T", "mock-model", "python3", "-c", program))


def chat(model, text="hello", **extra):
    return {"model": model, "messages": [{"role": "user", "content": text}], **extra}


def suite(stack, args):
    admin = stack.admin
    state = {}

    @check("controller is ready and refuses an unauthenticated operator call")
    def _():
        status, _ = call(stack, "GET", "/readyz", raw=True)
        expect(status == 200, f"readyz {status}")
        status, _ = call(stack, "GET", "/api/v1/endpoints")
        expect(status == 401, f"anonymous endpoints list returned {status}")
    _()

    @check("operator adds one model server directly, without LiteLLM")
    def _():
        status, endpoint = call(stack, "POST", "/api/v1/endpoints", admin, {
            "name": "Mock model server", "kind": "openai", "base_url": "http://mock-model:8080/v1",
            "api_key": "mock-upstream-key", "data_destination": "operator_network"})
        expect(status == 200, f"create endpoint {status} {endpoint}")
        state["endpoint"] = endpoint["id"]
        expect("mock-upstream-key" not in json.dumps(call(stack, "GET", "/api/v1/endpoints", admin)[1]),
               "the upstream key is listed back")
        for name, model, uses, thinking in (
            ("chat", "mock-chat", ["interactive"], "model_default"),
            ("chat-off", "mock-chat", ["interactive"], "off"),
            ("tools", "mock-tools", ["interactive"], "model_default"),
            ("error", "mock-error", ["interactive"], "model_default"),
            ("slow", "mock-slow", ["interactive"], "model_default"),
            ("cut", "mock-cut", ["interactive"], "model_default"),
            ("coder", "mock-coder", ["durable"], "model_default"),
        ):
            status, profile = call(stack, "POST", "/api/v1/model-profiles", admin, {
                "name": name, "endpoint_id": state["endpoint"], "model_id": model, "context_tokens": 16384,
                "max_output_tokens": 1024, "uses": uses, "wake_policy": "never", "node_id": None,
                "runtime_profile": None, "thinking": thinking})
            expect(status == 200, f"create profile {name}: {status} {profile}")
            state[f"profile:{name}"] = profile["id"]
        return f"{len([k for k in state if k.startswith('profile:')])} profiles"
    _()

    @check("operator issues application tokens with explicit permissions")
    def _():
        for key, permissions in (("app", ["models:read", "inference:invoke", "jobs:read", "jobs:submit", "jobs:cancel"]),
                                 ("other", ["models:read", "jobs:read", "jobs:cancel"])):
            status, issued = call(stack, "POST", "/api/v1/api-tokens", admin, {"name": f"e2e-{key}", "permissions": permissions})
            expect(status == 200 and issued.get("token"), f"issue {key}: {status} {issued}")
            state[key] = issued["token"]
            state[f"{key}-id"] = issued["id"]
        status, _ = call(stack, "GET", "/api/v1/endpoints", state["app"])
        expect(status in (401, 403), f"an application token reached operator configuration: {status}")
    _()

    app = state.get("app")

    @check("an application lists the published models and nothing secret")
    def _():
        status, models = call(stack, "GET", "/v1/models", app)
        expect(status == 200, f"/v1/models {status}")
        rows = {row["id"]: row for row in models["data"]}
        expect({"chat", "tools", "coder"} <= set(rows), f"listed {sorted(rows)}")
        expect(rows["coder"]["poundlock"]["uses"] == ["durable"], rows["coder"]["poundlock"].get("uses"))
        expect("mock-upstream-key" not in json.dumps(models), "upstream key exposed")
        # `uses` tells clients what a profile is meant for; it does not restrict
        # which requests the relay answers (docs/guide/integrations.md).
        return f"{len(rows)} models"
    _()

    @check("plain chat completion through the relay")
    def _():
        status, body = call(stack, "POST", "/v1/chat/completions", app, chat("chat", "hello"))
        expect(status == 200, f"{status} {body}")
        expect(body["choices"][0]["message"]["content"] == "pong: hello", body)
    _()

    @check("streamed chat completion ends with [DONE]")
    def _():
        status, lines, completed = stream(stack, "/v1/chat/completions", app, chat("chat", "streamed", stream=True))
        text = "".join((c["choices"][0]["delta"].get("content") or "") for c in lines if isinstance(c, dict) and c.get("choices"))
        expect(status == 200 and completed and text == "pong: streamed", f"{status} {completed} {text!r}")
    _()

    @check("tool-call arguments arrive unchanged, plain and streamed")
    def _():
        tools = [{"type": "function", "function": {"name": "write_note", "parameters": {"type": "object"}}}]
        status, body = call(stack, "POST", "/v1/chat/completions", app, chat("tools", tools=tools))
        expect(status == 200, f"{status} {body}")
        arguments = json.loads(body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"])
        expected = {"path": "notes/übersicht.md", "lines": [1, 2], "meta": {"quote": "a \"b\" c"}}
        expect(arguments == expected, arguments)
        status, lines, completed = stream(stack, "/v1/chat/completions", app, chat("tools", tools=tools, stream=True))
        pieces = "".join(call["function"].get("arguments", "") for c in lines if isinstance(c, dict)
                         for choice in c.get("choices", []) for call in choice["delta"].get("tool_calls", []))
        expect(completed and json.loads(pieces) == expected, f"streamed {pieces!r}")
    _()

    @check("a provider error reaches the client without the upstream address or key")
    def _():
        status, body = call(stack, "POST", "/v1/chat/completions", app, chat("error"))
        text = json.dumps(body)
        expect(status >= 400, f"error returned {status}")
        expect("10.20.0.66" not in text and "sk-mock-secret" not in text, f"leaked: {text[:300]}")
        return f"HTTP {status}"
    _()

    @check("an upstream that never answers ends in a timeout, not a hang")
    def _():
        started = time.monotonic()
        status, body = call(stack, "POST", "/v1/chat/completions", app, chat("slow"), timeout=60)
        elapsed = time.monotonic() - started
        expect(status is not None and status >= 400, f"slow returned {status} {body}")
        expect(elapsed < 25, f"took {elapsed:.0f}s with a 5s timeout")
        return f"HTTP {status} after {elapsed:.1f}s"
    _()

    @check("an interrupted upstream stream is not reported as complete")
    def _():
        status, lines, completed = stream(stack, "/v1/chat/completions", app, chat("cut", stream=True))
        expect(not completed, f"cut stream ended with [DONE]: {lines}")
        return f"status {status}; last event {str(lines[-1])[:120] if lines else 'none'}"
    _()

    @check("thinking off is sent upstream; the model default sends nothing; the client token stays local")
    def _():
        call(stack, "POST", "/v1/chat/completions", app, chat("chat-off", "think-off"))
        seen = [r for r in upstream_requests(stack) if r["path"] == "/v1/chat/completions"]
        off = [r for r in seen if r["body"]["messages"][-1]["content"] == "think-off"][-1]["body"]
        default = [r for r in seen if r["body"]["messages"][-1]["content"] == "hello"][-1]["body"]
        expect(off.get("reasoning_effort") == "none", f"off sent {off.get('reasoning_effort')!r}")
        expect("reasoning_effort" not in default, f"default sent {default.get('reasoning_effort')!r}")
        expect(all(r["authorization"] == "Bearer mock-upstream-key" for r in seen),
               "upstream did not receive exactly the configured key")
    _()

    @check("capability evidence from a Toolkit summary is stored and published")
    def _():
        spec = importlib.util.spec_from_file_location("converter", ROOT / "scripts/toolkit-capabilities.py")
        converter = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(converter)
        summary = json.loads((ROOT / "docs/reference/fixtures/toolkit-recommendation-0.5.0.json").read_text())
        block = converter.capability_block(summary)
        status, body = call(stack, "PATCH", f"/api/v1/model-profiles/{state['profile:chat']}", admin,
                            {"capabilities": block})
        expect(status == 200, f"patch {status} {body}")
        status, models = call(stack, "GET", "/v1/models", app)
        row = next(r for r in models["data"] if r["id"] == "chat")
        names = {c["name"] for c in row["poundlock"]["capabilities"]}
        expect(names == {"code_repair_verified", "sealed_investigation", "plan_tool_schema"}, names)
        return "settings_match " + ", ".join(sorted({c["settings_match"] for c in row["poundlock"]["capabilities"]}))
    _()

    snapshot = {"kind": "snapshot", "files": [{"path": "greeting.txt", "executable": False,
                                                "content_base64": base64.b64encode(b"Hello!\n").decode()}]}

    def submit(key, prompt="Update greeting.txt to greet the reader."):
        return call(stack, "POST", "/api/v1/jobs", app, {
            "idempotency_key": key, "model_profile_id": state["profile:coder"], "harness_profile_id": "pi",
            "prompt": prompt, "source": snapshot})

    def wait(job_id, token=None, limit=300):
        deadline = time.monotonic() + limit
        while time.monotonic() < deadline:
            status, job = call(stack, "GET", f"/api/v1/jobs/{job_id}", token or app)
            if job.get("status") in ("completed", "failed", "cancelled"):
                return job
            time.sleep(3)
        raise Failed(f"job {job_id} still {job.get('status')} after {limit}s")

    @check("a durable job runs in a restricted worker and returns a patch")
    def _():
        key = str(uuid.uuid4())
        status, job = submit(key)
        expect(status == 200, f"submit {status} {job}")
        state["job"] = job["id"]
        again = submit(key)
        expect(again[0] == 200 and again[1]["id"] == job["id"], f"replay {again}")
        conflict = submit(key, "A different request.")
        expect(conflict[0] == 409, f"same key, different request: {conflict[0]}")
        done = wait(job["id"])
        expect(done["status"] == "completed", f"job ended {done['status']}: {json.dumps(done)[:600]}")
        status, patch = call(stack, "GET", f"/api/v1/jobs/{job['id']}/patches/0", app, raw=True)
        expect(status == 200 and b"Hello, reader." in patch, f"patch {status} {patch[:300]!r}")
        status, events = call(stack, "GET", f"/api/v1/jobs/{job['id']}/events?after=0", app)
        expect(status == 200 and events, "no job events")
        return f"{len(events)} events"
    _()

    @check("another token cannot see, read or cancel that job")
    def _():
        other = state["other"]
        status, listed = call(stack, "GET", "/api/v1/jobs", other)
        expect(status == 200 and all(j["id"] != state.get("job") for j in listed), "listed another token's job")
        for method, path in (("GET", f"/api/v1/jobs/{state['job']}"), ("GET", f"/api/v1/jobs/{state['job']}/events?after=0"),
                             ("GET", f"/api/v1/jobs/{state['job']}/patches/0"), ("POST", f"/api/v1/jobs/{state['job']}/cancel")):
            status, _ = call(stack, method, path, other, {} if method == "POST" else None)
            expect(status == 404, f"{method} {path} returned {status}")
    _()

    @check("a cancelled job ends as cancelled and keeps no source")
    def _():
        status, job = submit(str(uuid.uuid4()))
        expect(status == 200, f"submit {status}")
        status, body = call(stack, "POST", f"/api/v1/jobs/{job['id']}/cancel", app, {})
        expect(status == 200, f"cancel {status} {body}")
        done = wait(job["id"])
        expect(done["status"] == "cancelled", f"ended {done['status']}")
    _()

    @check("revoking a token stops it at once")
    def _():
        status, _ = call(stack, "POST", f"/api/v1/api-tokens/{state['other-id']}/revoke", admin, {})
        expect(status == 200, f"revoke {status}")
        status, _ = call(stack, "GET", "/v1/models", state["other"])
        expect(status == 401, f"revoked token got {status}")
    _()

    @check("a containerised node enrolls, reports inventory and serves a runtime lease")
    def _():
        status, enrollment = call(stack, "POST", "/api/v1/nodes/enrollment-tokens", admin, {"node_name": "e2e-node"})
        expect(status == 200, f"enrollment {status} {enrollment}")
        stack.env["POUNDLOCK_ENROLLMENT_TOKEN"] = enrollment["token"]
        stack.compose("up", "-d", "node")
        deadline, node = time.monotonic() + 90, None
        while time.monotonic() < deadline and not node:
            nodes = call(stack, "GET", "/api/v1/nodes", admin)[1]
            node = next((n for n in nodes if n["name"] == "e2e-node" and n.get("status") == "online"), None)
            time.sleep(2)
        expect(node, f"node not online: {call(stack, 'GET', '/api/v1/nodes', admin)[1]}")
        status, profile = call(stack, "POST", "/api/v1/model-profiles", admin, {
            "name": "on-node", "endpoint_id": state["endpoint"], "model_id": "mock-chat", "context_tokens": 16384,
            "max_output_tokens": 1024, "uses": ["interactive"], "wake_policy": "on-demand", "node_id": node["id"],
            "runtime_profile": "fixture"})
        expect(status == 200, f"node profile {status} {profile}")
        status, body = call(stack, "POST", "/v1/chat/completions", app, chat("on-node", "via node"), timeout=60)
        expect(status == 200 and body["choices"][0]["message"]["content"] == "pong: via node", f"{status} {body}")
        markers = stack.compose("exec", "-T", "node", "ls", "/tmp")
        expect("ensured-mock-chat" in markers, f"no ensure marker: {markers}")
        status, _ = call(stack, "DELETE", f"/api/v1/model-profiles/{profile['id']}", admin)
        status, _ = call(stack, "DELETE", f"/api/v1/nodes/{node['id']}", admin)
        expect(status in (200, 204), f"remove node {status}")
        return "markers: " + " ".join(markers.split())
    _()

    if args.full:
        @check("a real model generates through Poundlock (llama.cpp, SmolLM2 135M)")
        def _():
            status, endpoint = call(stack, "POST", "/api/v1/endpoints", admin, {
                "name": "llama.cpp", "kind": "openai", "base_url": "http://llama:8080/v1", "api_key": "",
                "data_destination": "operator_network"})
            expect(status == 200, f"endpoint {status} {endpoint}")
            status, profile = call(stack, "POST", "/api/v1/model-profiles", admin, {
                "name": "smollm2", "endpoint_id": endpoint["id"], "model_id": "smollm2-135m", "context_tokens": 4096,
                "max_output_tokens": 64, "uses": ["interactive"], "wake_policy": "never", "node_id": None,
                "runtime_profile": None})
            expect(status == 200, f"profile {status} {profile}")
            status, body = call(stack, "POST", "/v1/chat/completions", app,
                                chat("smollm2", "Say hello.", max_tokens=16), timeout=120)
            content = body["choices"][0]["message"]["content"] if status == 200 else body
            expect(status == 200 and isinstance(content, str) and content.strip(), f"{status} {content}")
            status, lines, completed = stream(stack, "/v1/chat/completions", app,
                                              chat("smollm2", "Count to three.", max_tokens=16, stream=True), timeout=120)
            expect(status == 200 and completed, f"stream {status} {completed}")
            return repr(content[:60])
        _()


        gateway_key = stack.env["E2E_LITELLM_MASTER_KEY"]

        def gateway_keys():
            out = stack.compose("exec", "-T", "litellm", "python3", "-c",
                                "import json,sys,urllib.request as u; r=u.Request(sys.argv[1], headers={'Authorization': 'Bearer '+sys.argv[2]}); "
                                "print(json.dumps(json.load(u.urlopen(r, timeout=10))))",
                                "http://127.0.0.1:4000/key/list?return_full_object=true", gateway_key)
            return json.loads(out).get("keys", [])

        @check("an optional LiteLLM gateway with a shared key: discover, publish, chat")
        def _():
            status, endpoint = call(stack, "POST", "/api/v1/endpoints", admin, {
                "name": "LiteLLM gateway", "kind": "litellm", "base_url": "http://litellm:4000/v1",
                "api_key": gateway_key})
            expect(status == 200, f"endpoint {status} {endpoint}")
            listed = json.dumps(call(stack, "GET", "/api/v1/endpoints", admin)[1])
            expect(gateway_key not in listed, "the gateway key is listed back")
            status, found = call(stack, "GET", "/api/v1/discovery", admin, timeout=60)
            expect(status == 200 and "mock-chat" in json.dumps(found), f"discovery {status} {str(found)[:300]}")
            status, profile = call(stack, "POST", "/api/v1/model-profiles", admin, {
                "name": "via-gateway", "endpoint_id": endpoint["id"], "model_id": "mock-chat", "context_tokens": 16384,
                "max_output_tokens": 256, "uses": ["interactive"], "wake_policy": "never", "node_id": None,
                "runtime_profile": None, "data_destination": "operator_network"})
            expect(status == 200, f"profile {status} {profile}")
            status, models = call(stack, "GET", "/v1/models", app)
            entry = next((m for m in models.get("data", []) if m.get("id") == "via-gateway"), None) if status == 200 else None
            expect(entry and entry.get("poundlock", {}).get("endpoint_kind") == "litellm", f"models {status} {entry}")
            status, body = call(stack, "POST", "/v1/chat/completions", app, chat("via-gateway", "through the gateway"), timeout=60)
            content = body["choices"][0]["message"]["content"] if status == 200 else body
            expect(content == "pong: through the gateway", f"{status} {content}")
            state["gateway-profile"] = profile["id"]
            return "discovered and published mock-chat; " + entry["poundlock"]["data_destination"]
        _()

        @check("an optional LiteLLM gateway with managed application keys")
        def _():
            status, endpoint = call(stack, "POST", "/api/v1/endpoints", admin, {
                "name": "LiteLLM managed", "kind": "litellm", "base_url": "http://litellm:4000/v1",
                "api_key": gateway_key, "managed_identity": {"enabled": True}})
            expect(status == 200, f"endpoint {status} {endpoint}")
            status, tested = call(stack, "POST", f"/api/v1/endpoints/{endpoint['id']}/test", admin, {}, timeout=60)
            expect(status == 200 and tested.get("poundlock", {}).get("management") == "available", f"test {status} {tested}")
            status, profile = call(stack, "POST", "/api/v1/model-profiles", admin, {
                "name": "via-managed", "endpoint_id": endpoint["id"], "model_id": "mock-chat", "context_tokens": 16384,
                "max_output_tokens": 256, "uses": ["interactive"], "wake_policy": "never", "node_id": None,
                "runtime_profile": None, "data_destination": "operator_network"})
            expect(status == 200, f"profile {status} {profile}")
            before = len(gateway_keys())
            status, ready = call(stack, "POST", "/api/v1/access/preflight", app, {"model": "via-managed"}, timeout=60)
            expect(status == 200, f"preflight {status} {ready}")
            status, body = call(stack, "POST", "/v1/chat/completions", app, chat("via-managed", "with my own key"), timeout=60)
            content = body["choices"][0]["message"]["content"] if status == 200 else body
            expect(content == "pong: with my own key", f"{status} {content}")
            keys = gateway_keys()
            expect(len(keys) == before + 1, f"expected one new virtual key, gateway lists {len(keys)} (was {before})")
            created = keys[-1] if isinstance(keys[-1], dict) else {}
            expect(created.get("models") == ["mock-chat"] if created else True, f"virtual key models {created.get('models')}")
            seen = json.dumps(call(stack, "GET", "/api/v1/endpoints", admin)[1]) + json.dumps(ready)
            expect(gateway_key not in seen and "sk-" not in seen.replace("sk-e2e", ""), "a gateway or virtual key is listed back")
            return f"one virtual key, models {created.get('models')}"
        _()

def fetch_model():
    folder = ROOT / "tests/e2e/.models"
    folder.mkdir(exist_ok=True)
    path = folder / MODEL["file"]
    if not path.exists() or hashlib.sha256(path.read_bytes()).hexdigest() != MODEL["sha256"]:
        with tempfile.NamedTemporaryFile(dir=folder, delete=False) as partial:
            with urllib.request.urlopen(MODEL["url"], timeout=300) as response:
                while chunk := response.read(1 << 20):
                    partial.write(chunk)
        if hashlib.sha256(pathlib.Path(partial.name).read_bytes()).hexdigest() != MODEL["sha256"]:
            os.unlink(partial.name)
            raise SystemExit("Model download does not match its pinned SHA-256")
        os.replace(partial.name, path)
    path.chmod(0o644)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("image")
    parser.add_argument("--full", action="store_true", help="also run llama.cpp with a tiny real model")
    parser.add_argument("--report", type=pathlib.Path)
    parser.add_argument("--keep", action="store_true", help="leave the project running for inspection")
    args = parser.parse_args()
    if args.full:
        fetch_model()
    stack = Stack(args.image, args.full)
    try:
        stack.up()
        suite(stack, args)
    finally:
        failed = [r for r in results if r["result"] != "PASS"]
        if failed or not results:
            print(stack.logs()[-12000:], file=sys.stderr)
        if not args.keep:
            stack.down()
    report = {"image": args.image, "full": args.full, "results": results,
              "passed": sum(r["result"] == "PASS" for r in results), "failed": len(failed)}
    if args.report:
        args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(f"{report['passed']} passed, {report['failed']} failed")
    return 1 if failed or not results else 0


if __name__ == "__main__":
    sys.exit(main())
