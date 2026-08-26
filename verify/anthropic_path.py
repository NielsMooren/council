#!/usr/bin/env python3
"""Ad-hoc verification of council's Anthropic wire path + failure handling.

Separate from the OpenAI test because the Anthropic path has its own quirks:
thinking-block suppression, x-api-key/custom-header auth, and text_delta-only
extraction. Also verifies partial-panel degradation and truncation detection.
"""
import http.server
import json
import os
import pathlib
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
MODE = {"v": "ok"}


def sse(w, obj):
    w.write(f"data: {json.dumps(obj)}\n\n".encode())


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        REQS.append({
            "model": body["model"],
            "hdrs": {k.lower(): v for k, v in self.headers.items()},
            "thinking": body.get("thinking"),
            "has_max_tokens": "max_tokens" in body,
            "system_is_top_level": "system" in body,
            "stream": body.get("stream"),
        })
        mode = MODE["v"]
        if mode == "http500" and body["model"] == "model-beta":
            self.send_response(500)
            self.end_headers()
            self.wfile.write(b"upstream exploded")
            return

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        w = self.wfile
        sse(w, {"type": "message_start"})

        if mode in ("ok", "http500"):
            # A thinking block precedes text - its deltas must NOT be captured.
            sse(w, {"type": "content_block_start", "index": 0,
                    "content_block": {"type": "thinking"}})
            sse(w, {"type": "content_block_delta", "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": "SECRET-REASONING"}})
            sse(w, {"type": "content_block_stop", "index": 0})
            sse(w, {"type": "content_block_start", "index": 1,
                    "content_block": {"type": "text"}})
            for piece in (f"ANSWER from {body['model']}. ", "y" * 400):
                sse(w, {"type": "content_block_delta", "index": 1,
                        "delta": {"type": "text_delta", "text": piece}})
            sse(w, {"type": "message_delta", "delta": {"stop_reason": "end_turn"}})
        elif mode == "thinking_only":
            # The real-world failure: whole budget spent on thinking, zero text,
            # yet stop_reason says end_turn. Must be detected, not returned empty.
            sse(w, {"type": "content_block_start", "index": 0,
                    "content_block": {"type": "thinking"}})
            sse(w, {"type": "content_block_delta", "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": "z" * 500}})
            sse(w, {"type": "message_delta", "delta": {"stop_reason": "end_turn"}})
        elif mode == "truncated":
            sse(w, {"type": "content_block_start", "index": 0,
                    "content_block": {"type": "text"}})
            sse(w, {"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "cut off mid-sen"}})
            sse(w, {"type": "message_delta", "delta": {"stop_reason": "max_tokens"}})

        sse(w, {"type": "message_stop"})
        w.flush()


srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()

tmp = pathlib.Path(tempfile.mkdtemp())


def write_cfg(name, disable_thinking=True, auth='{ header = "api-key" }'):
    p = tmp / f"{name}.toml"
    p.write_text(f"""
max_tokens = 2000
data_dir = "{tmp / name}"

[[providers]]
name = "anth"
api = "anthropic_messages"
base_url = "http://127.0.0.1:{port}"
api_key_env = "ANTH_KEY"
auth = {auth}
headers = {{ "anthropic-version" = "2023-06-01", "x-trace" = "${{TRACE_ID}}" }}
disable_thinking = {str(disable_thinking).lower()}

[[panels]]
name = "default"
chair = "Chair"

  [[panels.members]]
  name = "Alpha"
  provider = "anth"
  model = "model-alpha"

  [[panels.members]]
  name = "Beta"
  provider = "anth"
  model = "model-beta"

  [[panels.members]]
  name = "Chair"
  provider = "anth"
  model = "model-chair"
""")
    return p


env = {**os.environ, "ANTH_KEY": "anth-sekret", "TRACE_ID": "trace-42"}


def run(cfg, *args, expect_ok=True):
    r = subprocess.run([BIN, "-c", str(cfg), *args], capture_output=True,
                       text=True, env=env, timeout=180)
    return r


print("1. Anthropic wire format")
cfg = write_cfg("ok")
REQS.clear()
MODE["v"] = "ok"
r = run(cfg, "ask", "Ship it?", "--rounds", "1")
check("ask exits 0", r.returncode == 0, r.stderr[-300:])
check("uses max_tokens (not max_completion_tokens)", all(x["has_max_tokens"] for x in REQS))
check("system is a top-level field", all(x["system_is_top_level"] for x in REQS))
check("streams", all(x["stream"] for x in REQS))
check("thinking disabled by default", all(x["thinking"] == {"type": "disabled"} for x in REQS))
check("custom auth header used", all(x["hdrs"].get("api-key") == "anth-sekret" for x in REQS))
check("no bearer header sent", all("authorization" not in x["hdrs"] for x in REQS))
check("static header passed", all(x["hdrs"].get("anthropic-version") == "2023-06-01" for x in REQS))
check("${ENV} expanded in header", all(x["hdrs"].get("x-trace") == "trace-42" for x in REQS))
check("thinking_delta NOT captured in output", "SECRET-REASONING" not in r.stdout, r.stdout[:200])
check("text_delta captured", "ANSWER from model-chair" in r.stdout, r.stdout[:200])

print("\n2. disable_thinking = false is honoured")
cfg2 = write_cfg("think", disable_thinking=False)
REQS.clear()
r = run(cfg2, "ask", "Ship it?", "--rounds", "1")
check("no thinking key sent when disabled=false", all(x["thinking"] is None for x in REQS),
      [x["thinking"] for x in REQS])

print("\n3. the zero-text failure mode is detected")
cfg3 = write_cfg("thinkonly")
MODE["v"] = "thinking_only"
r = run(cfg3, "ask", "Ship it?", "--rounds", "1")
check("all-thinking response fails loudly", r.returncode != 0, r.returncode)
check("error explains thinking budget",
      "thinking" in (r.stderr + r.stdout).lower(), (r.stderr + r.stdout)[-200:])

print("\n4. truncation is detected, not silently returned")
cfg4 = write_cfg("trunc")
MODE["v"] = "truncated"
r = run(cfg4, "ask", "Ship it?", "--rounds", "1")
check("stop_reason=max_tokens is an error", r.returncode != 0, r.returncode)
check("error says truncated", "truncated" in (r.stderr + r.stdout).lower(),
      (r.stderr + r.stdout)[-200:])

print("\n5. partial panel: one member dies, panel continues")
cfg5 = write_cfg("partial")
MODE["v"] = "http500"
r = run(cfg5, "ask", "Ship it?", "--rounds", "2")
check("run still succeeds with 2 of 3", r.returncode == 0, r.stderr[-300:])
check("failure surfaced to the user, not swallowed",
      "Beta" in r.stderr and "500" in r.stderr, r.stderr[-300:])
check("consensus still produced", "ANSWER from model-chair" in r.stdout, r.stdout[:200])

srv.shutdown()
print(f"\n{n - len(fails)}/{n} passed")
if fails:
    print("FAILED: " + ", ".join(fails))
sys.exit(1 if fails else 0)
