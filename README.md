# council

**Multi-model deliberation as an MCP tool.** Several LLMs — different vendors,
different training — debate a question across rounds, challenge each other, then
a chair synthesises where they genuinely agree and where they don't.

Call it from an AI session, or from your shell.

```
$ council ask "Should we migrate from REST to gRPC for the internal mesh?"
```

## Why

A single model states unverified assumptions with total confidence. Ask three
differently-trained models and make them read each other's arguments, and the
disagreements point straight at the weak premise.

This was extracted from a real session where a 4-model panel reviewed a design
document three times. It overturned three of the author's decisions, and one
model caught a contradiction between two claims in the same document that
nobody — human or model — had noticed. That is the failure mode this exists for.

## Install

```bash
cargo install --path .          # or: cargo install --git https://github.com/NielsMooren/council
council init                    # writes ~/.council/config.toml
$EDITOR ~/.council/config.toml
export OPENAI_API_KEY=...       # whatever your config names
council check                   # validates config + key presence
```

## Use from an AI session (MCP)

Add to your MCP client config:

```json
{
  "mcpServers": {
    "council": { "command": "council", "args": ["serve"] }
  }
}
```

For Hermes: `hermes mcp add council -- council serve`

Two tools appear:

| tool | purpose |
|---|---|
| `deliberate` | convene the panel on a question, return a consensus document |
| `panels` | list configured panels and providers |

`deliberate` takes `question` (required), plus optional `context`, `panel`,
`rounds` (1–6, default 3), `include_transcript`.

**When to reach for it:** consequential, contestable decisions — architecture,
risky trade-offs, plan review, "is this design sound". **When not to:** factual
lookups or anything with one correct answer. You will pay N times for the same
reply.

## Use from the shell

```bash
council ask "Is event sourcing right for this audit log?"
council ask "Review this plan" -x "$(cat PLAN.md)" --rounds 4
council ask "..." -x -               # read context from stdin
council ask "..." --panel security --transcript
council panels
```

## Configuration

Providers describe a **wire protocol**, not a vendor — so any OpenAI- or
Anthropic-compatible endpoint works: OpenAI, Anthropic, Azure, OpenRouter,
Groq, Together, vLLM, Ollama, LiteLLM, or a corporate gateway.

```toml
[[providers]]
name = "openai"
api = "openai_chat"                 # or "anthropic_messages"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"      # env var name; secrets never live in config
auth = "bearer"                     # "bearer" | "x_api_key" | "api_key" | { header = "..." }

[[providers]]
name = "work-gateway"
api = "anthropic_messages"
base_url = "https://gateway.corp.example/anthropic"
api_key_env = "WORK_KEY"
auth = { header = "api-key" }       # gateways love inventing their own header
headers = { "anthropic-version" = "2023-06-01", "x-trace" = "${TRACE_ID}" }
```

Panels mix providers deliberately, and members get **personas rather than model
names** — peers should argue with the argument, not defer to whichever model
sounds most authoritative.

```toml
[[panels]]
name = "default"
chair = "Chair"

  [[panels.members]]
  name = "Pragmatist"
  provider = "openai"
  model = "gpt-5.5"
  persona = "You optimise for what ships this week and holds in production."

  [[panels.members]]
  name = "Skeptic"
  provider = "anthropic"
  model = "claude-sonnet-4-5"
  persona = "You hunt unverified assumptions. Demand evidence for load-bearing claims."
```

Define as many panels as you like (`security`, `perf`, `cheap`) and pick one per
call.

## How a deliberation runs

1. **Round 1 — opening positions, in parallel, with no peer input.** Showing
   peers early causes premature convergence and destroys the diversity you are
   paying for.
2. **Round 2…n−1 — cross-examination.** Each member reads every position and
   must state where they were wrong and who changed their mind.
3. **Final round — commitment.** A decision, even in the minority.
4. **Chair synthesis**, explicitly instructed not to manufacture consensus.

Everything lands in `~/.council/runs/<hash>/`: per-member responses per round,
`transcript.md`, `consensus.md`.

## Design decisions worth knowing

These are not preferences. Each one is a bug that cost real debugging time.

**Streaming is mandatory.** nginx-fronted gateways return **504** while waiting
for the first byte of a large non-streamed response — reproduced
deterministically on a ~56KB prompt. Streaming emits bytes immediately, so the
proxy never idles out. Retries do not help; this is structural.

**Anthropic thinking is disabled by default.** Some gateways let a reasoning
model spend its *entire* `max_tokens` budget on `thinking` and return **zero
text blocks** while reporting `stop_reason: end_turn`. Measured: 2996 output
tokens, 2995 of them thinking, empty answer. Set `disable_thinking = false` if
you want reasoning and have budget headroom.

**Truncation is a hard error.** A silently clipped argument poisons every later
round, because the panel then reasons confidently from half a claim.

**Runs are resumable.** Each response is cached by
`question + panel + round + member`, so a killed run costs nothing to restart.

**One dead model does not kill the panel.** Failures are collected, reported in
the output, and the remaining members continue — a 2-of-4 panel is a weaker
signal and the caller is told so explicitly.

**`max_completion_tokens` vs `max_tokens`** is selected per wire format; newer
OpenAI reasoning models hard-reject the latter with a 400.

## Verification

51 ad-hoc checks across two harnesses, run against fake in-process SSE servers
(no tokens spent):

- **OpenAI path (32):** full 3-round × 3-member run, request accounting, round-1
  blindness vs round-2 peer visibility, persona injection, artifacts, resume
  issuing zero HTTP calls, `--fresh`, MCP `initialize`/`tools/list`/`tools/call`,
  stdout purity, config validation.
- **Anthropic path (19):** wire shape, custom auth headers, `${ENV}` expansion,
  `thinking_delta` exclusion, the zero-text failure mode, truncation detection,
  partial-panel degradation.

`cargo clippy --all-targets` and `cargo fmt --check` are clean. There is no
unit-test suite yet — the harnesses are the evidence, and they exercise the real
binary end to end.

## Licence

MIT OR Apache-2.0
