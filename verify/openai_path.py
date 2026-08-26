#!/usr/bin/env python3
"""Ad-hoc E2E verification of `council`. NOT a test suite.

Spins up a FAKE OpenAI-compatible SSE provider on loopback, points a real
council config at it, and drives the real binary.
"""
import http.server
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import threading

BIN = os.environ.get("COUNCIL_BIN",
                    str(pathlib.Path(__file__).resolve().parent.parent
                        / "target" / "debug" / "council"))
fails, n = [], 0


def check(name, cond, detail=""):
    global n
    n += 1
    print(f"  {'PASS' if cond else 'FAIL'}  {name}" + (f"  [{detail}]" if detail and not cond else ""))
    if not cond:
        fails.append(name)


REQS = []


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        REQS.append({
            "model": body["model"],
            "auth": self.headers.get("authorization"),
            "system": body["messages"][0]["content"],
            "user": body["messages"][1]["content"],
            "stream": body.get("stream"),
            "has_max_completion": "max_completion_tokens" in body,
        })
        text = f"POSITION from {body['model']}. " + ("x" * 400)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for piece in (text[:200], text[200:]):
            ev = {"choices": [{"delta": {"content": piece}, "finish_reason": None}]}
            self.wfile.write(f"data: {json.dumps(ev)}\n\n".encode())
        # keep-alive comment frame: must be skipped, never fatal
        self.wfile.write(b": keep-alive\n\n")
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

[[providers]]
name = "fake"
api = "openai_chat"
base_url = "http://127.0.0.1:{port}/v1"
api_key_env = "FAKE_KEY"
auth = "bearer"

[[panels]]
name = "default"
chair = "Chair"

  [[panels.members]]
  name = "Alpha"
  provider = "fake"
  model = "model-alpha"
  persona = "You argue for speed."

  [[panels.members]]
  name = "Beta"
  provider = "fake"
  model = "model-beta"
  persona = "You argue for safety."

  [[panels.members]]
  name = "Chair"
  provider = "fake"
  model = "model-chair"
""")
env = {**os.environ, "FAKE_KEY": "sekret"}


def run(*args):
    return subprocess.run([BIN, "-c", str(cfg), *args], capture_output=True,
                          text=True, env=env, timeout=180)


print("1. config + discovery")
r = run("check")
check("check exits 0", r.returncode == 0, r.stderr[-200:])
check("check reports keys present", "all provider keys present" in r.stdout, r.stdout)
r = run("panels")
check("panels lists all 3 members", all(m in r.stdout for m in ("Alpha", "Beta", "Chair")), r.stdout)

print("\n2. end-to-end deliberation (3 rounds x 3 members + chair)")
REQS.clear()
r = run("ask", "Should we ship on Friday?", "--rounds", "3")
check("ask exits 0", r.returncode == 0, r.stderr[-300:])
check("prints consensus", "model-chair" in r.stdout, r.stdout[:200])
check("9 member calls + 1 chair = 10 HTTP calls", len(REQS) == 10, len(REQS))
check("all requests streamed", all(x["stream"] for x in REQS))
check("uses max_completion_tokens not max_tokens", all(x["has_max_completion"] for x in REQS))
check("bearer auth sent", all(x["auth"] == "Bearer sekret" for x in REQS))

print("\n3. round structure: R1 blind, R2+ sees peers")
r1 = [x for x in REQS if "ROUND 1 - OPENING POSITION" in x["user"]]
r2 = [x for x in REQS if "ROUND 2 - CROSS-EXAMINATION" in x["user"]]
r3 = [x for x in REQS if "FINAL ROUND - COMMITMENT" in x["user"]]
check("3 members got round 1", len(r1) == 3, len(r1))
check("round 1 has NO peer transcript", all("=====" not in x["user"] for x in r1))
check("3 members got round 2", len(r2) == 3, len(r2))
check("round 2 DOES have peer transcript", all("ROUND 1 ##" in x["user"] for x in r2))
check("round 2 shows all 3 peers", all(
    sum(f"===== {p} =====" in x["user"] for p in ("Alpha", "Beta", "Chair")) == 3 for x in r2))
check("3 members got final round", len(r3) == 3, len(r3))
check("personas injected per-member",
      any("argue for speed" in x["system"] for x in REQS)
      and any("argue for safety" in x["system"] for x in REQS))
check("question in system prompt", all("Should we ship on Friday?" in x["system"] for x in REQS))

print("\n4. artifacts + resume")
runs = list((tmp / "data" / "runs").iterdir())
check("one run dir created", len(runs) == 1, [p.name for p in runs])
files = sorted(p.name for p in runs[0].iterdir())
check("9 per-member caches written", len([f for f in files if re.match(r"r\d_", f)]) == 9, files)
check("transcript.md written", "transcript.md" in files, files)
check("consensus.md written", "consensus.md" in files, files)

REQS.clear()
r = run("ask", "Should we ship on Friday?", "--rounds", "3")
check("resume run exits 0", r.returncode == 0, r.stderr[-200:])
check("resume issues ZERO http calls", len(REQS) == 0, len(REQS))

REQS.clear()
r = run("ask", "Should we ship on Friday?", "--rounds", "3", "--fresh")
check("--fresh re-runs", len(REQS) >= 1, len(REQS))

print("\n5. MCP stdio surface")
msgs = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize",
     "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "p", "version": "1"}}},
    {"jsonrpc": "2.0", "method": "notifications/initialized"},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
    {"jsonrpc": "2.0", "id": 3, "method": "tools/call",
     "params": {"name": "deliberate", "arguments": {"question": "Ship Friday?", "rounds": 1}}},
]
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
check("initialize responds", 1 in out, list(out))
if 1 in out:
    check("server identifies as council",
          out[1]["result"]["serverInfo"]["name"] == "council", out[1]["result"]["serverInfo"])
check("tools/list = deliberate + panels",
      2 in out and {t["name"] for t in out[2]["result"]["tools"]} == {"deliberate", "panels"},
      out.get(2, {}).get("result", {}).get("tools"))
if 2 in out:
    dl = next(t for t in out[2]["result"]["tools"] if t["name"] == "deliberate")
    check("deliberate requires only 'question'",
          dl["inputSchema"]["required"] == ["question"], dl["inputSchema"].get("required"))
    check("deliberate exposes panel/rounds/context",
          {"panel", "rounds", "context"} <= set(dl["inputSchema"]["properties"]),
          list(dl["inputSchema"]["properties"]))
check("tools/call deliberate returns content",
      3 in out and out[3]["result"]["content"][0]["text"], str(out.get(3))[:200])
check("no stdout pollution (logs to stderr)",
      all(line.strip().startswith("{") or not line.strip() for line in p.stdout.splitlines()))

print("\n6. config validation")
bad = tmp / "bad.toml"
bad.write_text(cfg.read_text().replace('provider = "fake"\n  model = "model-beta"',
                                       'provider = "nope"\n  model = "model-beta"'))
r = subprocess.run([BIN, "-c", str(bad), "check"], capture_output=True, text=True,
                   env=env, timeout=60)
check("rejects unknown provider",
      r.returncode != 0 and "nope" in (r.stderr + r.stdout), r.stderr[-160:])

srv.shutdown()
print(f"\n{n - len(fails)}/{n} passed")
if fails:
    print("FAILED: " + ", ".join(fails))
sys.exit(1 if fails else 0)
