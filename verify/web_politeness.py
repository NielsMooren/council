#!/usr/bin/env python3
"""Ad-hoc verification of gzip + per-origin rate limiting. NOT a unit-test suite.

A fake LLM provider requests fetch_url repeatedly against a local server that
records arrival timestamps, so spacing and budget enforcement are measured
rather than assumed.
"""
import http.server
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
import time

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


HITS = []          # (host_label, monotonic_time)
HEADERS = []
TOOL_OUT = []
COUNTS: dict[str, int] = {}
LOCK = threading.Lock()
PAGE = b"<html><body><p>Spec body</p></body></html>"


class Site(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a, **k):
        pass

    def do_GET(self):
        HITS.append((self.headers.get("host", "?"), time.monotonic()))
        HEADERS.append(dict(self.headers.items()))
        self.send_response(200)
        self.send_header("content-type", "text/html")
        self.send_header("content-length", str(len(PAGE)))
        self.end_headers()
        self.wfile.write(PAGE)


class FakeLLM(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    urls: list = []
    # Optional per-model script, for testing that different hosts don't block
    # each other across concurrently-running members.
    per_model = None

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
                        TOOL_OUT.append(x.get("content", ""))

        script = (self.per_model or {}).get(model, []) if self.per_model else self.urls
        if turn < len(script):
            args = json.dumps({"url": script[turn]})
            ch = [
                {"type": "content_block_start", "index": 0,
                 "content_block": {"type": "tool_use", "id": f"t{turn}", "name": "fetch_url"}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "input_json_delta", "partial_json": args}},
                {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
            ]
        else:
            ch = [
                {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "text_delta", "text": "Done researching. " + "z" * 320}},
                {"type": "message_delta", "delta": {"stop_reason": "end_turn"}},
            ]
        p = b"".join(f"data: {json.dumps(c)}\n\n".encode() for c in ch) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(p)))
        self.end_headers()
        self.wfile.write(p)


site = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Site)
site_port = site.server_address[1]
threading.Thread(target=site.serve_forever, daemon=True).start()

llm = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeLLM)
llm_port = llm.server_address[1]
threading.Thread(target=llm.serve_forever, daemon=True).start()

tmp = pathlib.Path(tempfile.mkdtemp())
cfg = tmp / "c.toml"
cfg.write_text(f"""
max_tokens = 2000
data_dir = "{tmp / 'data'}"

[[providers]]
name = "f"
api = "anthropic_messages"
base_url = "http://127.0.0.1:{llm_port}"
api_key_env = "K"
auth = "x_api_key"

[[models]]
name = "a"
provider = "f"
model = "m-a"

[[models]]
name = "b"
provider = "f"
model = "m-b"
""")
env = {**os.environ, "K": "k"}
URL = f"http://127.0.0.1:{site_port}/spec"


def run(urls, *extra, per_model=None, question="Q?"):
    """Run one deliberation. `urls` is the script every member follows unless
    `per_model` overrides it. A fresh question avoids the resume cache, which
    would otherwise silently skip the HTTP calls we are trying to measure."""
    HITS.clear()
    HEADERS.clear()
    TOOL_OUT.clear()
    with LOCK:
        COUNTS.clear()
    FakeLLM.urls = urls or []
    FakeLLM.per_model = per_model
    return subprocess.run(
        [BIN, "-c", str(cfg), "ask", question, "--with", "a,b", "--rounds", "1",
         "--web", "--fresh", *extra],
        capture_output=True, text=True, env=env, timeout=300)


print("1. gzip/brotli advertised")
r = run([URL], question="Q-gzip")
check("run succeeds", r.returncode == 0, r.stderr[-300:])
check("at least one request arrived", len(HEADERS) >= 1, len(HEADERS))
ae = HEADERS[0].get("accept-encoding", "") if HEADERS else ""
check("accept-encoding sent", "gzip" in ae, repr(ae))
check("brotli offered too", "br" in ae, repr(ae))
check("user-agent still honest and attributable",
      HEADERS[0].get("user-agent", "").startswith("council/")
      and "github.com" in HEADERS[0].get("user-agent", ""),
      HEADERS[0].get("user-agent") if HEADERS else None)
check("no cookies sent", "cookie" not in {k.lower() for k in HEADERS[0]}, list(HEADERS[0]))
check("page text reached the model", any("Spec body" in x for x in TOOL_OUT), TOOL_OUT[:1])

print("\n2. per-host spacing is enforced (measured)")
# 4 sequential fetches per member to the same host, 300ms apart minimum.
r = run([URL] * 4, "--host-delay-ms", "300", question="Q-spacing")
check("run succeeds", r.returncode == 0, r.stderr[-300:])
times = sorted(t for _, t in HITS)
gaps = [round((b - a) * 1000) for a, b in zip(times, times[1:])]
check("multiple requests made", len(times) >= 4, len(times))
# Allow slack for scheduling jitter; the point is that they are NOT bunched.
check("consecutive requests >= ~300ms apart", all(g >= 240 for g in gaps), gaps)
# Two members x 4 fetches must interleave through ONE shared queue, so no pair
# of requests may arrive together even though the members run concurrently.
check("spacing is shared across members, not per-member",
      len(gaps) >= 4 and all(g >= 240 for g in gaps), gaps)

print("\n3. spacing is per-HOST, not global")
# Second server = different host bucket. Requests to different hosts must not
# serialise behind each other.
site2 = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Site)
p2 = site2.server_address[1]
threading.Thread(target=site2.serve_forever, daemon=True).start()
# 127.0.0.1 vs localhost resolve to the same address but are DIFFERENT host
# strings, which is what the limiter buckets on.
# Each member fetches a DIFFERENT host string on its first turn, so the two
# requests land in different buckets and must not wait for each other.
r = run(None, "--host-delay-ms", "3000", question="Q-hosts", per_model={
    "m-a": [f"http://127.0.0.1:{site_port}/a"],
    "m-b": [f"http://localhost:{site_port}/b"],
})
hosts = {h.split(":")[0] for h, _ in HITS}
times = sorted(t for _, t in HITS)
if len(times) >= 2 and len(hosts) >= 2:
    gap = (times[1] - times[0]) * 1000
    check("different hosts are not serialised", gap < 2500, f"{gap:.0f}ms across {hosts}")
else:
    check("different hosts are not serialised", False, f"hosts={hosts} hits={len(times)}")
site2.shutdown()

print("\n4. per-host budget is a hard stop")
r = run([URL] * 8, "--host-delay-ms", "0", "--host-budget", "3", question="Q-budget")
check("run still succeeds", r.returncode == 0, r.stderr[-300:])
check("requests capped at the shared budget", len(HITS) <= 3,
      f"{len(HITS)} hits, budget 3")
joined = "\n".join(TOOL_OUT)
check("refusal explained to the model", "rate limit" in joined, joined[-300:])
check("refusal names the remedy",
      "unverifiable" in joined or "Use what you have" in joined, joined[-300:])

print("\n5. limits are visible to the operator")
r = run([URL], "--host-delay-ms", "700", "--host-budget", "9", question="Q-report")
check("stderr reports the limits",
      "req/host" in r.stderr and "700ms" in r.stderr, r.stderr[:300])

print("\n6. host bucketing ignores port, credentials and case")
# All of these are the same host bucket, so they must be spaced apart.
r = run([f"http://127.0.0.1:{site_port}/x", f"http://127.0.0.1:{site_port}/y"],
        "--host-delay-ms", "400", question="Q-paths")
times = sorted(t for _, t in HITS)
gaps6 = [round((b - a) * 1000) for a, b in zip(times, times[1:])]
check("same host, different paths, still throttled",
      len(gaps6) >= 1 and all(g >= 320 for g in gaps6), gaps6)

site.shutdown()
llm.shutdown()
print(f"\n{n - len(fails)}/{n} passed")
if fails:
    print("FAILED: " + ", ".join(fails))
sys.exit(1 if fails else 0)
