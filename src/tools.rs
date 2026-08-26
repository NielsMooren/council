//! Research tools panellists may call to answer their own questions.
//!
//! Deliberately narrow and **read-only**. A panellist's job is to form an
//! argument, not to change your machine, so every tool here either reads a file
//! under an explicit root or performs an HTTP GET. There is no shell, no write
//! path, and no way to reach outside the configured roots.
//!
//! Off by default: a run only gets tools when the caller passes roots (or
//! enables web search). Silence is the safe default.

use anyhow::{Context as _, Result, bail};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use url::Url;

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

/// URL cache with single-flight de-duplication.
///
/// Two purposes, and the second is the one that actually matters here:
///
/// 1. A page fetched once is reused for `ttl`, so a later round does not refetch
///    what an earlier round already read.
/// 2. **Concurrent** panellists asking for the same URL collapse into ONE
///    request. A plain check-then-fill cache does not do this: four members
///    starting together all miss, all fetch, and the cache only helps the fifth
///    request that never comes. The per-URL lock below is what makes the
///    common case - a whole panel reading the same spec - cost one fetch.
///
/// Only successful fetches are stored. A transient 503 must not be remembered
/// for ten minutes.
/// One URL's cache slot. `Option` is the entry; the `Mutex` around it is the
/// single-flight gate, so concurrent readers of the same URL queue rather than
/// racing to fetch it.
type Slot = Arc<Mutex<Option<CachedPage>>>;

#[derive(Debug, Clone)]
pub struct UrlCache {
    ttl: Duration,
    /// Max distinct URLs retained. Nothing else bounds this: a model can name
    /// unlimited URLs across unlimited hosts, and the per-host request budget
    /// does not help because each new host gets its own budget.
    max_entries: usize,
    /// Outer lock is held only briefly, to hand out the per-URL slot.
    slots: Arc<Mutex<HashMap<String, Slot>>>,
}

#[derive(Debug, Clone)]
struct CachedPage {
    outcome: Outcome,
    fetched_at: Instant,
}

/// A slot holds either a body or a recent failure.
///
/// Failures are recorded for `FAILURE_GRACE` only - long enough that peers
/// already queued behind a 30s timeout inherit the error instead of each
/// starting their own 30s attempt, short enough that a transient 503 is retried
/// rather than remembered for the whole TTL.
#[derive(Debug, Clone)]
enum Outcome {
    Body(String),
    Failed(String),
}

/// How long a failure suppresses re-attempts. Covers the queue that built up
/// during one slow fetch without meaningfully delaying an honest retry.
const FAILURE_GRACE: Duration = Duration::from_secs(5);

impl Default for UrlCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(600))
    }
}

/// Entry ceiling. Generous for a single deliberation, but finite - TTL alone
/// governs *freshness*, not retention, so without this expired entries stay
/// resident for the life of the process.
const DEFAULT_MAX_ENTRIES: usize = 256;

impl UrlCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            max_entries: DEFAULT_MAX_ENTRIES,
            slots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub const fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Return the cached body for `url`, or run `fetch` and store its result.
    ///
    /// Returns `(body, from_cache)` so the caller can tell the model whether it
    /// is looking at a fresh read - a panellist citing a page should know if it
    /// was fetched seconds ago by a peer.
    async fn get_or_fetch<F, Fut>(&self, url: &str, fetch: F) -> Result<(String, bool)>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        // Grab (or create) this URL's slot, then release the map so a slow fetch
        // of one URL never blocks lookups of a different URL.
        let mut slots = self.slots.lock().await;
        if !slots.contains_key(url) {
            self.evict(&mut slots);
        }
        let slot = Arc::clone(slots.entry(url.to_owned()).or_default());
        drop(slots);

        // The per-URL guard is deliberately held ACROSS the fetch - that is the
        // single-flight gate. A concurrent caller for this URL blocks here and
        // then finds the filled entry instead of issuing a second request.
        // (clippy::significant_drop_tightening wants it dropped earlier; doing
        // so would reintroduce the thundering herd this exists to prevent.)
        let mut entry = slot.lock().await;
        if let Some(page) = entry.as_ref() {
            let age = Instant::now().saturating_duration_since(page.fetched_at);
            match &page.outcome {
                Outcome::Body(body) if age < self.ttl => {
                    let body = body.clone();
                    drop(entry);
                    return Ok((body, true));
                }
                // Inherit a recent failure rather than repeating it. This is the
                // difference between one 30s timeout and N of them: without it,
                // each waiter acquires the guard and starts a fresh attempt.
                // Suppressed when caching is disabled (ttl 0), so `--cache-ttl 0`
                // really means "no caching of anything".
                Outcome::Failed(msg) if !self.ttl.is_zero() && age < FAILURE_GRACE => {
                    let msg = msg.clone();
                    drop(entry);
                    bail!("{msg} (shared with a concurrent request for the same URL)");
                }
                _ => {}
            }
        }

        let outcome = fetch().await;
        let result = match outcome {
            Ok(body) => {
                *entry = Some(CachedPage {
                    outcome: Outcome::Body(body.clone()),
                    fetched_at: Instant::now(),
                });
                Ok((body, false))
            }
            Err(e) => {
                let msg = format!("{e:#}");
                *entry = Some(CachedPage {
                    outcome: Outcome::Failed(msg.clone()),
                    fetched_at: Instant::now(),
                });
                Err(anyhow::anyhow!(msg))
            }
        };
        drop(entry);
        result
    }

    /// Drop expired entries; if that is not enough, drop the oldest.
    ///
    /// Called only when inserting a NEW key, so a cache-hit path never pays for
    /// eviction.
    fn evict(&self, slots: &mut HashMap<String, Slot>) {
        if slots.len() < self.max_entries {
            return;
        }
        let now = Instant::now();
        let ttl = self.ttl;
        // Keep a slot if it is in flight (try_lock fails -> a waiter depends on
        // it) or if its page is still fresh. Note the polarity: `try_lock`
        // FAILING means keep, so this cannot evict a slot out from under a
        // concurrent fetch.
        // Polarity matters: try_lock FAILING means in flight, so keep it. This
        // cannot evict a slot out from under a concurrent fetch.
        slots.retain(|_, slot| {
            slot.try_lock().map_or(true, |guard| {
                guard.as_ref().is_some_and(|page| {
                    let life = match page.outcome {
                        Outcome::Body(_) => ttl,
                        Outcome::Failed(_) => FAILURE_GRACE,
                    };
                    now.saturating_duration_since(page.fetched_at) < life
                })
            })
        });
        // Still full: evict the oldest fetched entry to make room.
        while slots.len() >= self.max_entries {
            let oldest = slots
                .iter()
                .filter_map(|(k, slot)| {
                    let at = slot.try_lock().ok()?.as_ref()?.fetched_at;
                    Some((k.clone(), at))
                })
                .min_by_key(|(_, at)| *at)
                .map(|(k, _)| k);
            match oldest {
                Some(k) => {
                    slots.remove(&k);
                }
                // Everything left is locked (in flight) - leave it alone.
                None => break,
            }
        }
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
    /// Fetched pages, shared by every panellist in the run.
    pub cache: UrlCache,
    /// Every URL actually contacted, including redirect hops.
    ///
    /// The model only ever names the FIRST url; hops are chosen by the remote
    /// server. Recording only `call.args` meant a redirect to
    /// `?dump=<secret>` executed and left no trace - which falsified the stated
    /// justification for permitting query strings at all.
    pub egress: EgressLog,
}

/// Records every URL actually contacted, so the audit trail covers hops the
/// model never named.
#[derive(Debug, Clone, Default)]
pub struct EgressLog {
    urls: Arc<Mutex<Vec<String>>>,
}

impl EgressLog {
    /// Note a URL immediately BEFORE the request goes out, so an attempt is
    /// recorded even if the request then fails or hangs.
    async fn record(&self, url: &Url) {
        self.urls.lock().await.push(url.as_str().to_owned());
    }

    /// Take everything recorded since the last drain.
    ///
    /// Drained per tool call so each `ToolRecord` carries exactly the hops that
    /// its own call produced.
    pub async fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.urls.lock().await)
    }
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
        let raw = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("fetch_url needs a 'url'"))?;
        let url = Url::parse(raw.trim()).map_err(|e| anyhow::anyhow!("bad url: {e}"))?;
        check_no_data_egress(&url)?;

        // Cache on the normalised URL so trivial spelling differences still hit.
        let key = url.as_str().to_owned();
        let (body, cached) = self
            .cache
            .get_or_fetch(&key, || async {
                self.fetch_following_redirects(http, url.clone()).await
            })
            .await?;

        let mut text = body;
        if text.len() > self.cap() {
            text.truncate(self.cap());
            text.push_str("\n... (truncated)\n");
        }
        if cached {
            // Tell the model, so it knows the page was not re-read just now and
            // can say so if freshness matters to its argument.
            text.insert_str(
                0,
                "(from cache: this URL was already fetched during this deliberation)\n\n",
            );
        }
        Ok(text)
    }

    /// Hard ceiling on bytes ingested from one response, before decompression
    /// is accounted for. Ten times the text we are willing to hand the model,
    /// which leaves room for markup while still bounding memory.
    const fn max_response_bytes(&self) -> usize {
        self.cap().saturating_mul(10)
    }

    /// Read a response body with a running byte limit.
    ///
    /// `resp.text()` buffers the WHOLE body first, which is a self-DoS: gzip and
    /// brotli are enabled and reqwest decompresses transparently, so a small
    /// compressed response can expand into a large `String` - and then
    /// `strip_html` allocates again and the cache clones again. Truncating the
    /// final string does not help; the allocation has already happened. The
    /// limit has to gate ingestion, which means streaming.
    async fn read_capped(resp: reqwest::Response, limit: usize) -> Result<String> {
        // Refuse early when the server declares an oversized body.
        if let Some(len) = resp.content_length() {
            if usize::try_from(len).unwrap_or(usize::MAX) > limit {
                bail!("response too large: Content-Length {len} exceeds {limit} bytes");
            }
        }
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("stream error")?;
            if buf.len().saturating_add(chunk.len()) > limit {
                // Keep what we have and stop pulling. A truncated document is
                // more useful to a panellist than an error, and the point of the
                // cap is to bound memory, which it now does.
                let take = limit.saturating_sub(buf.len());
                buf.extend_from_slice(chunk.get(..take).unwrap_or(&chunk));
                break;
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Follow redirects manually, charging the rate limiter for EVERY hop.
    ///
    /// reqwest's automatic redirect following is disabled for this reason: it
    /// would turn one reservation on the original origin into up to ten
    /// unmetered requests to arbitrary other origins, so a chain of redirectors
    /// could hammer a host at unlimited rate while the limiter reported
    /// everything was fine. Accounting evasion, no attacker required.
    async fn fetch_following_redirects(
        &self,
        http: &reqwest::Client,
        mut url: Url,
    ) -> Result<String> {
        // Same ceiling reqwest uses by default, so behaviour is unsurprising.
        const MAX_HOPS: usize = 10;

        for hop in 0..=MAX_HOPS {
            // Record BEFORE the request, not after: an attempt that times out or
            // is refused still tells you what was reached for.
            self.egress.record(&url).await;
            // Charge the origin we are ABOUT to contact, on every hop.
            self.rate.acquire(&origin_of(&url)).await?;
            let resp = http
                .get(url.clone())
                .header(
                    "user-agent",
                    "council/0.1 (+https://github.com/NielsMooren/council)",
                )
                .timeout(Duration::from_secs(30))
                .send()
                .await?;

            let status = resp.status();
            if status.is_redirection() {
                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| anyhow::anyhow!("HTTP {status} with no usable Location"))?;
                // Resolve relative Locations against the current URL.
                let next = url
                    .join(location)
                    .map_err(|e| anyhow::anyhow!("bad redirect target '{location}': {e}"))?;
                // A redirect target is chosen by the remote server, so it gets
                // the same scrutiny as a caller-supplied URL - otherwise a
                // permitted host could bounce us to `?secret=...`.
                check_no_data_egress(&next)
                    .map_err(|e| anyhow::anyhow!("refusing redirect to '{location}': {e}"))?;
                if hop == MAX_HOPS {
                    bail!("too many redirects (>{MAX_HOPS})");
                }
                url = next;
                continue;
            }

            // Check status BEFORE reading the body: no reason to buffer a
            // megabyte of error page.
            if !status.is_success() {
                // Deliberately NOT cached - a transient 503 must not be
                // remembered for the whole TTL.
                bail!("HTTP {status}");
            }
            let text = Self::read_capped(resp, self.max_response_bytes()).await?;
            return Ok(strip_html(&text));
        }
        bail!("too many redirects (>{MAX_HOPS})")
    }
}

/// Reject URLs that are not fetches at all, or that carry credentials.
///
/// Deliberately NOT blocked:
///
/// * **Local and private addresses.** A panellist reading `localhost`, the LAN,
///   or a metadata endpoint is a feature. A local MCP subprocess adds no
///   privilege its parent did not already have.
/// * **Query strings.** They are a real exfiltration channel (`?dump=<secret>`
///   is a write disguised as a read), but they are also how a large share of
///   useful endpoints work, so blocking them broke more than it protected. The
///   accepted position: council can exfiltrate via a GET query exactly like
///   `curl` can. Every URL actually contacted - including redirect hops the
///   model never named - is recorded in the run's provenance as `fetched`, so an
///   attempt is *forensically visible after the fact*. That is telemetry, NOT a
///   control: the same process does the fetching and the logging, so a
///   compromised run could in principle write whatever it likes. Do not expose
///   `fetch_url` to untrusted callers or untrusted URL content.
///
/// What remains blocked has no legitimate server-side use:
fn check_no_data_egress(url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("only http(s) URLs are allowed (got '{}')", url.scheme());
    }
    // Fragments are never transmitted to a server, so a fragment in a fetch is
    // either a mistake or an attempt to hide something in a logged URL.
    if let Some(frag) = url.fragment() {
        bail!("URL fragments are not allowed (got '#{frag}'); request the plain URL");
    }
    // Credentials in a URL replay a secret the model should never hold, and land
    // verbatim in the provenance log.
    if !url.username().is_empty() || url.password().is_some() {
        bail!("credentials in the URL are not allowed");
    }
    Ok(())
}

/// Rate-limit bucket key for a URL: `scheme://host:port`.
///
/// A real origin, parsed by `url::Url` rather than string-splitting. The previous
/// hand-rolled version collapsed every IPv6 address into one bucket, because
/// `split(':')` does not know that `[::1]` is full of colons:
///
/// ```text
/// http://[::1]:8080/x      -> "["        (verified, not theoretical)
/// https://[2001:db8::1]/y  -> "[2001"
/// ```
///
/// Keeping scheme and port is what makes this an origin rather than a hostname:
/// `http://x.com` and `https://x.com:8443` are genuinely different servers.
fn origin_of(url: &Url) -> String {
    let host = url.host_str().unwrap_or("<no-host>").to_ascii_lowercase();
    let scheme = url.scheme();
    url.port_or_known_default().map_or_else(
        || format!("{scheme}://{host}"),
        |port| format!("{scheme}://{host}:{port}"),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The old string-splitting `host_of` collapsed every IPv6 address into one
    /// bucket. These cases exist so that regression cannot come back silently.
    /// Local addresses are ALLOWED by design (a panellist reading a service on
    /// localhost is a feature, and a local MCP subprocess adds no privilege).
    /// What is blocked is a request that could carry data OUT.
    #[test]
    fn local_addresses_are_permitted() {
        for u in [
            "http://127.0.0.1:8080/metrics",
            "http://localhost:3000/health",
            "http://10.0.0.231:8080/co2",
            "http://192.168.1.1/status",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:8080/x",
        ] {
            let url = Url::parse(u).expect("parse");
            assert!(
                check_no_data_egress(&url).is_ok(),
                "local/private addresses must remain fetchable: {u}"
            );
        }
    }

    /// Query strings are ALLOWED: too many real endpoints need them, and the
    /// mitigation is provenance (every fetch is logged with its full URL), not
    /// prevention. This test pins the decision so it is not silently reversed.
    #[test]
    fn query_strings_are_permitted() {
        for u in [
            "https://api.example/v1/search?q=rust&limit=10",
            "http://127.0.0.1:8080/metrics?format=prometheus",
            "https://ok.example/page?",
        ] {
            let url = Url::parse(u).expect("parse");
            assert!(
                check_no_data_egress(&url).is_ok(),
                "query strings must remain usable: {u}"
            );
        }
    }

    #[test]
    fn fragments_and_credentials_are_refused() {
        for (u, want) in [
            ("https://ok.example/page#SECRET", "fragment"),
            ("https://user:pw@ok.example/page", "credentials"),
            ("https://user@ok.example/page", "credentials"),
        ] {
            let url = Url::parse(u).expect("parse");
            let err = check_no_data_egress(&url).expect_err("must refuse");
            assert!(
                format!("{err:#}").to_lowercase().contains(want),
                "expected '{want}' in: {err:#}"
            );
        }
    }

    #[test]
    fn non_http_schemes_are_refused() {
        for u in [
            "file:///etc/passwd",
            "ftp://x.example/f",
            "data:text/html,hi",
        ] {
            let url = Url::parse(u).expect("parse");
            assert!(
                check_no_data_egress(&url).is_err(),
                "only http(s) should be allowed: {u}"
            );
        }
    }

    #[test]
    fn plain_urls_are_allowed() {
        for u in [
            "https://docs.rs/reqwest/latest/reqwest/",
            "http://example.com",
            "https://example.com/a/b/c.html",
        ] {
            let url = Url::parse(u).expect("parse");
            assert!(check_no_data_egress(&url).is_ok(), "should allow: {u}");
        }
    }

    #[test]
    fn origin_of_handles_ipv6_ports_and_case() {
        let cases = [
            ("http://[::1]:8080/x", "http://[::1]:8080"),
            ("https://[2001:db8::1]/y", "https://[2001:db8::1]:443"),
            ("http://EXAMPLE.com/z", "http://example.com:80"),
            ("https://example.com:8443/a", "https://example.com:8443"),
            ("http://user:pw@example.com/q", "http://example.com:80"),
            // Distinct ports and schemes are distinct origins, which is the
            // whole point of using an origin rather than a bare hostname.
            ("http://example.com/a", "http://example.com:80"),
            ("https://example.com/a", "https://example.com:443"),
        ];
        for (input, want) in cases {
            let url = Url::parse(input).expect("test url should parse");
            assert_eq!(origin_of(&url), want, "input: {input}");
        }
    }

    #[test]
    fn ipv6_addresses_are_not_all_one_bucket() {
        let a = Url::parse("http://[::1]:8080/").expect("parse");
        let b = Url::parse("http://[2001:db8::1]:8080/").expect("parse");
        assert_ne!(
            origin_of(&a),
            origin_of(&b),
            "distinct IPv6 hosts must not share a rate-limit bucket"
        );
    }

    #[test]
    fn strip_html_drops_script_and_style_content() {
        // Built by concatenation so clippy does not mistake CSS braces for
        // format arguments.
        let html = [
            "<html><head><script>var x=1;</script>",
            "<style>p",
            "{",
            "color:red",
            "}",
            "</style></head>",
            "<body><p>Caf\u{e9} \u{20ac}</p></body></html>",
        ]
        .concat();
        let out = strip_html(&html);
        assert!(!out.contains("var x"), "script body leaked: {out}");
        assert!(!out.contains("color:red"), "style body leaked: {out}");
        assert!(out.contains("Caf\u{e9}"), "non-ASCII text lost: {out}");
        assert!(out.contains('\u{20ac}'), "non-ASCII text lost: {out}");
    }

    #[test]
    fn strip_html_survives_unterminated_tag() {
        // A truncated document must not panic or hang.
        assert_eq!(strip_html("<p>hi<span"), "hi");
    }

    /// Rate-limit slots must chain off each other, not off the wall clock, or
    /// concurrent callers all compute wait=0 and fire together.
    #[tokio::test(start_paused = true)]
    async fn rate_limit_spaces_concurrent_callers() {
        let rate = RateLimit::new(Duration::from_millis(500), 10);
        let start = tokio::time::Instant::now();
        // Three back-to-back acquires: 0ms, 500ms, 1000ms.
        for expect_ms in [0_u64, 500, 1000] {
            rate.acquire("http://example.com:80")
                .await
                .expect("within budget");
            let elapsed = start.elapsed().as_millis();
            assert_eq!(
                u64::try_from(elapsed).unwrap_or(u64::MAX),
                expect_ms,
                "acquire should be spaced by the interval"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_budget_is_a_hard_stop() {
        let rate = RateLimit::new(Duration::ZERO, 2);
        assert!(rate.acquire("http://a:80").await.is_ok());
        assert!(rate.acquire("http://a:80").await.is_ok());
        let err = rate
            .acquire("http://a:80")
            .await
            .expect_err("third call must be refused");
        assert!(
            format!("{err:#}").contains("rate limit"),
            "error should name the limit: {err:#}"
        );
        // A different origin has its own budget.
        assert!(rate.acquire("http://b:80").await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn cache_serves_within_ttl_and_refetches_after() {
        let cache = UrlCache::new(Duration::from_secs(600));
        let (body, hit) = cache
            .get_or_fetch("u", || async { Ok("one".to_owned()) })
            .await
            .expect("first fetch");
        assert_eq!((body.as_str(), hit), ("one", false));

        // Inside the TTL: the closure must not run again.
        let (body, hit) = cache
            .get_or_fetch("u", || async { panic!("must not refetch inside ttl") })
            .await
            .expect("cached");
        assert_eq!((body.as_str(), hit), ("one", true));

        tokio::time::advance(Duration::from_secs(601)).await;
        let (body, hit) = cache
            .get_or_fetch("u", || async { Ok("two".to_owned()) })
            .await
            .expect("refetch after ttl");
        assert_eq!((body.as_str(), hit), ("two", false));
    }

    #[tokio::test(start_paused = true)]
    async fn cache_shares_a_recent_failure_then_allows_retry() {
        let cache = UrlCache::new(Duration::from_secs(600));
        let first = cache
            .get_or_fetch("u", || async { bail!("boom") })
            .await
            .expect_err("should fail");
        assert!(format!("{first:#}").contains("boom"));

        // Within the grace window a peer inherits the error instead of starting
        // its own attempt - the difference between one 30s timeout and N.
        let shared = cache
            .get_or_fetch("u", || async { panic!("must not refetch during grace") })
            .await
            .expect_err("should inherit");
        assert!(format!("{shared:#}").contains("shared with a concurrent request"));

        tokio::time::advance(FAILURE_GRACE + Duration::from_secs(1)).await;
        let (body, hit) = cache
            .get_or_fetch("u", || async { Ok("recovered".to_owned()) })
            .await
            .expect("retry after grace");
        assert_eq!((body.as_str(), hit), ("recovered", false));
    }

    #[tokio::test(start_paused = true)]
    async fn cache_ttl_zero_disables_everything() {
        let cache = UrlCache::new(Duration::ZERO);
        for _ in 0..3 {
            let (_, hit) = cache
                .get_or_fetch("u", || async { Ok("x".to_owned()) })
                .await
                .expect("fetch");
            assert!(!hit, "ttl 0 must never serve from cache");
        }
    }
}
