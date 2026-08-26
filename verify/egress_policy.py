#!/usr/bin/env python3
"""Verify the data-egress policy end to end through the real binary.

Threat model: council runs locally and may read local services (that is a
feature). What must be impossible is using a GET as a covert write channel.
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


HITS = []
TOOL_OUT = []
COUNTS: dict = {}
LOCK = threading.Lock()
PAGE = b"<html><body>LOCAL_SERVICE_PAYLOAD</body></html>"


class Local(http.server.BaseHTTPRequestHandler):
    """Stands in for a service on localhost, e.g. metrics or an internal API."""
    protocol_version = "HTTP/1.1"

    def log_message(self, *a, **k):
        pass

    def do_GET(self):
        HITS.append(self.path)
        if self.path.startswith("/redirect-with-query"):
            self.send_response(302)
            self.send_header("location", "/collect?stolen=SECRET")
            self.send_header("content-length", "0")
            self.end_headers()
            return
        if self.path.startswith("/redirect-to-file"):
            self.send_response(302)
            self.send_header("location", "file:///etc/passwd")
            self.send_header("content-length", "0")
            self.end_headers()
            return
        if self.path.startswith("/redirect-plain"):
            self.send_response(302)
            self.send_header("location", "/ok")
            self.send_header("content-length", "0")
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("content-type", "text/html")
        self.send_header("content-length", str(len(PAGE)))
        self.end_headers()
        self.wfile.write(PAGE)


class FakeLLM(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    url = ""

    def log_message(self, *a, **k):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        model = body.get("model", "?")
        with LOCK:
            turn = COUNTS.get(model, 0)
            COUNTS[model] = turn + 1
        for m in body.get("messages", []):
            c = m.get("content")
            if isinstance(c, list):
                for x in c:
                    if isinstance(x, dict) and x.get("type") == "tool_result":
                        TOOL_OUT.append(str(x.get("content", "")))
        if turn == 0:
            ch = [
                {"type": "content_block_start", "index": 0,
                 "content_block": {"type": "tool_use", "id": "t1", "name": "fetch_url"}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "input_json_delta",
                           "partial_json": json.dumps({"url": self.url})}},
                {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
            ]
        else:
            ch = [
                {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "text_delta", "text": "Done. " + "z" * 340}},
                {"type": "message_delta", "delta": {"stop_reason": "end_turn"}},
            ]
        p = b"".join(f"data: {json.dumps(c)}\n\n".encode() for c in ch) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(p)))
        self.end_headers()
        self.wfile.write(p)


local = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Local)
lport = local.server_address[1]
threading.Thread(target=local.serve_forever, daemon=True).start()

llm = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeLLM)
mport = llm.server_address[1]
threading.Thread(target=llm.serve_forever, daemon=True).start()

tmp = pathlib.Path(tempfile.mkdtemp())
cfg = tmp / "c.toml"
cfg.write_text(f"""
max_tokens = 900
data_dir = "{tmp / 'data'}"

[[providers]]
name = "p"
api = "anthropic_messages"
base_url = "http://127.0.0.1:{mport}"
api_key_env = "K"
auth = "x_api_key"

[[models]]
name = "a"
provider = "p"
model = "m-a"

[[models]]
name = "b"
provider = "p"
model = "m-b"
""")
env = {**os.environ, "K": "k"}


def run(url, question):
    HITS.clear()
    TOOL_OUT.clear()
    with LOCK:
        COUNTS.clear()
    FakeLLM.url = url
    return subprocess.run(
        [BIN, "-c", str(cfg), "ask", question, "--with", "a,b", "--rounds", "1",
         "--web", "--fresh", "--host-delay-ms", "0"],
        capture_output=True, text=True, env=env, timeout=200)


print("1. local services ARE reachable (by design)")
r = run(f"http://127.0.0.1:{lport}/metrics", "Q-local")
check("run succeeds", r.returncode == 0, r.stderr[-300:])
check("localhost was fetched", len(HITS) >= 1, HITS)
check("payload reached the model",
      any("LOCAL_SERVICE_PAYLOAD" in x for x in TOOL_OUT), TOOL_OUT[:1])

print("\n2. query strings ARE permitted (deliberate risk decision)")
# Accepted trade-off: too many real endpoints need a query string. The
# mitigation is provenance, not prevention.
r = run(f"http://127.0.0.1:{lport}/search?q=rust&limit=10", "Q-query")
check("run succeeds", r.returncode == 0, r.stderr[-200:])
check("query-bearing request reached the server", len(HITS) == 1, HITS)
check("the query was preserved verbatim",
      any("q=rust&limit=10" in h for h in HITS), HITS)

print("\n3. fragments and credentials are refused")
r = run(f"http://127.0.0.1:{lport}/x#SECRET", "Q-frag")
check("fragment refused", len(HITS) == 0 and "fragment" in "\n".join(TOOL_OUT).lower(),
      (HITS, TOOL_OUT[:1]))
r = run(f"http://user:pw@127.0.0.1:{lport}/x", "Q-creds")
check("credentials refused", len(HITS) == 0 and "credential" in "\n".join(TOOL_OUT).lower(),
      (HITS, TOOL_OUT[:1]))

print("\n4. non-http schemes are refused")
r = run("file:///etc/passwd", "Q-file")
check("file:// refused", len(HITS) == 0, HITS)
check("refusal names the scheme", "http" in "\n".join(TOOL_OUT).lower(), TOOL_OUT[:1])

print("\n5. a redirect to a query-bearing URL is followed (consistent with 2)")
r = run(f"http://127.0.0.1:{lport}/redirect-with-query", "Q-redirect-query")
check("run succeeds", r.returncode == 0, r.stderr[-200:])
check("both hops fetched", len(HITS) == 2, HITS)
check("the redirect target kept its query",
      any("stolen=SECRET" in h for h in HITS), HITS)

print("\n5b. but a redirect to a BLOCKED shape is still refused")
r = run(f"http://127.0.0.1:{lport}/redirect-to-file", "Q-redirect-file")
check("run still succeeds", r.returncode == 0, r.stderr[-200:])
check("only the redirect itself was fetched", len(HITS) == 1, HITS)
joined = "\n".join(TOOL_OUT)
check("refusal mentions the redirect", "redirect" in joined.lower(), joined[:250])

print("\n5c. every hop is RECORDED in provenance, not just the model's URL")
# The justification for permitting query strings is that fetches are auditable.
# A redirect hop the model never named must therefore appear in the record.
r = run(f"http://127.0.0.1:{lport}/redirect-with-query", "Q-hop-logged")
rundir = sorted((tmp / "data" / "runs").iterdir(), key=lambda q: q.stat().st_mtime)[-1]
logs = sorted(rundir.glob("*.research.json"))
fetched, asked = [], []
for lp in logs:
    for rec in json.loads(lp.read_text()).get("research", []):
        asked.append(rec["args"].get("url"))
        fetched.extend(rec.get("fetched", []))
check("provenance was written", bool(logs), logs)
check("the model's own URL is recorded",
      any("redirect-with-query" in (u or "") for u in asked), asked)
check("the REDIRECT HOP is recorded (was invisible before)",
      any("stolen=SECRET" in u for u in fetched), fetched)
# One member fetches for real, the other gets a cache hit and so records no
# hops - the ordering guarantee applies to the member that actually fetched.
first_hop = next((f for f in fetched if "redirect-with-query" in f), None)
check("the requested URL precedes its hop",
      first_hop is not None and fetched.index(first_hop) < next(
          i for i, u in enumerate(fetched) if "stolen=SECRET" in u),
      fetched)

print("\n5d. council audit surfaces the hop to a human")
a = subprocess.run([BIN, "-c", str(cfg), "audit", rundir.name],
                   capture_output=True, text=True, env=env, timeout=120)
check("audit succeeds", a.returncode == 0, a.stderr[-200:])
check("audit prints the hop", "hop 2:" in a.stdout and "stolen=SECRET" in a.stdout,
      a.stdout[:400])

print("\n6. a plain redirect still works")
r = run(f"http://127.0.0.1:{lport}/redirect-plain", "Q-redirect-ok")
check("plain redirect followed to the target", len(HITS) == 2, HITS)
check("final payload delivered",
      any("LOCAL_SERVICE_PAYLOAD" in x for x in TOOL_OUT), TOOL_OUT[:1])

local.shutdown()
llm.shutdown()
print(f"\n{n - len(fails)}/{n} passed")
if fails:
    print("FAILED: " + ", ".join(fails))
sys.exit(1 if fails else 0)
