//! MCP server surface: exposes the panel as callable tools.

use crate::config::Config;
use crate::deliberate::Deliberation;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use std::fmt::Write as _;
use std::path::PathBuf;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeliberateArgs {
    /// The question or decision to deliberate. Be specific; a vague question
    /// produces a vague consensus.
    pub question: String,
    /// Background the panel needs: constraints, code, prior decisions, verified
    /// facts. Panellists know nothing beyond what you put here.
    #[serde(default)]
    pub context: Option<String>,
    /// Named panel from config. Ignored when `with` is given.
    #[serde(default)]
    pub panel: Option<String>,
    /// Pick the council at runtime from the model registry, e.g.
    /// `["opus", "sol", "haiku"]`. Call `panels` to see the registry. Also
    /// accepts `Alias=handle` to rename, or `provider:model` for an
    /// unregistered model. Overrides `panel`.
    #[serde(default)]
    pub with: Option<Vec<String>>,
    /// Which member writes the synthesis. Defaults to the last.
    #[serde(default)]
    pub chair: Option<String>,
    /// Per-member token ceiling for this run.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Absolute directories the panellists may READ and SEARCH so they can
    /// answer their own questions from the code instead of speculating.
    /// Read-only; no writes, no shell. Omit for text-only reasoning.
    #[serde(default)]
    pub code: Option<Vec<String>>,
    /// Allow panellists to HTTP GET specs, docs or upstream source.
    #[serde(default)]
    pub web: Option<bool>,
    /// Rounds of debate. 1 = independent opinions only (no cross-talk),
    /// 3 = opening/cross-examination/commitment (recommended), 4+ for hard calls.
    #[serde(default)]
    pub rounds: Option<u8>,
    /// Return the full transcript as well as the consensus. Long.
    #[serde(default)]
    pub include_transcript: Option<bool>,
}

#[derive(Clone)]
pub struct Council {
    config_path: Option<PathBuf>,
    // Read by the #[tool_handler] macro expansion, which dead-code analysis
    // does not see through.
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

#[tool_router]
impl Council {
    pub fn new(config_path: Option<PathBuf>) -> Self {
        Self {
            config_path,
            tool_router: Self::tool_router(),
        }
    }

    fn cfg(&self) -> Result<Config, ErrorData> {
        Config::load(self.config_path.as_deref())
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))
    }

    /// Convene a panel of different LLMs to debate a question across multiple
    /// rounds, then have a chair synthesise where they genuinely agree and where
    /// they don't.
    ///
    /// Use this for consequential, contestable decisions - architecture choices,
    /// risky trade-offs, plan review, "is this design sound". The value is
    /// disagreement between models with different training, so it surfaces
    /// unverified assumptions a single model states confidently.
    ///
    /// Do NOT use it for factual lookups, mechanical work, or anything with one
    /// correct answer: you will pay N times for the same reply.
    #[tool(
        description = "Convene a multi-model panel to debate a question across rounds and \
synthesise consensus. For consequential, contestable decisions (architecture, trade-offs, plan \
review) where disagreement between differently-trained models surfaces hidden assumptions. Not \
for factual lookups or tasks with a single correct answer."
    )]
    pub async fn deliberate(
        &self,
        Parameters(args): Parameters<DeliberateArgs>,
    ) -> Result<String, ErrorData> {
        let cfg = self.cfg()?;
        let rounds = args.rounds.unwrap_or(3).clamp(1, 6);
        let specs = args.with.unwrap_or_default();
        // A runtime roster wins over any named panel.
        let panel = if specs.is_empty() {
            cfg.named_panel(args.panel.as_deref())
        } else {
            cfg.panel_from_specs(&specs, args.chair.as_deref())
        }
        .map_err(|e| ErrorData::invalid_params(format!("{e:#}"), None))?;

        // Roots are resolved and checked here so a bad path is an invalid-params
        // error rather than a confusing mid-run tool failure.
        let mut roots = Vec::new();
        for dir in args.code.unwrap_or_default() {
            let real = std::path::Path::new(&dir)
                .canonicalize()
                .map_err(|e| ErrorData::invalid_params(format!("code path '{dir}': {e}"), None))?;
            if !real.is_dir() {
                return Err(ErrorData::invalid_params(
                    format!("code path '{dir}' is not a directory"),
                    None,
                ));
            }
            roots.push(real);
        }
        let tools = crate::tools::Toolbox {
            roots,
            web: args.web.unwrap_or(false),
            max_bytes: crate::tools::Toolbox::DEFAULT_MAX_BYTES,
            // Defaults only: an MCP caller is a program, and letting it dial
            // politeness limits down is exactly how a service becomes abusive.
            rate: crate::tools::RateLimit::default(),
            // 10-minute TTL: long enough that a multi-round deliberation reads a
            // page once, short enough that a live doc is not stale by the end.
            cache: crate::tools::UrlCache::default(),
        };

        let d = Deliberation {
            question: args.question,
            context: args.context,
            rounds,
            panel,
            max_tokens: args.max_tokens,
            tools,
            resume: true,
        };
        let out = d
            .run(&cfg)
            .await
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))?;

        let mut s = out.consensus;
        // A program cannot infer roster health from prose, so state it plainly:
        // a 2-of-4 panel is a materially weaker signal than a 4-of-4 one.
        let absent: Vec<String> = out
            .members
            .iter()
            .filter(|m| !m.ok)
            .map(|m| format!("{} ({}/{rounds} rounds)", m.name, m.rounds_present))
            .collect();
        if !absent.is_empty() {
            let _ = write!(
                s,
                "\n\n---\n### Incomplete panel\n{} did not complete every round. \
                 Weigh the consensus accordingly.",
                absent.join(", ")
            );
        }
        if !out.failures.is_empty() {
            // Surfaced, not swallowed: a 2-of-4 panel is a weaker signal and the
            // caller must know that before trusting the consensus.
            s.push_str("\n\n---\n### Panellists that failed\n");
            for f in &out.failures {
                let _ = writeln!(s, "- {f}");
            }
        }
        if args.include_transcript.unwrap_or(false) {
            let _ = write!(s, "\n\n---\n## Full transcript\n{}", out.transcript);
        }
        let _ = writeln!(s, "\n\n---\nArtifacts: {}", out.cache_dir.display());
        Ok(s)
    }

    /// The registry of models a caller can put on a council.
    ///
    /// Call this before `deliberate` when you do not already know the handles:
    /// it is the authoritative list, and passing an unregistered handle is an
    /// error rather than a silent fallback.
    #[tool(
        description = "List the model handles available for a council, with their provider, \
whether the provider's API key is present, and whether each model is currently usable. Call this \
before `deliberate` to discover what you can pass to its `with` argument."
    )]
    pub async fn models(&self) -> Result<String, ErrorData> {
        let cfg = self.cfg()?;
        if cfg.models.is_empty() {
            return Ok(
                "The model registry is empty. Add `[[models]]` entries to the council \
                       config, or pass `provider:model` directly to `deliberate`."
                    .to_owned(),
            );
        }

        let mut s = String::from(
            "## Available models\n\n\
             Pass these handles to `deliberate`'s `with` argument, e.g. \
             `with: [\"opus\", \"sol\"]`.\n\n\
             | handle | provider | model | usable |\n|---|---|---|---|\n",
        );
        let mut unusable = Vec::new();
        for m in &cfg.models {
            // "Usable" means the provider exists AND its key is in the env - the
            // two things that make a call fail before it is even sent.
            let (usable, why) = match cfg.provider(&m.provider) {
                Ok(p) if std::env::var(&p.api_key_env).is_ok() => ("yes", None),
                Ok(p) => ("no", Some(format!("{} not set", p.api_key_env))),
                Err(_) => ("no", Some(format!("unknown provider '{}'", m.provider))),
            };
            let _ = writeln!(
                s,
                "| `{}` | {} | {} | {}{} |",
                m.name,
                m.provider,
                m.model,
                usable,
                why.as_ref().map_or_else(String::new, |w| format!(" ({w})"))
            );
            if let Some(w) = why {
                unusable.push(format!("{} — {w}", m.name));
            }
        }

        if !unusable.is_empty() {
            s.push_str("\n**Not usable right now** (do not select these):\n");
            for u in &unusable {
                let _ = writeln!(s, "- {u}");
            }
        }

        s.push_str(
            "\n### Choosing\n\
             - Diversity beats size: two models from *different* providers disagree more \
             usefully than three from one. The disagreement is the point.\n\
             - Cost is `members x rounds + 1` (the +1 is the chair). Start with 2 members \
             and 1 round; widen only if they split.\n\
             - `rounds`: 1 = independent opinions, 2 = + cross-examination, \
             3 = + commitment (default), 4-6 = genuinely contested designs.\n\
             - You may rename a member for the transcript with `Alias=handle`, or use \
             `provider:model` for a model that is not registered.\n",
        );
        Ok(s)
    }

    /// List configured panels, so a caller can pick a ready-made roster.
    #[tool(
        description = "List the pre-configured panels (named rosters of models) and the \
providers they use. Use a panel name as `deliberate`'s `panel` argument, or ignore panels \
entirely and pass `with` to choose models yourself."
    )]
    pub async fn panels(&self) -> Result<String, ErrorData> {
        let cfg = self.cfg()?;
        let mut s = String::from("## Panels\n");
        if cfg.panels.is_empty() {
            s.push_str("\n(none configured — use `deliberate`'s `with` argument instead)\n");
        }
        for p in &cfg.panels {
            let chair = p.chair.as_deref().unwrap_or("(last member)");
            let default = if cfg.default_panel.as_deref() == Some(p.name.as_str()) {
                "  [default]"
            } else {
                ""
            };
            let _ = writeln!(s, "\n### {} — chair: {chair}{default}", p.name);
            match cfg.resolve(p) {
                Ok(rp) => {
                    for m in &rp.members {
                        let _ = writeln!(s, "- {} — {}:{}", m.name, m.provider, m.model);
                    }
                }
                Err(e) => {
                    let _ = writeln!(s, "- UNRESOLVABLE: {e}");
                }
            }
        }
        s.push_str("\n## Providers\n");
        for p in &cfg.providers {
            let key = if std::env::var(&p.api_key_env).is_ok() {
                "key set"
            } else {
                "KEY MISSING"
            };
            let _ = writeln!(s, "- {} — {:?} @ {} ({})", p.name, p.api, p.base_url, key);
        }
        s.push_str("\nCall `models` for the individual handles you can mix yourself.\n");
        Ok(s)
    }
}

// The macro generates a non-awaiting async trait fn; not ours to restructure.
#[expect(
    clippy::unused_async_trait_impl,
    reason = "generated by #[tool_handler]"
)]
#[tool_handler]
impl ServerHandler for Council {
    fn get_info(&self) -> ServerInfo {
        // InitializeResult is #[non_exhaustive]; use the builder, not a literal.
        // from_build_env() would report the rmcp crate, not ours.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Multi-model deliberation: several LLMs debate a question over rounds, then a \
                 chair synthesises where they genuinely agree and where they do not.\n\n\
                 Workflow: call `models` to see which model handles are available and usable, \
                 then call `deliberate` with `with: [handles]` to choose the council, or omit \
                 `with` to use the default panel. `panels` lists pre-configured rosters.\n\n\
                 Use this for consequential, contestable decisions - architecture choices, \
                 risky trade-offs, design review - where disagreement between \
                 differently-trained models surfaces assumptions a single model states \
                 confidently. Do not use it for factual lookups or tasks with one correct \
                 answer: you pay N times for the same reply.",
            )
    }
}
