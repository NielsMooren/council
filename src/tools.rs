//! Research tools panellists may call to answer their own questions.
//!
//! Deliberately narrow and **read-only**. A panellist's job is to form an
//! argument, not to change your machine, so every tool here either reads a file
//! under an explicit root or performs an HTTP GET. There is no shell, no write
//! path, and no way to reach outside the configured roots.
//!
//! Off by default: a run only gets tools when the caller passes roots (or
//! enables web search). Silence is the safe default.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Per-origin politeness limiter.
///
/// Enforces a minimum gap between requests to the *same* host and a hard cap on
/// total requests to it per deliberation. Per-origin rather than global, because
/// a global limit both over-throttles a panellist reading two unrelated docs and
/// under-protects a single host being hammered.
///
/// Shared across panellists via `Arc`, so four members researching concurrently
/// cannot each open their own quota on one poor server.
#[derive(Debug, Clone)]
pub struct RateLimit {
    /// Minimum spacing between requests to one host.
    pub min_interval: Duration,
    /// Max requests to a single host per deliberation.
    pub max_per_host: u32,
    state: Arc<Mutex<HashMap<String, HostState>>>,
}

#[derive(Debug)]
struct HostState {
    last: Option<Instant>,
    count: u32,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            // 1 req/s to a host is the conventional crawl-delay floor and well
            // inside what any server tolerates.
            min_interval: Duration::from_millis(1000),
            max_per_host: 20,
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl RateLimit {
    /// Build a limiter with explicit limits.
    pub fn new(min_interval: Duration, max_per_host: u32) -> Self {
        Self {
            min_interval,
            max_per_host,
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Wait until it is polite to call `host`, or refuse if the budget is spent.
    ///
    /// Sleeping (rather than erroring) on the interval is deliberate: the model
    /// asked a reasonable question and should get an answer, just not instantly.
    /// The *budget* is a hard error because a panellist making 20 requests to one
    /// host has stopped researching and started crawling.
    async fn acquire(&self, host: &str) -> Result<()> {
        // The guard must cover the whole read-modify-write, or two concurrent
        // panellists both see the same `last` and fire simultaneously. It is
        // dropped explicitly before sleeping so nobody waits on the lock while
        // we wait on the clock.
        let mut map = self.state.lock().await;
        let entry = map.entry(host.to_owned()).or_insert(HostState {
            last: None,
            count: 0,
        });
        if entry.count >= self.max_per_host {
            bail!(
                "rate limit: already made {} requests to {host} in this deliberation; \
                 refusing more. Use what you have or cite it as unverifiable.",
                entry.count
            );
        }
        entry.count = entry.count.saturating_add(1);

        // Chain each slot off the PREVIOUS reservation, not off "now".
        //
        // The naive version (`wait = min_interval - (now - last)`, then
        // `last = now`) lets two concurrent callers both compute wait=0 and fire
        // together - measured as gaps of [303, 4, 297, 6] ms with two members.
        // Reserving `prev + min_interval` makes the slots strictly sequential
        // however many panellists race for them.
        let now = Instant::now();
        let slot = entry.last.map_or(now, |prev| {
            let next = prev
                .checked_add(self.min_interval)
                .unwrap_or_else(Instant::now);
            if next > now { next } else { now }
        });
        entry.last = Some(slot);
        let wait = slot.checked_duration_since(now);
        drop(map);

        if let Some(d) = wait {
            tokio::time::sleep(d).await;
        }
        Ok(())
    }
}

/// What a deliberation is allowed to look at.
#[derive(Debug, Clone, Default)]
pub struct Toolbox {
    /// Directories panellists may read and search. Empty = no filesystem access.
    pub roots: Vec<PathBuf>,
    /// Allow HTTP GET of documentation/spec URLs.
    pub web: bool,
    /// Max bytes returned from any single read, so one huge file cannot blow the
    /// context window of every subsequent round.
    pub max_bytes: usize,
    /// Per-origin politeness limiter, shared by every panellist in the run.
    pub rate: RateLimit,
}

impl Toolbox {
    pub const DEFAULT_MAX_BYTES: usize = 24_000;

    pub const fn is_empty(&self) -> bool {
        self.roots.is_empty() && !self.web
    }

    const fn cap(&self) -> usize {
        if self.max_bytes == 0 {
            Self::DEFAULT_MAX_BYTES
        } else {
            self.max_bytes
        }
    }

    /// JSON tool schemas, in whichever dialect the provider speaks.
    pub fn schemas(&self, api: crate::provider::Api) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.roots.is_empty() {
            out.push(spec(
                api,
                "read_file",
                "Read a UTF-8 text file from the project under review. Use this to check what \
                 the code actually does instead of assuming.",
                &json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Path relative to a project root."},
                        "offset": {"type": "integer", "description": "1-indexed first line. Optional."},
                        "limit": {"type": "integer", "description": "Max lines to return. Optional."}
                    },
                    "required": ["path"]
                }),
            ));
            out.push(spec(
                api,
                "search_code",
                "Regex-search the project's file contents (ripgrep). Returns file:line matches. \
                 Use this to find where something is defined or used.",
                &json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Regular expression."},
                        "glob": {"type": "string", "description": "Filter files, e.g. '*.rs'. Optional."}
                    },
                    "required": ["pattern"]
                }),
            ));
            out.push(spec(
                api,
                "list_files",
                "List files in the project matching a glob, so you can orient before reading.",
                &json!({
                    "type": "object",
                    "properties": {
                        "glob": {"type": "string", "description": "e.g. '*.rs' or 'src/**'. Optional."}
                    }
                }),
            ));
        }
        if self.web {
            out.push(spec(
                api,
                "fetch_url",
                "HTTP GET a URL and return its text, tags stripped. Use for specs, docs, RFCs, \
                 or upstream source. Do not use it to look up opinions.",
                &json!({
                    "type": "object",
                    "properties": {"url": {"type": "string"}},
                    "required": ["url"]
                }),
            ));
        }
        out
    }

    /// Execute one tool call. Errors are returned as text for the model to read,
    /// never propagated - a panellist mistyping a path should self-correct, not
    /// abort the round.
    pub async fn call(&self, http: &reqwest::Client, name: &str, args: &Value) -> String {
        let result = match name {
            "read_file" => self.read_file(args),
            "search_code" => self.search_code(args),
            "list_files" => self.list_files(args),
            "fetch_url" => self.fetch_url(http, args).await,
            other => Err(anyhow::anyhow!("unknown tool '{other}'")),
        };
        match result {
            Ok(s) if s.trim().is_empty() => "(no results)".to_owned(),
            Ok(s) => s,
            Err(e) => format!("error: {e:#}"),
        }
    }

    /// Resolve a caller-supplied path inside a root, refusing escapes.
    ///
    /// Rejects absolute paths and any `..` component *before* touching the disk,
    /// then canonicalises and re-checks the prefix - so a symlink pointing out of
    /// the root is caught too.
    fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let rel = Path::new(rel.trim());
        if rel.is_absolute() {
            bail!("path must be relative to a project root");
        }
        if rel
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
        {
            bail!("path may not contain '..'");
        }
        for root in &self.roots {
            let candidate = root.join(rel);
            let Ok(target) = candidate.canonicalize() else {
                continue;
            };
            let Ok(base) = root.canonicalize() else {
                continue;
            };
            if target.starts_with(&base) && target.is_file() {
                return Ok(target);
            }
        }
        bail!("'{}' not found under any project root", rel.display());
    }

    fn read_file(&self, args: &Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("read_file needs a 'path'"))?;
        let real = self.resolve(path)?;
        let text = std::fs::read_to_string(&real)?;

        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX);
        let mut out = String::new();
        let last = offset.saturating_add(limit);
        for (i, line) in text.lines().enumerate() {
            let n = u64::try_from(i).unwrap_or(u64::MAX).saturating_add(1);
            if n < offset {
                continue;
            }
            if n >= last {
                break;
            }
            let _ = writeln!(out, "{n}|{line}");
            if out.len() > self.cap() {
                out.push_str("... (truncated; narrow with offset/limit)\n");
                break;
            }
        }
        Ok(out)
    }

    fn search_code(&self, args: &Value) -> Result<String> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("search_code needs a 'pattern'"))?;
        let mut cmd = std::process::Command::new("rg");
        cmd.arg("--line-number")
            .arg("--no-heading")
            .arg("--color=never")
            .arg("--max-count=40")
            .arg("--max-filesize=1M");
        if let Some(glob) = args.get("glob").and_then(Value::as_str) {
            cmd.arg("--glob").arg(glob);
        }
        // `-e` so a pattern starting with '-' is not read as a flag.
        cmd.arg("-e").arg(pattern);
        for root in &self.roots {
            cmd.arg(root);
        }
        self.run(cmd, "ripgrep (rg) is not installed")
    }

    fn list_files(&self, args: &Value) -> Result<String> {
        let mut cmd = std::process::Command::new("rg");
        cmd.arg("--files").arg("--color=never");
        if let Some(glob) = args.get("glob").and_then(Value::as_str) {
            cmd.arg("--glob").arg(glob);
        }
        for root in &self.roots {
            cmd.arg(root);
        }
        self.run(cmd, "ripgrep (rg) is not installed")
    }

    fn run(&self, mut cmd: std::process::Command, missing: &str) -> Result<String> {
        let out = match cmd.output() {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("{missing}"),
            Err(e) => return Err(e.into()),
        };
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        // rg exits 1 on "no matches", which is not an error worth surfacing.
        if text.is_empty() && !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                bail!("{}", err.trim());
            }
        }
        // Strip the root prefix so paths read as project-relative.
        for root in &self.roots {
            if let Some(prefix) = root.to_str() {
                text = text.replace(&format!("{prefix}/"), "");
            }
        }
        if text.len() > self.cap() {
            text.truncate(self.cap());
            text.push_str("\n... (truncated; refine the pattern or glob)\n");
        }
        Ok(text)
    }

    async fn fetch_url(&self, http: &reqwest::Client, args: &Value) -> Result<String> {
        if !self.web {
            bail!("web access is disabled for this run");
        }
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("fetch_url needs a 'url'"))?;
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            bail!("only http(s) URLs are allowed");
        }
        // Throttle per host before the request, not after: the point is to not
        // hit the server too fast, so the wait has to happen first.
        self.rate.acquire(&host_of(url)).await?;
        let resp = http
            .get(url)
            .header(
                "user-agent",
                "council/0.1 (+https://github.com/NielsMooren/council)",
            )
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("HTTP {status}");
        }
        let mut text = strip_html(&body);
        if text.len() > self.cap() {
            text.truncate(self.cap());
            text.push_str("\n... (truncated)\n");
        }
        Ok(text)
    }
}

/// Host portion of a URL, for rate-limit bookkeeping.
///
/// Deliberately string-based rather than pulling in a URL parser: we only need a
/// stable bucket key, and a malformed URL will fail at the request anyway.
fn host_of(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // Strip credentials and port so the bucket is per-host, not per-URL.
    let host = host.rsplit('@').next().unwrap_or(host);
    host.split(':').next().unwrap_or(host).to_ascii_lowercase()
}

/// Crude tag stripper. Good enough for docs and specs; we are feeding a language
/// model, not rendering a page.
///
/// Character-based, not byte-indexed: HTML is frequently non-ASCII and slicing a
/// `str` at a byte offset panics on a non-char boundary.
fn strip_html(body: &str) -> String {
    #[derive(PartialEq, Eq)]
    enum Mode {
        Text,
        Tag,
        Skip,
    }
    let mut out = String::with_capacity(body.len() / 2);
    let mut mode = Mode::Text;
    // Name of the element whose *content* we are discarding (script/style).
    let mut skipping: Option<&'static str> = None;
    let mut tag = String::new();

    for ch in body.chars() {
        match mode {
            Mode::Text if ch == '<' => {
                mode = Mode::Tag;
                tag.clear();
            }
            Mode::Text => out.push(ch),
            Mode::Tag if ch == '>' => {
                let name = tag.trim().to_ascii_lowercase();
                if let Some(open) = ["script", "style"]
                    .into_iter()
                    .find(|t| name == *t || name.starts_with(&format!("{t} ")))
                {
                    skipping = Some(open);
                    mode = Mode::Skip;
                } else {
                    mode = Mode::Text;
                }
                out.push(' ');
            }
            Mode::Tag => tag.push(ch),
            Mode::Skip if ch == '<' => {
                tag.clear();
                tag.push('<');
            }
            Mode::Skip if ch == '>' => {
                let closing = tag.trim().to_ascii_lowercase();
                if skipping.is_some_and(|s| closing == format!("</{s}")) {
                    skipping = None;
                    mode = Mode::Text;
                }
                tag.clear();
            }
            Mode::Skip => {
                if !tag.is_empty() {
                    tag.push(ch);
                }
            }
        }
    }
    // Collapse the whitespace the tag stripping leaves behind.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One tool definition in the provider's dialect.
fn spec(api: crate::provider::Api, name: &str, description: &str, schema: &Value) -> Value {
    match api {
        crate::provider::Api::OpenaiChat => json!({
            "type": "function",
            "function": {"name": name, "description": description, "parameters": schema.clone()}
        }),
        crate::provider::Api::AnthropicMessages => json!({
            "name": name, "description": description, "input_schema": schema.clone()
        }),
    }
}

/// A tool invocation parsed out of a model response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}
