# Verification harnesses

Ad-hoc end-to-end verification. **Not** a unit-test suite — these drive the real
`council` binary against fake in-process SSE servers on loopback, so no tokens
are spent and no network access is needed.

```bash
cargo build
python3.11+ verify/openai_path.py       # 32 checks
python3.11+ verify/anthropic_path.py    # 19 checks
```

Override the binary with `COUNCIL_BIN=/path/to/council`.

`openai_path.py` covers a full 3-round × 3-member deliberation, request
accounting, the round-1-blind / round-2-sees-peers property, persona injection,
artifacts, resume issuing zero HTTP calls, `--fresh`, the MCP stdio surface, and
config validation.

`anthropic_path.py` covers the Anthropic wire shape, custom auth headers,
`${ENV}` expansion, `thinking_delta` exclusion, the zero-text-response failure
mode, truncation detection, and partial-panel degradation.

Requires Python 3.11+ (`tomllib`).
