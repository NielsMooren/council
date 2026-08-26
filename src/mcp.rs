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

        let d = Deliberation {
            question: args.question,
            context: args.context,
            rounds,
            panel,
            max_tokens: args.max_tokens,
            resume: true,
        };
        let out = d
            .run(&cfg)
            .await
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))?;

        let mut s = out.consensus;
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

    /// List configured panels and providers, so a caller can pick a panel
    /// without reading the config file.
    #[tool(description = "List the panels and providers available in the council config.")]
    pub async fn panels(&self) -> Result<String, ErrorData> {
        let cfg = self.cfg()?;
        let mut s = String::from("## Models (handles for `with`)\n");
        for m in &cfg.models {
            let _ = writeln!(s, "- `{}` — {}:{}", m.name, m.provider, m.model);
        }
        if cfg.models.is_empty() {
            s.push_str("- (registry empty; add [[models]] to the config)\n");
        }
        s.push_str("\n## Panels\n");
        for p in &cfg.panels {
            let chair = p.chair.as_deref().unwrap_or("(last member)");
            let _ = writeln!(s, "\n### {} — chair: {chair}", p.name);
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
                "Multi-model deliberation. `deliberate` convenes a panel of different LLMs to \
                 debate a question over several rounds and returns a consensus document that \
                 separates genuine agreement from unresolved disagreement. `panels` lists \
                 available panels.",
            )
    }
}
