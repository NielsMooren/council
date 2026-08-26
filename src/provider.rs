//! Provider abstraction: one trait, N wire formats.
//!
//! Every quirk encoded here was learned the hard way against a real gateway.
//! Read the comments before "simplifying" anything.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

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

    fn body(&self, r: &Request<'_>) -> serde_json::Value {
        use serde_json::json;
        match self.api {
            // `max_completion_tokens`, not `max_tokens`: newer OpenAI reasoning
            // models hard-reject the latter with a 400.
            Api::OpenaiChat => json!({
                "model": r.model,
                "max_completion_tokens": r.max_tokens,
                "stream": true,
                "messages": [
                    {"role": "system", "content": r.system},
                    {"role": "user", "content": r.user},
                ],
            }),
            Api::AnthropicMessages => {
                let mut b = json!({
                    "model": r.model,
                    "max_tokens": r.max_tokens,
                    "stream": true,
                    "system": r.system,
                    "messages": [{"role": "user", "content": r.user}],
                });
                if self.disable_thinking {
                    // Insert via the map: `b["k"] = v` panics if `b` is not an object.
                    if let Some(map) = b.as_object_mut() {
                        map.insert("thinking".into(), json!({"type": "disabled"}));
                    }
                }
                b
            }
        }
    }

    /// Stream a completion to a single String.
    ///
    /// STREAMING IS MANDATORY, not an optimisation: nginx-fronted gateways
    /// return **504 Gateway Timeout** while waiting for the first byte of a
    /// large non-streamed response. Reproduced deterministically on a ~56KB
    /// prompt. Streaming emits bytes immediately so the proxy never idles out.
    pub async fn complete(&self, http: &reqwest::Client, r: Request<'_>) -> Result<String> {
        let mut req = http
            .post(self.endpoint())
            .json(&self.body(&r))
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

        let (text, stop) = self.read_stream(resp).await?;
        Self::check(&text, stop.as_deref())
    }

    /// Consume the SSE body into `(text, stop_reason)`.
    async fn read_stream(&self, resp: reqwest::Response) -> Result<(String, Option<String>)> {
        let mut stream = resp.bytes_stream();
        // Buffer BYTES, not a String. Decoding each chunk with from_utf8_lossy
        // would corrupt any multi-byte character split across a chunk boundary,
        // and byte-slicing a String panics on a non-char boundary. Split on the
        // newline byte, then decode whole lines.
        let (mut buf, mut text, mut stop) = (Vec::<u8>::new(), String::new(), None);

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
                let Ok(ev) = serde_json::from_str::<serde_json::Value>(payload) else {
                    continue;
                };
                // `.get()` throughout rather than `[]`: Value indexing panics on
                // a type mismatch, and a stray frame shape must never abort a run.
                match self.api {
                    Api::OpenaiChat => {
                        let Some(choice) = ev.get("choices").and_then(|c| c.get(0)) else {
                            continue;
                        };
                        if let Some(s) = choice
                            .get("delta")
                            .and_then(|d| d.get("content"))
                            .and_then(serde_json::Value::as_str)
                        {
                            text.push_str(s);
                        }
                        if let Some(fr) = choice
                            .get("finish_reason")
                            .and_then(serde_json::Value::as_str)
                        {
                            stop = Some(fr.to_owned());
                        }
                    }
                    Api::AnthropicMessages => {
                        let delta = ev.get("delta");
                        match ev.get("type").and_then(serde_json::Value::as_str) {
                            // Only `text_delta` — `thinking_delta` is not an answer.
                            Some("content_block_delta") => {
                                let is_text = delta
                                    .and_then(|d| d.get("type"))
                                    .and_then(serde_json::Value::as_str)
                                    == Some("text_delta");
                                if is_text {
                                    if let Some(s) = delta
                                        .and_then(|d| d.get("text"))
                                        .and_then(serde_json::Value::as_str)
                                    {
                                        text.push_str(s);
                                    }
                                }
                            }
                            Some("message_delta") => {
                                if let Some(sr) = delta
                                    .and_then(|d| d.get("stop_reason"))
                                    .and_then(serde_json::Value::as_str)
                                {
                                    stop = Some(sr.to_owned());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok((text, stop))
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
