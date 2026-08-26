//! Provider abstraction: one trait, N wire formats.
//!
//! Every quirk encoded here was learned the hard way against a real gateway.
//! Read the comments before "simplifying" anything.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Wire protocol. Not the vendor — a vendor may speak several, and gateways
/// (`LiteLLM`, `OpenRouter`, corporate proxies) usually speak one of these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Api {
    /// POST /chat/completions — `OpenAI`, Azure, `OpenRouter`, Groq, vLLM, Ollama…
    OpenaiChat,
    /// POST /v1/messages — Anthropic and Anthropic-compatible gateways.
    AnthropicMessages,
}

/// How to authenticate. Gateways differ here more than anywhere else.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    /// `Authorization: Bearer <key>` — the common case.
    Bearer,
    /// `x-api-key: <key>` — Anthropic direct.
    XApiKey,
    /// `api-key: <key>` — Azure `OpenAI` and several corporate gateways.
    ApiKey,
    /// Arbitrary header name, for gateways that invent their own.
    Header(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub api: Api,
    /// Base URL *without* the endpoint path, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    /// Env var holding the key. Never put secrets in the config file.
    pub api_key_env: String,
    #[serde(default = "default_auth")]
    pub auth: Auth,
    /// Extra headers, e.g. `anthropic-version`. Values may use `${ENV_VAR}`.
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    /// Disable Anthropic extended thinking.
    ///
    /// WHY THIS EXISTS: some gateways let a reasoning model spend its *entire*
    /// `max_tokens` budget on `thinking` and return **zero text blocks** while
    /// still reporting `stop_reason: end_turn`. Measured: 2996 output tokens,
    /// 2995 of them thinking, empty response. Looks like success, yields
    /// nothing. Default on for Anthropic; set false if you want reasoning.
    #[serde(default = "default_true")]
    pub disable_thinking: bool,
}

const fn default_auth() -> Auth {
    Auth::Bearer
}
const fn default_true() -> bool {
    true
}

/// One panellist: a display name plus the provider/model that backs it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    /// Shown to peers in the transcript. Use a persona ("Skeptic"), not a
    /// model id — panellists should argue, not defer to whoever sounds biggest.
    pub name: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Optional extra instruction, e.g. "argue the security angle".
    #[serde(default)]
    pub persona: Option<String>,
}

pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub max_tokens: u32,
    /// Tools this call may use. An empty toolbox = plain single-shot completion.
    pub tools: &'a crate::tools::Toolbox,
}

impl Provider {
    fn key(&self) -> Result<String> {
        std::env::var(&self.api_key_env)
            .with_context(|| format!("env var {} is not set", self.api_key_env))
    }

    /// Expand `${VAR}` in header values so `anthropic-version` style statics and
    /// env-backed secrets can live side by side.
    fn expand(v: &str) -> String {
        // Split on the delimiters rather than byte-slicing: `${` / `}` are ASCII
        // but the surrounding value need not be, and str slicing panics on a
        // non-char-boundary index.
        let mut out = String::with_capacity(v.len());
        let mut rest = v;
        while let Some((before, after)) = rest.split_once("${") {
            let Some((var, tail)) = after.split_once('}') else {
                // Unterminated `${` - emit the remainder verbatim.
                out.push_str(before);
                out.push_str("${");
                out.push_str(after);
                return out;
            };
            out.push_str(before);
            out.push_str(&std::env::var(var).unwrap_or_default());
            rest = tail;
        }
        out.push_str(rest);
        out
    }

    fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.api {
            Api::OpenaiChat => format!("{base}/chat/completions"),
            Api::AnthropicMessages => format!("{base}/v1/messages"),
        }
    }

    /// Build the request body for the current conversation state.
    ///
    /// `msgs` is the running exchange: the model's tool requests and our results
    /// get appended, so a tool-using turn is just this called repeatedly.
    fn body(&self, r: &Request<'_>, msgs: &[Value]) -> Value {
        let schemas = r.tools.schemas(self.api);
        match self.api {
            // `max_completion_tokens`, not `max_tokens`: newer OpenAI reasoning
            // models hard-reject the latter with a 400.
            Api::OpenaiChat => {
                let mut all = vec![json!({"role": "system", "content": r.system})];
                all.extend_from_slice(msgs);
                let mut b = json!({
                    "model": r.model,
                    "max_completion_tokens": r.max_tokens,
                    "stream": true,
                    "messages": all,
                });
                if let Some(map) = b.as_object_mut() {
                    if !schemas.is_empty() {
                        map.insert("tools".into(), json!(schemas));
                    }
                }
                b
            }
            Api::AnthropicMessages => {
                let mut b = json!({
                    "model": r.model,
                    "max_tokens": r.max_tokens,
                    "stream": true,
                    "system": r.system,
                    "messages": msgs,
                });
                if let Some(map) = b.as_object_mut() {
                    if self.disable_thinking {
                        map.insert("thinking".into(), json!({"type": "disabled"}));
                    }
                    if !schemas.is_empty() {
                        map.insert("tools".into(), json!(schemas));
                    }
                }
                b
            }
        }
    }

    /// Run one panellist turn to completion, servicing any tool calls.
    ///
    /// STREAMING IS MANDATORY, not an optimisation: nginx-fronted gateways
    /// return **504 Gateway Timeout** while waiting for the first byte of a
    /// large non-streamed response. Reproduced deterministically on a ~56KB
    /// prompt. Streaming emits bytes immediately so the proxy never idles out.
    ///
    /// With a non-empty toolbox this is an agentic loop: the model asks for a
    /// file or a search, we run it, append the result, and ask again.
    pub async fn complete(&self, http: &reqwest::Client, r: Request<'_>) -> Result<String> {
        const MAX_TOOL_ROUNDS: usize = 12;

        let mut msgs = vec![json!({"role": "user", "content": r.user})];
        // Kept so the transcript shows what each panellist actually checked -
        // that audit trail is the whole point of giving them tools.
        let mut findings: Vec<String> = Vec::new();

        for _ in 0..MAX_TOOL_ROUNDS {
            let turn = self.one_turn(http, &r, &msgs).await?;
            if turn.calls.is_empty() {
                let text = Self::check(&turn.text, turn.stop.as_deref())?;
                return Ok(with_research(text, &findings));
            }
            msgs.push(self.assistant_turn(&turn));
            for call in &turn.calls {
                let out = r.tools.call(http, &call.name, &call.args).await;
                findings.push(format!(
                    "- {}({}) -> {}",
                    call.name,
                    compact(&call.args),
                    first_line(&out)
                ));
                msgs.push(self.tool_result(call, &out));
            }
        }

        // Out of tool budget: ask once more with tools withheld so we still get
        // prose rather than an endless request loop.
        let closing = Request {
            tools: &crate::tools::Toolbox::default(),
            ..r
        };
        let turn = self.one_turn(http, &closing, &msgs).await?;
        // Do NOT lose a panellist that did all its research and then fumbled the
        // final message - the findings are the expensive part. Fall back to
        // reporting them rather than failing the member out of the round.
        match Self::check(&turn.text, turn.stop.as_deref()) {
            Ok(text) => Ok(with_research(text, &findings)),
            Err(e) if findings.is_empty() => Err(e),
            Err(e) => Ok(with_research(
                format!(
                    "(No summary produced: {e}. Research findings below are all this member \
                     established; weigh them accordingly.)"
                ),
                &findings,
            )),
        }
    }

    /// The assistant message echoing the model's tool requests back to it.
    fn assistant_turn(&self, turn: &Turn) -> Value {
        match self.api {
            Api::OpenaiChat => json!({
                "role": "assistant",
                "content": if turn.text.is_empty() { Value::Null } else { json!(turn.text) },
                "tool_calls": turn.calls.iter().map(|c| json!({
                    "id": c.id,
                    "type": "function",
                    "function": {"name": c.name, "arguments": c.args.to_string()}
                })).collect::<Vec<_>>()
            }),
            Api::AnthropicMessages => {
                let mut content = Vec::new();
                if !turn.text.is_empty() {
                    content.push(json!({"type": "text", "text": turn.text}));
                }
                for c in &turn.calls {
                    content.push(json!({
                        "type": "tool_use", "id": c.id, "name": c.name, "input": c.args
                    }));
                }
                json!({"role": "assistant", "content": content})
            }
        }
    }

    /// The message carrying a tool's output back to the model.
    fn tool_result(&self, call: &crate::tools::ToolCall, out: &str) -> Value {
        match self.api {
            Api::OpenaiChat => json!({
                "role": "tool", "tool_call_id": call.id, "content": out
            }),
            Api::AnthropicMessages => json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": call.id, "content": out}]
            }),
        }
    }

    /// One HTTP round trip.
    async fn one_turn(
        &self,
        http: &reqwest::Client,
        r: &Request<'_>,
        msgs: &[Value],
    ) -> Result<Turn> {
        let mut req = http
            .post(self.endpoint())
            .json(&self.body(r, msgs))
            .header("content-type", "application/json");

        let key = self.key()?;
        req = match &self.auth {
            Auth::Bearer => req.header("authorization", format!("Bearer {key}")),
            Auth::XApiKey => req.header("x-api-key", key),
            Auth::ApiKey => req.header("api-key", key),
            Auth::Header(h) => req.header(h.as_str(), key),
        };
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), Self::expand(v));
        }

        let resp = req.send().await.context("request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "HTTP {status}: {}",
                body.chars().take(400).collect::<String>()
            );
        }
        self.read_stream(resp).await
    }

    /// Consume the SSE body into text, stop reason, and any tool calls.
    ///
    /// Both APIs stream tool arguments as *fragments* of a JSON string, so they
    /// are accumulated per index and parsed only once the stream ends.
    async fn read_stream(&self, resp: reqwest::Response) -> Result<Turn> {
        let mut stream = resp.bytes_stream();
        // Buffer BYTES, not a String. Decoding each chunk with from_utf8_lossy
        // would corrupt any multi-byte character split across a chunk boundary,
        // and byte-slicing a String panics on a non-char boundary. Split on the
        // newline byte, then decode whole lines.
        let mut buf = Vec::<u8>::new();
        let mut acc = Acc::default();

        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.context("stream error")?);
            // SSE frames are newline-delimited; keep the partial tail in `buf`.
            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(payload) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                // Non-JSON keep-alive frames are normal; skip, never fail.
                let Ok(ev) = serde_json::from_str::<Value>(payload) else {
                    continue;
                };
                match self.api {
                    Api::OpenaiChat => acc.openai_frame(&ev),
                    Api::AnthropicMessages => acc.anthropic_frame(&ev),
                }
            }
        }
        Ok(acc.finish())
    }

    /// Reject the two silent-failure shapes: empty output, and truncation.
    fn check(text: &str, stop: Option<&str>) -> Result<String> {
        let text = text.trim().to_owned();
        if text.is_empty() {
            bail!(
                "empty response (stop={stop:?}) — if this is a reasoning model, thinking tokens may have consumed the whole budget; keep disable_thinking = true or raise max_tokens"
            );
        }
        // Truncation must be loud. A silently clipped argument poisons every
        // later round, and the panel will confidently reason from half a claim.
        if matches!(stop, Some("length" | "max_tokens")) {
            bail!(
                "truncated at {} chars (stop={stop:?}) — raise max_tokens",
                text.len()
            );
        }
        Ok(text)
    }
}

/// Accumulates a streamed response. `calls` is keyed by stream index because
/// providers emit tool arguments as fragments that must be stitched in order.
#[derive(Default)]
struct Acc {
    text: String,
    stop: Option<String>,
    calls: std::collections::BTreeMap<u64, PartialCall>,
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

impl Acc {
    fn slot(&mut self, ev: &Value) -> &mut PartialCall {
        let idx = ev.get("index").and_then(Value::as_u64).unwrap_or(0);
        self.calls.entry(idx).or_default()
    }

    /// One `OpenAI` SSE frame: prose deltas, tool-call fragments, finish reason.
    fn openai_frame(&mut self, ev: &Value) {
        let Some(choice) = ev.get("choices").and_then(|c| c.get(0)) else {
            return;
        };
        let delta = choice.get("delta");
        if let Some(s) = delta.and_then(|d| d.get("content")).and_then(Value::as_str) {
            self.text.push_str(s);
        }
        if let Some(calls) = delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for tc in calls {
                let slot = self.slot(tc);
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    id.clone_into(&mut slot.id);
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(Value::as_str) {
                        slot.name.push_str(n);
                    }
                    if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                        slot.args.push_str(a);
                    }
                }
            }
        }
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop = Some(fr.to_owned());
        }
    }

    /// One Anthropic SSE frame. Note the three delta types: `text_delta` is
    /// prose, `input_json_delta` is tool arguments, and `thinking_delta` is
    /// neither and must be dropped.
    fn anthropic_frame(&mut self, ev: &Value) {
        let kind = ev.get("type").and_then(Value::as_str);
        let delta = ev.get("delta");
        match kind {
            Some("content_block_start") => {
                let block = ev.get("content_block");
                if block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use") {
                    let id = block
                        .and_then(|b| b.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let name = block
                        .and_then(|b| b.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let slot = self.slot(ev);
                    slot.id = id;
                    slot.name = name;
                }
            }
            Some("content_block_delta") => {
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(s) = delta.and_then(|d| d.get("text")).and_then(Value::as_str) {
                            self.text.push_str(s);
                        }
                    }
                    Some("input_json_delta") => {
                        let frag = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        self.slot(ev).args.push_str(&frag);
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(sr) = delta
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop = Some(sr.to_owned());
                }
            }
            _ => {}
        }
    }

    fn finish(self) -> Turn {
        let calls = self
            .calls
            .into_values()
            .filter(|c| !c.name.is_empty())
            .map(|c| crate::tools::ToolCall {
                id: c.id,
                name: c.name,
                // A model can emit malformed JSON; pass it through and let the
                // tool report the problem rather than failing the whole round.
                args: serde_json::from_str(if c.args.trim().is_empty() {
                    "{}"
                } else {
                    &c.args
                })
                .unwrap_or_else(|_| json!({"_raw": c.args})),
            })
            .collect();
        Turn {
            text: self.text.trim().to_owned(),
            stop: self.stop,
            calls,
        }
    }
}

/// One model turn: prose, why it stopped, and any tools it wants run.
pub struct Turn {
    pub text: String,
    pub stop: Option<String>,
    pub calls: Vec<crate::tools::ToolCall>,
}

/// Attach the research trail so the transcript records what was actually checked.
fn with_research(text: String, findings: &[String]) -> String {
    if findings.is_empty() {
        return text;
    }
    format!("{text}\n\n<research>\n{}\n</research>", findings.join("\n"))
}

/// Render tool args compactly for the research trail.
fn compact(args: &Value) -> String {
    args.as_object().map_or_else(
        || args.to_string(),
        |m| {
            m.iter()
                .map(|(k, v)| {
                    let val = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
                    format!("{k}={}", val.chars().take(60).collect::<String>())
                })
                .collect::<Vec<_>>()
                .join(", ")
        },
    )
}

/// First meaningful line of a tool result, for the research trail.
fn first_line(out: &str) -> String {
    let line = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let head: String = line.chars().take(110).collect();
    let n = out.lines().count();
    if n > 1 {
        format!("{head} ({n} lines)")
    } else {
        head
    }
}
