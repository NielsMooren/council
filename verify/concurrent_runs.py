#!/usr/bin/env python3
"""Settle the panel's unresolved disagreement: can two concurrent runs sharing a
cache key cross-pair one run's provenance with another's prose?

sol/terra said yes, opus said the race is too narrow to matter, nobody tested it.
This runs N identical invocations simultaneously against the same cache key and
checks every artifact pair for internal consistency.
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

BIN = str(pathlib.Path.home() / "programming/council/target/release/council")
LOCK = threading.Lock()
COUNTS: dict = {}
fails, n = [], 0


def check(name, cond, detail: object = ""):
    global n
    n += 1
    print(f"  {'PASS' if cond else 'FAIL'}  {name}" + (f"  [{detail}]" if detail and not cond else ""))
    if not cond:
        fails.append(name)


class FakeLLM(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    # Artificial latency to widen the interleaving window the panel argued about.
    delay = 0.35

    def log_message(self, *a, **k):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        model = body.get("model", "?")
        with LOCK:
            turn = COUNTS.get(model, 0)
            COUNTS[model] = turn + 1
        time.sleep(self.delay)
        if turn == 0:
            args = json.dumps({"path": "src/lib.rs"})
            ch = [
                {"type": "content_block_start", "index": 0,
                 "content_block": {"type": "tool_use", "id": "t1", "name": "read_file"}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "input_json_delta", "partial_json": args}},
                {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
            ]
        else:
            ch = [
                {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "text_delta",
                           "text": "Answer citing the lookup. " + "q" * 340}},
                {"type": "message_delta", "delta": {"stop_reason": "end_turn"}},
            ]
        p = b"".join(f"data: {json.dumps(c)}\n\n".encode() for c in ch) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(p)))
        self.end_headers()
        self.wfile.write(p)


srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeLLM)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()

tmp = pathlib.Path(tempfile.mkdtemp())
proj = tmp / "proj"
(proj / "src").mkdir(parents=True)
(proj / "src" / "lib.rs").write_text("// UNIQUE_MARKER\n" + "".join(f"// l{i}\n" for i in range(30)))

cfg = tmp / "c.toml"
cfg.write_text(f"""
max_tokens = 900
data_dir = "{tmp / 'data'}"

[[providers]]
name = "p"
api = "anthropic_messages"
base_url = "http://127.0.0.1:{port}"
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

CONCURRENT = 4
print(f"1. {CONCURRENT} identical runs, same cache key, launched simultaneously")
# Deliberately NOT --fresh: they must all target the same cache dir, which is
# the precondition for the race.
procs = [
    subprocess.Popen(
        [BIN, "-c", str(cfg), "ask", "Same question for all",
         "--with", "a,b", "--rounds", "1", "--code", str(proj)],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
    for _ in range(CONCURRENT)
]
results = [p.communicate(timeout=300) for p in procs]
codes = [p.returncode for p in procs]
check("every run exited cleanly", all(c == 0 for c in codes), codes)

runs = [d for d in (tmp / "data" / "runs").iterdir() if d.is_dir()]
check("all runs shared ONE cache dir (race precondition held)", len(runs) == 1,
      [r.name for r in runs])

print("\n2. every artifact pair is internally consistent")
rd = runs[0]
mds = sorted(rd.glob("r1_*.md"))
jsons = sorted(rd.glob("r1_*.research.json"))
check("prose written for both members", len(mds) == 2, [p.name for p in mds])
check("provenance written for both members", len(jsons) == 2, [p.name for p in jsons])

# The claim under test: prose citing lookups must have matching provenance, and
# that provenance must belong to the same member.
bad_pairs = []
for md in mds:
    member = md.stem.removeprefix("r1_")
    jp = rd / f"r1_{member}.research.json"
    prose = md.read_text()
    if "<research>" in prose and not jp.exists():
        bad_pairs.append(f"{member}: prose cites lookups but no provenance")
        continue
    if jp.exists():
        log = json.loads(jp.read_text())
        if log.get("member") != member:
            bad_pairs.append(f"{member}: provenance says member={log.get('member')}")
        # The recorded answer must be the answer that was kept.
        if log.get("answer") and log["answer"].strip() and log["answer"] != prose:
            bad_pairs.append(f"{member}: recorded answer != persisted prose")
check("no cross-paired provenance/prose", not bad_pairs, bad_pairs)

print("\n3. provenance content is not garbled by concurrent writers")
for jp in jsons:
    raw = jp.read_text()
    try:
        log = json.loads(raw)
    except json.JSONDecodeError as e:
        check(f"{jp.name} parses", False, str(e))
        continue
    check(f"{jp.name} parses as valid JSON", True)
    recs = log.get("research", [])
    check(f"{jp.name} has its lookup", len(recs) == 1, len(recs))
    if recs:
        check(f"{jp.name} result intact",
              "UNIQUE_MARKER" in recs[0].get("result", ""), recs[0].get("result", "")[:60])

print("\n4. consensus is written once and is readable")
cons = rd / "consensus.md"
check("consensus.md exists", cons.exists())
if cons.exists():
    text = cons.read_text()
    check("consensus is non-trivial", len(text) > 40, len(text))
    check("consensus is not duplicated/interleaved",
          text.count("Answer citing the lookup") <= 1 or "##" in text, text[:120])

print("\n5. audit works on the concurrently-written run")
a = subprocess.run([BIN, "-c", str(cfg), "audit", rd.name],
                   capture_output=True, text=True, env=env, timeout=120)
check("audit succeeds", a.returncode == 0, a.stderr[-200:])
check("audit reports both members",
      a.stdout.count("=== round 1") == 2, a.stdout[:300])

srv.shutdown()
print(f"\n{n - len(fails)}/{n} passed")
if fails:
    print("FAILED: " + ", ".join(fails))
sys.exit(1 if fails else 0)
