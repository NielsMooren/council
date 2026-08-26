//! MCP server surface: exposes the panel as callable tools.

use crate::config::Config;
use crate::deliberate::Deliberation;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
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
    /// Panel name from config. Defaults to "default".
    #[serde(default)]
    pub panel: Option<String>,
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
        let d = Deliberation {
            question: args.question,
            context: args.context,
            rounds,
            panel: args.panel.unwrap_or_else(|| "default".into()),
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
                s.push_str(&format!("- {f}\n"));
            }
        }
        if args.include_transcript.unwrap_or(false) {
            s.push_str(&format!("\n\n---\n## Full transcript\n{}", out.transcript));
        }
        s.push_str(&format!(
            "\n\n---\nArtifacts: {}\n",
            out.cache_dir.display()
        ));
        Ok(s)
    }

    /// List configured panels and providers, so a caller can pick a panel
    /// without reading the config file.
    #[tool(description = "List the panels and providers available in the council config.")]
    pub async fn panels(&self) -> Result<String, ErrorData> {
        let cfg = self.cfg()?;
        let mut s = String::from("## Panels\n");
        for p in &cfg.panels {
            let chair = p.chair.as_deref().unwrap_or("(last member)");
            s.push_str(&format!("\n### {} — chair: {chair}\n", p.name));
            for m in &p.members {
                s.push_str(&format!("- {} — {}:{}\n", m.name, m.provider, m.model));
            }
        }
        s.push_str("\n## Providers\n");
        for p in &cfg.providers {
            let key = if std::env::var(&p.api_key_env).is_ok() {
                "key set"
            } else {
                "KEY MISSING"
            };
            s.push_str(&format!(
                "- {} — {:?} @ {} ({})\n",
                p.name, p.api, p.base_url, key
            ));
        }
        Ok(s)
    }
}

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
