# Verification harnesses

Ad-hoc end-to-end verification. **Not** a unit-test suite — these drive the real
`council` binary against fake in-process SSE servers on loopback, so no tokens
are spent and no network access is needed.

```bash
cargo build
python3.11+ verify/openai_path.py        # 32 checks
python3.11+ verify/anthropic_path.py     # 19 checks
python3.11+ verify/runtime_selection.py  # 33 checks
python3.11+ verify/research_tools.py     # 27 checks
```

Override the binary with `COUNCIL_BIN=/path/to/council`.

`openai_path.py` covers a full 3-round × 3-member deliberation, request
accounting, the round-1-blind / round-2-sees-peers property, persona injection,
artifacts, resume issuing zero HTTP calls, `--fresh`, the MCP stdio surface, and
config validation.

`anthropic_path.py` covers the Anthropic wire shape, custom auth headers,
`${ENV}` expansion, `thinking_delta` exclusion, the zero-text-response failure
mode, truncation detection, and partial-panel degradation.

`runtime_selection.py` covers the model registry, the `models` MCP endpoint
(including how an unusable model is flagged when its provider key is absent),
and per-call selection:
`--with` running exactly the chosen models, rounds driving the call count, chair
selection, aliases, the `provider:model` escape hatch, validation failures
before any API call, and the same knobs over MCP.

Requires Python 3.11+ (`tomllib`).

`research_tools.py` drives a fake provider that *requests tools*, so the
agentic loop is exercised end to end: both wire formats, fragmented tool
arguments, sandbox escape attempts, tool errors, loop bounding, and the
research audit trail.
