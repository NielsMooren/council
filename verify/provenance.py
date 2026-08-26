#!/usr/bin/env python3
"""Ad-hoc verification of tool provenance + `council audit`. NOT a unit suite.

A fake provider that requests tools, then the real `audit` subcommand run
against the resulting artifacts. The point of provenance is that a PAST run can
be checked, so the assertions are about what survives on disk.
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


COUNTS: dict = {}
LOCK = threading.Lock()
SCRIPT = []


class FakeLLM(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a, **k):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        model = body.get("model", "?")
        with LOCK:
            turn = COUNTS.get(model, 0)
            COUNTS[model] = turn + 1
        if turn < len(SCRIPT):
            name, args = SCRIPT[turn]
            ch = [
                {"type": "content_block_start", "index": 0,
                 "content_block": {"type": "tool_use", "id": f"t{turn}", "name": name}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "input_json_delta", "partial_json": json.dumps(args)}},
                {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
            ]
        else:
            ch = [
                {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "text_delta", "text": "Concluded. " + "y" * 340}},
                {"type": "message_delta", "delta": {"stop_reason": "end_turn"}},
            ]
        pl = b"".join(f"data: {json.dumps(c)}\n\n".encode() for c in ch) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(pl)))
        self.end_headers()
        self.wfile.write(pl)


srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeLLM)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()

tmp = pathlib.Path(tempfile.mkdtemp())
proj = tmp / "proj"
(proj / "src").mkdir(parents=True)
# Long enough that the default 3-line preview genuinely truncates.
(proj / "src" / "lib.rs").write_text(
    "// line1\npub fn f() {}\n// UNIQUE_MARKER\n" + "".join(f"// filler {i}\n" for i in range(20))
)

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


def run(script, question):
    global SCRIPT
    SCRIPT = script
    with LOCK:
        COUNTS.clear()
    return subprocess.run(
        [BIN, "-c", str(cfg), "ask", question, "--with", "a,b", "--rounds", "1",
         "--code", str(proj), "--fresh"],
        capture_output=True, text=True, env=env, timeout=200)


def audit(*args):
    return subprocess.run([BIN, "-c", str(cfg), "audit", *args],
                          capture_output=True, text=True, env=env, timeout=120)


print("1. provenance is written alongside the prose")
r = run([("read_file", {"path": "src/lib.rs"}),
         ("search_code", {"pattern": "UNIQUE_MARKER"}),
         ("read_file", {"path": "does/not/exist.rs"})], "Q-prov")
check("run succeeds", r.returncode == 0, r.stderr[-300:])
runs = sorted((tmp / "data" / "runs").iterdir(), key=lambda p: p.stat().st_mtime)
rd = runs[-1]
jsons = sorted(rd.glob("*.research.json"))
check("a .research.json per member", len(jsons) == 2, [p.name for p in jsons])
log = json.loads(jsons[0].read_text())
check("records round/member/provider/model",
      {"round", "member", "provider", "model"} <= set(log), list(log))
check("records the system prompt (protocol is visible in the record)",
      isinstance(log.get("system"), str) and len(log["system"]) > 50, len(str(log.get("system"))))
check("records the answer", "Concluded." in log.get("answer", ""), log.get("answer", "")[:60])
recs = log.get("research", [])
check("one record per lookup", len(recs) == 3, len(recs))

print("\n2. records carry the FULL result, not a summary line")
byname = {r["tool"] + str(r["args"]): r for r in recs}
rf = next(r for r in recs if r["tool"] == "read_file" and "lib.rs" in str(r["args"]))
check("full file body retained", "UNIQUE_MARKER" in rf["result"], rf["result"][:80])
check("line numbers retained", "2|" in rf["result"], rf["result"][:80])
check("args retained verbatim", rf["args"].get("path") == "src/lib.rs", rf["args"])
check("step index recorded", all(isinstance(r["step"], int) and r["step"] >= 1 for r in recs),
      [r["step"] for r in recs])

print("\n3. failed lookups stay visible and countable")
bad = next(r for r in recs if "does/not/exist" in str(r["args"]))
check("failure flagged", bad["failed"] is True, bad)
check("failure reason retained", "not found" in bad["result"], bad["result"][:80])
ok = next(r for r in recs if r["tool"] == "search_code")
check("successes not flagged as failures", ok["failed"] is False, ok)

print("\n4. the audit subcommand reads it back")
a = audit(rd.name)
check("audit succeeds", a.returncode == 0, a.stderr[-200:])
check("names the member and model", "m-a" in a.stdout or "m-b" in a.stdout, a.stdout[:200])
check("shows the tool and args", "read_file(path=src/lib.rs)" in a.stdout, a.stdout[:300])
check("marks failures", "FAILED" in a.stdout, a.stdout[:300])
check("counts lookups", "6 lookups, 2 failed" in a.stdout, a.stdout[-120:])
check("truncates by default", "--full to see" in a.stdout, a.stdout[-200:])

print("\n5. flags")
af = audit(rd.name, "--full")
check("--full shows whole results", "UNIQUE_MARKER" in af.stdout, af.stdout[:200])
check("--full drops the truncation notice", "more lines" not in af.stdout, af.stdout[-200:])
aq = audit(rd.name, "--failed")
check("--failed shows only failures", "FAILED" in aq.stdout and "UNIQUE_MARKER" not in aq.stdout,
      aq.stdout[:300])

print("\n6. graceful on missing or pre-provenance runs")
old = tmp / "data" / "runs" / "legacy"
old.mkdir(parents=True)
(old / "r1_a.md").write_text("prose only, no provenance\n")
ao = audit("legacy")
check("no-provenance run explained, not an error", ao.returncode == 0 and "no provenance" in ao.stdout,
      ao.stdout[:200])
check("explains why", "before tool provenance" in ao.stdout, ao.stdout[:300])
an = audit("nonexistent-run")
check("unknown run is an error", an.returncode != 0, an.returncode)
check("error names where it looked", "runs" in (an.stderr + an.stdout), (an.stderr + an.stdout)[:200])

print("\n7. a run with no tools writes no provenance")
r = run([], "Q-notools")
runs = sorted((tmp / "data" / "runs").iterdir(), key=lambda p: p.stat().st_mtime)
check("no .research.json when nothing was looked up",
      not list(runs[-1].glob("*.research.json")), [p.name for p in runs[-1].iterdir()])

srv.shutdown()
print(f"\n{n - len(fails)}/{n} passed")
if fails:
    print("FAILED: " + ", ".join(fails))
sys.exit(1 if fails else 0)
