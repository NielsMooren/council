#!/usr/bin/env python3
"""Ad-hoc verification of panellist research tools. NOT a unit-test suite.

A fake provider that *requests tools* (both OpenAI and Anthropic wire shapes),
so the agentic loop, tool execution, sandboxing and audit trail are all
exercised without spending tokens.
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


# What the fake model asks for, per (api, call-index). Set per test.
SCRIPT = {"openai": [], "anthropic": []}
SEEN = {"tools_offered": [], "results": []}
# Per-model turn counter: members run concurrently, so a single global counter
# would interleave and hand the wrong script step to the wrong model.
COUNTS: dict[str, int] = {}
COUNT_LOCK = threading.Lock()


def sse(w, obj):
    w.write(f"data: {json.dumps(obj)}\n\n".encode())


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a, **k):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        anthropic = "/v1/messages" in self.path
        key = "anthropic" if anthropic else "openai"
        SEEN["tools_offered"].append([
            (t.get("name") or t.get("function", {}).get("name"))
            for t in body.get("tools", [])
        ])
        # Harvest any tool results the client sent back to us.
        for m in body.get("messages", []):
            if m.get("role") == "tool":
                SEEN["results"].append(m.get("content", ""))
            for c in (m.get("content") if isinstance(m.get("content"), list) else []):
                if isinstance(c, dict) and c.get("type") == "tool_result":
                    SEEN["results"].append(c.get("content", ""))

        model = body.get("model", "?")
        with COUNT_LOCK:
            turn = COUNTS.get(model, 0)
            COUNTS[model] = turn + 1
        want = SCRIPT[key][turn] if turn < len(SCRIPT[key]) else None

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        w = self.wfile

        if want is None:
            # Final prose turn.
            if anthropic:
                sse(w, {"type": "content_block_start", "index": 0,
                        "content_block": {"type": "text"}})
                sse(w, {"type": "content_block_delta", "index": 0,
                        "delta": {"type": "text_delta",
                                  "text": "FINAL ANSWER based on research. " + "x" * 320}})
                sse(w, {"type": "message_delta", "delta": {"stop_reason": "end_turn"}})
            else:
                sse(w, {"choices": [{"delta": {"content": "FINAL ANSWER based on research. "
                                                          + "x" * 320}, "finish_reason": None}]})
                sse(w, {"choices": [{"delta": {}, "finish_reason": "stop"}]})
        else:
            name, args = want
            if anthropic:
                sse(w, {"type": "content_block_start", "index": 0,
                        "content_block": {"type": "tool_use", "id": "tu_1", "name": name}})
                # Fragmented arguments, as the real API streams them.
                blob = json.dumps(args)
                mid = len(blob) // 2
                sse(w, {"type": "content_block_delta", "index": 0,
                        "delta": {"type": "input_json_delta", "partial_json": blob[:mid]}})
                sse(w, {"type": "content_block_delta", "index": 0,
                        "delta": {"type": "input_json_delta", "partial_json": blob[mid:]}})
                sse(w, {"type": "message_delta", "delta": {"stop_reason": "tool_use"}})
            else:
                blob = json.dumps(args)
                mid = len(blob) // 2
                sse(w, {"choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "tc_1", "function": {"name": name, "arguments": blob[:mid]}}]},
                    "finish_reason": None}]})
                sse(w, {"choices": [{"delta": {"tool_calls": [
                    {"index": 0, "function": {"arguments": blob[mid:]}}]},
                    "finish_reason": None}]})
                sse(w, {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
        w.write(b"data: [DONE]\n\n")
        w.flush()


srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()

tmp = pathlib.Path(tempfile.mkdtemp())
# A tiny project for the panel to read.
proj = tmp / "proj"
(proj / "src").mkdir(parents=True)
(proj / "src" / "lib.rs").write_text(
    "// SPDX\npub fn add(a: u8, b: u8) -> u8 {\n    a + b // OVERFLOW_MARKER\n}\n")
(proj / "src" / "util.rs").write_text("pub const NAME: &str = \"util\";\n")
(proj / "secret.txt").write_text("TOP_SECRET_VALUE")
outside = tmp / "outside.txt"
outside.write_text("SHOULD_NEVER_BE_READ")

cfg = tmp / "config.toml"
cfg.write_text(f"""
max_tokens = 2000
data_dir = "{tmp / 'data'}"

[[providers]]
name = "fo"
api = "openai_chat"
base_url = "http://127.0.0.1:{port}/v1"
api_key_env = "K"
auth = "bearer"

[[providers]]
name = "fa"
api = "anthropic_messages"
base_url = "http://127.0.0.1:{port}"
api_key_env = "K"
auth = "x_api_key"

[[models]]
name = "o"
provider = "fo"
model = "m-openai"

[[models]]
name = "a"
provider = "fa"
model = "m-anthropic"
""")
env = {**os.environ, "K": "k"}


def run(*args):
    SEEN["tools_offered"].clear()
    SEEN["results"].clear()
    return subprocess.run([BIN, "-c", str(cfg), *args], capture_output=True,
                          text=True, env=env, timeout=180)


def reset(openai_calls, anthropic_calls):
    SCRIPT["openai"] = openai_calls
    SCRIPT["anthropic"] = anthropic_calls
    with COUNT_LOCK:
        COUNTS.clear()


print("1. tools are OFF unless asked for")
reset([], [])
r = run("ask", "Q?", "--with", "o,a", "--rounds", "1")
check("run succeeds without tools", r.returncode == 0, r.stderr[-200:])
check("no tools advertised to the model",
      all(t == [] for t in SEEN["tools_offered"]), SEEN["tools_offered"])

print("\n2. --code advertises the filesystem tools")
reset([], [])
r = run("ask", "Q2?", "--with", "o,a", "--rounds", "1", "--code", str(proj))
offered = SEEN["tools_offered"][0] if SEEN["tools_offered"] else []
check("read_file/search_code/list_files offered",
      {"read_file", "search_code", "list_files"} <= set(offered), offered)
check("fetch_url NOT offered without --web", "fetch_url" not in offered, offered)
check("stderr announces tools", "tools enabled" in r.stderr, r.stderr[:200])

print("\n3. --web adds fetch_url")
reset([], [])
run("ask", "Q3?", "--with", "o,a", "--rounds", "1", "--code", str(proj), "--web")
offered = SEEN["tools_offered"][0] if SEEN["tools_offered"] else []
check("fetch_url offered with --web", "fetch_url" in offered, offered)

print("\n4. the agentic loop actually runs a tool (both wire formats)")
reset([("read_file", {"path": "src/lib.rs"})],
      [("read_file", {"path": "src/lib.rs"})])
r = run("ask", "Q4?", "--with", "o,a", "--rounds", "1", "--code", str(proj))
check("run succeeds", r.returncode == 0, r.stderr[-300:])
check("file contents returned to the model",
      any("OVERFLOW_MARKER" in x for x in SEEN["results"]), SEEN["results"][:2])
check("line numbers included", any("2|" in x for x in SEEN["results"]), SEEN["results"][:1])
runs = sorted((tmp / "data" / "runs").iterdir(), key=lambda p: p.stat().st_mtime)
txt = "\n".join(f.read_text() for f in runs[-1].glob("r1_*.md"))
check("research trail recorded in the transcript", "<research>" in txt, txt[-200:])
check("trail names the tool and args", "read_file(path=src/lib.rs)" in txt, txt[-300:])

print("\n5. search_code and list_files")
reset([("search_code", {"pattern": "OVERFLOW_MARKER"}), ("list_files", {"glob": "*.rs"})],
      [("list_files", {"glob": "*.rs"}), ("search_code", {"pattern": "OVERFLOW_MARKER"})])
run("ask", "Q5?", "--with", "o,a", "--rounds", "1", "--code", str(proj))
joined = "\n".join(SEEN["results"])
check("search returns file:line", "lib.rs:3" in joined or "lib.rs:2" in joined, joined[:200])
check("list returns project files", "util.rs" in joined, joined[:200])
check("root prefix stripped from paths", str(proj) not in joined, joined[:200])

print("\n6. sandbox: escapes are refused")
for label, path in (("absolute path", str(outside)),
                    ("parent traversal", "../outside.txt"),
                    ("nested traversal", "src/../../outside.txt")):
    reset([("read_file", {"path": path})], [])
    run("ask", f"Q6{label}?", "--with", "o,a", "--rounds", "1", "--code", str(proj))
    joined = "\n".join(SEEN["results"])
    check(f"{label} refused", "SHOULD_NEVER_BE_READ" not in joined and "error" in joined.lower(),
          joined[:160])

reset([("read_file", {"path": "secret.txt"})], [])
run("ask", "Q6in?", "--with", "o,a", "--rounds", "1", "--code", str(proj))
check("files INSIDE the root are readable",
      any("TOP_SECRET_VALUE" in x for x in SEEN["results"]), SEEN["results"][:1])

print("\n7. web tool is gated even if the model asks")
reset([("fetch_url", {"url": "http://127.0.0.1:1/"})], [])
run("ask", "Q7?", "--with", "o,a", "--rounds", "1", "--code", str(proj))
joined = "\n".join(SEEN["results"])
check("fetch_url refused without --web", "disabled" in joined.lower(), joined[:200])

print("\n8. bad tool input is reported to the model, not fatal")
reset([("read_file", {"path": "does/not/exist.rs"})], [])
r = run("ask", "Q8?", "--with", "o,a", "--rounds", "1", "--code", str(proj))
check("run still succeeds", r.returncode == 0, r.stderr[-200:])
check("error handed back as tool output",
      any("not found" in x for x in SEEN["results"]), SEEN["results"][:1])
reset([("no_such_tool", {})], [])
r = run("ask", "Q8b?", "--with", "o,a", "--rounds", "1", "--code", str(proj))
check("unknown tool name reported, not fatal",
      r.returncode == 0 and any("unknown tool" in x for x in SEEN["results"]),
      SEEN["results"][:1])

print("\n9. the loop is bounded")
# Ask for a tool on every single turn; the loop must stop and still produce prose.
reset([("list_files", {})] * 40, [("list_files", {})] * 40)
r = run("ask", "Q9?", "--with", "o,a", "--rounds", "1", "--code", str(proj))
check("infinite tool requests terminate", r.returncode == 0, r.stderr[-300:])
# 2 members + chair, each capped at 12 tool rounds + 1 closing call.
check("loop is bounded (not unbounded)",
      len(SEEN["tools_offered"]) <= 3 * 14, len(SEEN["tools_offered"]))
check("still produces output despite exhausting the budget",
      "FINAL ANSWER" in r.stdout or "research" in r.stdout.lower(), r.stdout[:300])

print("\n10. cache key separates tool-enabled runs")
reset([], [])
run("ask", "SameQ?", "--with", "o,a", "--rounds", "1")
before = len(list((tmp / "data" / "runs").iterdir()))
run("ask", "SameQ?", "--with", "o,a", "--rounds", "1", "--code", str(proj))
after = len(list((tmp / "data" / "runs").iterdir()))
check("tool access creates a distinct run dir", after == before + 1, f"{before}->{after}")

print("\n11. MCP exposes code/web")
msgs = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize",
     "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "p", "version": "1"}}},
    {"jsonrpc": "2.0", "method": "notifications/initialized"},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
]
p = subprocess.run([BIN, "-c", str(cfg), "serve"],
                   input="\n".join(json.dumps(m) for m in msgs) + "\n",
                   capture_output=True, text=True, env=env, timeout=120)
tool = None
for line in p.stdout.splitlines():
    try:
        d = json.loads(line.strip())
    except (json.JSONDecodeError, ValueError):
        continue
    if d.get("id") == 2:
        tool = next((t for t in d["result"]["tools"] if t["name"] == "deliberate"), None)
props = list(tool["inputSchema"]["properties"]) if tool else []
check("deliberate exposes `code` and `web`", {"code", "web"} <= set(props), props)

srv.shutdown()
print(f"\n{n - len(fails)}/{n} passed")
if fails:
    print("FAILED: " + ", ".join(fails))
sys.exit(1 if fails else 0)
