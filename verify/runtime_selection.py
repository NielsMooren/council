#!/usr/bin/env python3
"""Ad-hoc verification of runtime model selection. NOT a unit-test suite.

Fake OpenAI-compatible SSE provider + a real config with a model registry.
Verifies that WHICH models run and HOW MANY rounds are decidable at call time,
via both the CLI (--with/--rounds/--chair/--max-tokens) and MCP (with/rounds/...).
"""
import http.server
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading

BIN = os.environ.get(
    "COUNCIL_BIN",
    str(pathlib.Path(__file__).resolve().parent.parent / "target" / "debug" / "council"),
)
fails, n = [], 0


def check(name, cond, detail: object = ""):
    global n
    n += 1
    print(f"  {'PASS' if cond else 'FAIL'}  {name}" + (f"  [{detail}]" if detail and not cond else ""))
    if not cond:
        fails.append(name)


REQS = []


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a, **k):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        REQS.append({"model": body["model"], "max": body.get("max_completion_tokens")})
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        ev = {"choices": [{"delta": {"content": f"VIEW from {body['model']}. " + "x" * 300},
                           "finish_reason": None}]}
        self.wfile.write(f"data: {json.dumps(ev)}\n\n".encode())
        done = {"choices": [{"delta": {}, "finish_reason": "stop"}]}
        self.wfile.write(f"data: {json.dumps(done)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()

tmp = pathlib.Path(tempfile.mkdtemp())
cfg = tmp / "config.toml"
cfg.write_text(f"""
max_tokens = 2000
data_dir = "{tmp / 'data'}"
default_panel = "default"

[[providers]]
name = "fake"
api = "openai_chat"
base_url = "http://127.0.0.1:{port}/v1"
api_key_env = "FAKE_KEY"
auth = "bearer"

[[models]]
name = "alpha"
provider = "fake"
model = "model-alpha"

[[models]]
name = "beta"
provider = "fake"
model = "model-beta"
persona = "Registry-level default persona."

[[models]]
name = "gamma"
provider = "fake"
model = "model-gamma"

[[models]]
name = "delta"
provider = "fake"
model = "model-delta"

[[panels]]
name = "default"
chair = "Chair"

  [[panels.members]]
  model = "alpha"
  name = "Pragmatist"
  persona = "Ship it."

  [[panels.members]]
  model = "beta"
  name = "Skeptic"

  [[panels.members]]
  model = "gamma"
  name = "Chair"

[[panels]]
name = "quick"
  [[panels.members]]
  model = "alpha"
  [[panels.members]]
  model = "beta"
""")
env = {**os.environ, "FAKE_KEY": "k"}


def run(*args):
    return subprocess.run([BIN, "-c", str(cfg), *args], capture_output=True,
                          text=True, env=env, timeout=180)


def models_used():
    return sorted({r["model"] for r in REQS})


print("1. registry discovery")
r = run("models")
check("models lists all 4 handles",
      all(h in r.stdout for h in ("alpha", "beta", "gamma", "delta")), r.stdout)
check("models shows the provider:model mapping", "model-alpha" in r.stdout, r.stdout)
r = run("panels")
check("panels resolves registry refs", "model-alpha" in r.stdout, r.stdout)

print("\n2. runtime model choice via --with")
REQS.clear()
r = run("ask", "Q?", "--with", "alpha,beta", "--rounds", "1")
check("--with runs exactly the chosen models", r.returncode == 0 and models_used() ==
      ["model-alpha", "model-beta"], models_used())
check("2 members + 1 chair = 3 calls", len(REQS) == 3, len(REQS))
check("roster echoed to stderr", "2 members" in r.stderr, r.stderr[:160])

REQS.clear()
r = run("ask", "Q?", "--with", "alpha,beta,gamma,delta", "--rounds", "1")
check("4 models -> 4 distinct models used", models_used() ==
      ["model-alpha", "model-beta", "model-delta", "model-gamma"], models_used())
check("4 members + chair = 5 calls", len(REQS) == 5, len(REQS))

print("\n3. runtime round count")
for rounds, expect in ((1, 2 + 1), (2, 4 + 1), (3, 6 + 1)):
    REQS.clear()
    run("ask", f"Q{rounds}?", "--with", "alpha,beta", "--rounds", str(rounds))
    check(f"--rounds {rounds} -> {expect} calls", len(REQS) == expect, len(REQS))

print("\n4. chair selection at runtime")
REQS.clear()
r = run("ask", "Qc?", "--with", "alpha,beta,gamma", "--chair", "beta", "--rounds", "1")
check("--chair beta accepted", r.returncode == 0, r.stderr[-200:])
check("chair ran last (beta synthesises)", REQS[-1]["model"] == "model-beta",
      [x["model"] for x in REQS])
r = run("ask", "Qc2?", "--with", "alpha,beta", "--chair", "nope", "--rounds", "1")
check("unknown chair rejected before any call", r.returncode != 0 and "chair" in
      (r.stderr + r.stdout).lower(), r.stderr[-200:])

print("\n5. aliases and unregistered models")
REQS.clear()
r = run("ask", "Qa?", "--with", "Hawk=alpha,Dove=beta", "--rounds", "2")
check("Alias=handle works", r.returncode == 0, r.stderr[-200:])
tr = (tmp / "data" / "runs")
txt = "\n".join(p.read_text() for p in tr.rglob("transcript.md"))
check("aliases appear in the transcript", "===== Hawk =====" in txt and "===== Dove =====" in txt,
      txt[:200])
REQS.clear()
r = run("ask", "Qu?", "--with", "alpha,fake:model-oneoff", "--rounds", "1")
check("provider:model escape hatch works",
      r.returncode == 0 and "model-oneoff" in models_used(), models_used())
r = run("ask", "Qz?", "--with", "alpha,nosuch", "--rounds", "1")
check("unknown handle rejected with the registry listed",
      r.returncode != 0 and "nosuch" in (r.stderr + r.stdout) and "alpha" in (r.stderr + r.stdout),
      r.stderr[-220:])
r = run("ask", "Q1?", "--with", "alpha", "--rounds", "1")
check("single member rejected (needs >=2 to debate)",
      r.returncode != 0 and "2 members" in (r.stderr + r.stdout), r.stderr[-200:])

print("\n6. named panels still work, and --with overrides them")
REQS.clear()
r = run("ask", "Qp?", "--panel", "quick", "--rounds", "1")
check("--panel quick uses its 2 members", models_used() ==
      ["model-alpha", "model-beta"], models_used())
REQS.clear()
r = run("ask", "Qp2?", "--panel", "quick", "--with", "gamma,delta", "--rounds", "1")
check("--with overrides --panel", models_used() ==
      ["model-delta", "model-gamma"], models_used())
REQS.clear()
r = run("ask", "Qd?", "--rounds", "1")
check("no flags -> default_panel", len(REQS) == 4, len(REQS))

print("\n7. runtime token ceiling")
REQS.clear()
run("ask", "Qt?", "--with", "alpha,beta", "--rounds", "1", "--max-tokens", "777")
check("--max-tokens applied to members", any(x["max"] == 777 for x in REQS),
      [x["max"] for x in REQS])

print("\n8. models tool flags an unusable model")
bad_cfg = tmp / "badkey.toml"
bad_cfg.write_text(cfg.read_text() + """
[[providers]]
name = "keyless"
api = "openai_chat"
base_url = "http://127.0.0.1:1/v1"
api_key_env = "DEFINITELY_NOT_SET_XYZ"
auth = "bearer"

[[models]]
name = "orphan"
provider = "keyless"
model = "model-orphan"
""")
msgs_u = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize",
     "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "p", "version": "1"}}},
    {"jsonrpc": "2.0", "method": "notifications/initialized"},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
     "params": {"name": "models", "arguments": {}}},
]
pu = subprocess.run([BIN, "-c", str(bad_cfg), "serve"],
                    input="\n".join(json.dumps(m) for m in msgs_u) + "\n",
                    capture_output=True, text=True, env=env, timeout=120)
utxt = ""
for line in pu.stdout.splitlines():
    try:
        d = json.loads(line.strip())
    except (json.JSONDecodeError, ValueError):
        continue
    if d.get("id") == 2:
        utxt = d["result"]["content"][0]["text"]
check("missing key marks the model unusable",
      "orphan" in utxt and "DEFINITELY_NOT_SET_XYZ" in utxt, utxt[-300:])
check("unusable models called out separately",
      "Not usable right now" in utxt, utxt[-300:])

print("\n9. MCP: same knobs over the wire")
msgs = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize",
     "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "p", "version": "1"}}},
    {"jsonrpc": "2.0", "method": "notifications/initialized"},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
    {"jsonrpc": "2.0", "id": 3, "method": "tools/call",
     "params": {"name": "deliberate",
                "arguments": {"question": "MCP?", "with": ["gamma", "delta"], "rounds": 1}}},
    {"jsonrpc": "2.0", "id": 4, "method": "tools/call",
     "params": {"name": "panels", "arguments": {}}},
    {"jsonrpc": "2.0", "id": 5, "method": "tools/call",
     "params": {"name": "models", "arguments": {}}},
]
REQS.clear()
p = subprocess.run([BIN, "-c", str(cfg), "serve"],
                   input="\n".join(json.dumps(m) for m in msgs) + "\n",
                   capture_output=True, text=True, env=env, timeout=180)
out = {}
for line in p.stdout.splitlines():
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
    except json.JSONDecodeError:
        continue
    if "id" in d:
        out[d["id"]] = d
tool = next((t for t in out.get(2, {}).get("result", {}).get("tools", [])
             if t["name"] == "deliberate"), None)
check("deliberate exposes with/chair/max_tokens/rounds",
      tool is not None and {"with", "chair", "max_tokens", "rounds"} <=
      set(tool["inputSchema"]["properties"]),
      list(tool["inputSchema"]["properties"]) if tool else None)
check("MCP with=[gamma,delta] ran those models",
      models_used() == ["model-delta", "model-gamma"], models_used())
check("MCP call returned content", 3 in out and out[3]["result"]["content"][0]["text"],
      str(out.get(3))[:160])
check("three tools exposed: deliberate/models/panels",
      2 in out and {t["name"] for t in out[2]["result"]["tools"]} ==
      {"deliberate", "models", "panels"},
      [t["name"] for t in out.get(2, {}).get("result", {}).get("tools", [])])
check("panels tool lists rosters",
      4 in out and "Pragmatist" in out[4]["result"]["content"][0]["text"],
      str(out.get(4))[:200])
mt = out.get(5, {}).get("result", {}).get("content", [{}])[0].get("text", "")
check("models tool lists every handle",
      all(h in mt for h in ("alpha", "beta", "gamma", "delta")), mt[:200])
check("models tool reports usability", "usable" in mt and "yes" in mt, mt[:200])
check("models tool explains how to choose",
      "rounds" in mt and "with" in mt, mt[-300:])

srv.shutdown()
print(f"\n{n - len(fails)}/{n} passed")
if fails:
    print("FAILED: " + ", ".join(fails))
sys.exit(1 if fails else 0)
