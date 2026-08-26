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

`deliberate` takes `question` (required) plus, all optional:

| arg | effect |
|---|---|
| `context` | background the panel needs; they know nothing else |
| `with` | pick the council at runtime, e.g. `["opus","sonnet","gpt"]` — overrides `panel` |
| `panel` | a named roster from config |
| `chair` | which member synthesises |
| `rounds` | 1–6, default 3 |
| `max_tokens` | per-member ceiling for this run |
| `include_transcript` | return the full debate, not just the consensus |

Call `panels` first to see the registry handles and rosters.

**When to reach for it:** consequential, contestable decisions — architecture,
risky trade-offs, plan review, "is this design sound". **When not to:** factual
lookups or anything with one correct answer. You will pay N times for the same
reply.

## Choosing the council at runtime

Register your models once, then pick who sits on the council per call.

```bash
council models                       # the handles you can use
# HANDLE         PROVIDER     MODEL
# gpt            openai       gpt-5.5
# sonnet         anthropic    claude-sonnet-4-5
# opus           anthropic    claude-opus-4-5

council ask "Is event sourcing right here?" --with sonnet,gpt
council ask "..." --with opus,sonnet,gpt --rounds 4 --chair opus
council ask "..." --with Hawk=gpt,Dove=sonnet          # rename for the transcript
council ask "..." --with sonnet,openai:gpt-4.1         # one-off, unregistered
council ask "..." --with sonnet,gpt --max-tokens 4000  # cheaper run
```

`--with` overrides `--panel`. Everything is validated *before* the first API
call, so an unknown handle, a chair who is not a member, or a one-member
"panel" fails instantly rather than after two paid rounds.

### Picking rounds

| rounds | shape | use for |
|---|---|---|
| 1 | independent opinions, no cross-talk | a quick spread of views; cheapest |
| 2 | + cross-examination | most decisions |
| **3** | + commitment (**default**) | consequential calls |
| 4–6 | more cross-examination | genuinely contested designs |

Cost scales as `members × rounds + 1` (the `+1` is the chair). Two models at
1 round is 3 calls; four models at 4 rounds is 17. Start small.

### Efficiency

- **Diversity beats size.** Two models from different vendors disagree more
  usefully than three from one. The disagreement is the product.
- **Runs resume.** Identical question + panel + rounds reuses cached responses
  and issues zero calls. Re-run freely; add `--fresh` to force.
- **Widen after you narrow.** Ask 2 models at 1 round first. If they already
  agree, you are done. If they split, re-ask with more members and rounds.
- **Cap output** with `--max-tokens` for exploratory runs.

## Use from the shell

```bash
council ask "Review this plan" -x "$(cat PLAN.md)" --rounds 4
council ask "..." -x -               # read context from stdin
council ask "..." --panel security --transcript
council models                       # the registry
council panels                       # rosters + key presence
council check                        # validate config
```

## Configuration

Three layers, each referencing the one above by name:

```
[[providers]]  a wire endpoint + auth          "anthropic", "work-gateway"
[[models]]     a named model on a provider     "opus", "sonnet", "gpt"
[[panels]]     a reusable roster of models     "default", "security"
```

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

The **model registry** is what makes runtime selection ergonomic — name a model
once, then refer to it by that handle everywhere:

```toml
[[models]]
name = "opus"                       # the handle `--with` takes
provider = "anthropic"
model = "claude-opus-4-5"
persona = "You weigh trade-offs and refuse to manufacture agreement."
max_tokens = 16000                  # optional per-model ceiling
```

Panels are reusable rosters that reference the registry. They mix providers
deliberately, and members get **personas rather than model names** — peers
should argue with the argument, not defer to whichever model sounds most
authoritative.

```toml
[[panels]]
name = "default"
chair = "Chair"

  [[panels.members]]
  model = "gpt"                     # registry handle
  name = "Pragmatist"               # renamed for the transcript
  persona = "You optimise for what ships this week and holds in production."

  [[panels.members]]
  model = "sonnet"
  name = "Skeptic"
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

## Code standards

This crate follows the strict-lint standard from
[namtao.com/rust](https://www.namtao.com/rust/): `clippy::pedantic` and
`clippy::nursery` at `deny`, plus an explicit deny-list for everything that can
panic at runtime — `unwrap_used`, `expect_used`, `indexing_slicing`,
`arithmetic_side_effects`, `panic`, `exit`, `as_conversions`, `string_slice` and
friends. See `[lints.clippy]` in `Cargo.toml`.

`clippy.toml` re-permits `unwrap`/`expect`/`panic`/indexing **in tests only**, so
prototyping stays fast where a panic is just a failed assertion.

```bash
cargo clippy --all-targets   # zero errors, zero warnings, zero suppressions
cargo fmt --check
```

There is not a single `#[allow(clippy::…)]` or `#[expect(clippy::…)]` in `src/`.
Every one of the 44 initial violations was fixed rather than silenced.

Adopting this standard was not cosmetic — it found three reachable panics in the
first pass:

- `Value["key"]` indexing in the SSE parser, which panics on an unexpected frame
  shape from a provider. Now `.get()` throughout.
- `buf[..nl]` byte-slicing a `String` of streamed data, which panics if a
  multi-byte UTF-8 character straddles the slice index. The buffer is now
  `Vec<u8>`, split on the newline byte and decoded per whole line — which also
  fixed a latent corruption bug, since `from_utf8_lossy` per chunk mangles any
  character split across a chunk boundary.
- `format!("{:x}", hash)[..16]` in the cache key, same class of bug.

If you extend this crate, keep the lints on. They pay for themselves.

## Verification

78 ad-hoc checks across three harnesses, run against fake in-process SSE servers
(no tokens spent):

- **OpenAI path (32):** full 3-round × 3-member run, request accounting, round-1
  blindness vs round-2 peer visibility, persona injection, artifacts, resume
  issuing zero HTTP calls, `--fresh`, MCP `initialize`/`tools/list`/`tools/call`,
  stdout purity, config validation.
- **Anthropic path (19):** wire shape, custom auth headers, `${ENV}` expansion,
  `thinking_delta` exclusion, the zero-text failure mode, truncation detection,
  partial-panel degradation.
- **Runtime selection (27):** registry discovery, `--with` running exactly the
  chosen models, round count driving call count, chair selection, aliases, the
  `provider:model` escape hatch, rejection of unknown handles / bad chairs /
  one-member panels, `--with` overriding `--panel`, `--max-tokens`, and the same
  knobs over MCP.

`cargo clippy --all-targets` and `cargo fmt --check` are clean. There is no
unit-test suite yet — the harnesses are the evidence, and they exercise the real
binary end to end.

## Licence

MIT OR Apache-2.0
