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

Three tools appear:

| tool | purpose |
|---|---|
| `models` | list available model handles, their provider, and whether each is **usable** right now (provider known + API key present) |
| `panels` | list pre-configured rosters |
| `deliberate` | convene a council on a question, return a consensus document |

An agent's workflow is `models` → `deliberate(with: [...])`. The `models` output
also carries the selection guidance (diversity beats size, cost is
`members × rounds + 1`, what each round count buys), so the agent can size a run
sensibly without being told.

`deliberate` takes `question` (required) plus, all optional:

| arg | effect |
|---|---|
| `context` | background the panel needs; they know nothing else |
| `with` | pick the council at runtime, e.g. `["opus","sonnet","gpt"]` — overrides `panel` |
| `panel` | a named roster from config |
| `chair` | which member synthesises |
| `rounds` | 1–6, default 3 |
| `max_tokens` | per-member ceiling for this run |
| `code` | absolute dirs panellists may read/search (read-only) |
| `web` | allow fetching specs/docs/upstream source |
| `include_transcript` | return the full debate, not just the consensus |

Call `models` first to discover handles; `panels` for ready-made rosters.

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

### Letting the panel do its own research

By default a panellist is text-only: it can reason about your question and
whatever you pass as `context`, and nothing else. That means it *speculates*
about code it cannot see. Give it tools instead:

```bash
# panellists may read and search these directories (read-only)
council ask "Does our SSE parser corrupt multi-byte UTF-8 split across chunks?" \
  --with sonnet,sol --code ./src

# also allow fetching specs, docs, RFCs, upstream source
council ask "Are we using the OAuth device flow correctly?" \
  --with opus,sol --code ./src --web
```

Four read-only tools, offered only when you enable them:

| tool | needs | what it does |
|---|---|---|
| `read_file` | `--code` | read a file, with line numbers and offset/limit |
| `search_code` | `--code` | ripgrep the contents, returns `file:line` matches |
| `list_files` | `--code` | glob the tree to orient before reading |
| `fetch_url` | `--web` | HTTP GET, tags stripped |

**There is no shell, no write path, and no way out of the roots.** Paths are
rejected before touching disk if absolute or containing `..`, then
canonicalised and re-checked so a symlink pointing outward is caught too.

#### How a website sees council

Captured off the wire against `httpbin.org`, not inferred from the code:

```
GET /anything HTTP/1.1
Host: httpbin.org
User-Agent: council/0.1 (+https://github.com/NielsMooren/council)
Accept: */*
Accept-Encoding: gzip,br
```

No cookies, no `Accept-Language`, no `Referer`. The UA is honest and
attributable — it names the software, versions it, and links a real project,
following the `Googlebot/2.1 (+http://...)` convention. **Never** spoof a
browser or another crawler's identity.

Stack: `reqwest` 0.12 over `hyper` 1.x and `rustls` 0.23 (not OpenSSL), HTTP/1.1,
30s per-fetch timeout, up to 10 redirects.

#### Politeness limits

Fetching is throttled **per origin** (`scheme://host:port`, parsed by
`url::Url`), shared across every panellist in the run — four members researching
concurrently cannot each open their own quota on one server:

```bash
council ask "..." --web --host-delay-ms 1000 --host-budget 20   # defaults
```

- `--host-delay-ms` (default 1000): minimum gap between requests to the *same*
  host. Requests **wait**; the model still gets its answer, just not instantly.
- `--host-budget` (default 20): hard cap per host per deliberation. Exceeding it
  is an **error** returned to the model — a panellist making 20 requests to one
  host has stopped researching and started crawling.

Different origins do not block each other. Over MCP the defaults are fixed: a
program caller does not get to dial politeness down.

**Redirects are followed manually and every hop is charged.** reqwest's
automatic following is disabled, because it would turn one reservation on the
original origin into up to ten unmetered requests to arbitrary other origins — a
chain of redirectors could hammer a host at full rate while the limiter reported
everything was fine.

**Response bodies are size-capped during ingestion**, not after. `resp.text()`
buffers the whole body first, which with gzip and brotli enabled means a small
compressed response can expand into a large `String` and then get cloned twice
more. The limit gates the stream and rejects on an oversized `Content-Length`.

#### URL cache

Fetched pages are cached for 10 minutes and shared across the whole
deliberation, so a panel reading the same spec costs **one** request:

```bash
council ask "..." --web --cache-ttl 600    # default; 0 disables
```

The important part is **single-flight de-duplication**. A plain check-then-fill
cache does not help here: four members starting concurrently all miss, all
fetch, and the cache only benefits a fifth request that never comes. Each URL
gets its own lock held across the fetch, so concurrent readers of the same URL
queue behind one real request. Measured: two members fetching the same URL
simultaneously produce exactly one upstream hit.

- Cache hits pay **no** politeness delay and consume **no** host budget — they
  never touch the network.
- Hits are disclosed to the model (`(from cache: ...)`) so a panellist can say
  so if freshness matters to its argument.
- Failures are shared with peers already queued on the same URL for 5 seconds,
  then retried. Without that window a 30s timeout costs N fetches and N budget
  units — one per waiting member — instead of one of each.
- Entries are capped (256) with expiry-first eviction. TTL governs *freshness*,
  not retention, so without a cap a model naming unlimited URLs across unlimited
  hosts would grow memory unbounded; the per-host budget does not help because
  each new host gets its own.
- Keyed on the full URL, so distinct paths on one host are distinct entries.

#### Not implemented, and why

- **`robots.txt`** — RFC 9309 governs automated crawling, not a user asking for
  one page. Required before any recursive or bulk fetching.
- **Conditional GET** (`ETag`/`If-Modified-Since`) — cosmetic against a
  run-scoped 10-minute cache.
- **429/`Retry-After` backoff** — worth adding, not yet done.
- **Egress policy** (blocking private/link-local/cloud-metadata addresses) —
  `fetch_url` validates the scheme only, so it can reach anything the host can,
  including `169.254.169.254`. On a trusted laptop with URLs you choose this is
  equivalent to having `curl`. **It is not safe to expose to untrusted callers
  or untrusted URL content**, and partial validation that reads like a security
  boundary would be worse than none.

Every lookup is recorded in the transcript, so you can audit what a claim was
actually based on:

```
<research>
- search_code(pattern=from_utf8_lossy) -> provider.rs:303: // Buffer BYTES, not a String. (5 lines)
- read_file(limit=145, offset=260, path=provider.rs) -> 260|    } (145 lines)
- read_file(path=Cargo.toml) -> error: 'Cargo.toml' not found under any project root
</research>
```

The prompts change too: panellists are told not to speculate about anything
they can verify, to check a *peer's* load-bearing claims rather than accept
them, and to cite `file:line`. Cross-examination gains an explicit
"UNVERIFIED CLAIMS" heading.

The **chair gets no tools** — it synthesises what was argued, and must not
introduce evidence nobody debated.

The tool loop is bounded at 12 rounds per panellist. If a member burns its
budget and then fails to produce a summary, its findings are still reported
rather than the member being dropped.

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

9 unit tests plus 152 ad-hoc checks across five harnesses, run against fake in-process SSE servers
(no tokens spent):

- **OpenAI path (32):** full 3-round × 3-member run, request accounting, round-1
  blindness vs round-2 peer visibility, persona injection, artifacts, resume
  issuing zero HTTP calls, `--fresh`, MCP `initialize`/`tools/list`/`tools/call`,
  stdout purity, config validation.
- **Anthropic path (19):** wire shape, custom auth headers, `${ENV}` expansion,
  `thinking_delta` exclusion, the zero-text failure mode, truncation detection,
  partial-panel degradation.
- **Research tools (27):** tools off by default, `--code`/`--web` gating, the
  agentic loop on both wire formats (including fragmented tool arguments),
  sandbox escapes refused (absolute paths, `..`, nested traversal), tool errors
  handed back to the model rather than aborting, the loop being bounded, the
  audit trail, and cache-key separation for tool-enabled runs.
- **Web politeness & caching (41):** `Accept-Encoding` actually on the wire,
  per-host spacing *measured* from arrival timestamps, spacing shared across
  concurrent members, different hosts not serialised, the budget as a hard stop
  with an explanatory refusal, concurrent members collapsing to one fetch,
  cache hits bypassing both the delay and the budget, distinct URLs cached
  separately, `--cache-ttl 0` disabling it, every redirect hop being metered, an
  oversized body refused before buffering, and a hung fetch coalesced rather
  than multiplied across members.
- **Runtime selection (33):** registry discovery, `--with` running exactly the
  chosen models, round count driving call count, chair selection, aliases, the
  `provider:model` escape hatch, rejection of unknown handles / bad chairs /
  one-member panels, `--with` overriding `--panel`, `--max-tokens`, the `models`
  MCP endpoint (including unusable-model flagging), and the same knobs over MCP.

`cargo test` runs 9 unit tests covering the pure logic — origin parsing
(including IPv6 literals, ports, credentials and case), HTML stripping, and the
rate limiter and cache against a **paused tokio clock**, so they are
deterministic, need no sockets, and finish in 0.00s.

`cargo clippy --all-targets` and `cargo fmt --check` are clean, with zero
suppressions.

## Licence

MIT OR Apache-2.0
