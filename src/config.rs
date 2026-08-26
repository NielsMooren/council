//! Config loading. Providers and panels live in TOML; secrets live in env.

use crate::provider::{Member, Provider};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Panel {
    pub name: String,
    pub members: Vec<Member>,
    /// Who writes the final synthesis. Defaults to the last member.
    #[serde(default)]
    pub chair: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub panels: Vec<Panel>,
    /// Where transcripts and the resume cache go. Default ~/.council.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

const fn default_max_tokens() -> u32 {
    12_000
}

impl Config {
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".council/config.toml")
    }

    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path.map_or_else(Self::default_path, Path::to_path_buf);
        if !path.exists() {
            bail!(
                "no config at {}\n\nRun `council init` to write a starter config.",
                path.display()
            );
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Fail at load time, not three rounds into a paid run.
    pub fn validate(&self) -> Result<()> {
        let known: HashMap<&str, &Provider> = self
            .providers
            .iter()
            .map(|p| (p.name.as_str(), p))
            .collect();
        if self.providers.is_empty() {
            bail!("config defines no providers");
        }
        for panel in &self.panels {
            if panel.members.len() < 2 {
                bail!(
                    "panel '{}' needs at least 2 members to deliberate",
                    panel.name
                );
            }
            for m in &panel.members {
                if !known.contains_key(m.provider.as_str()) {
                    bail!(
                        "panel '{}' member '{}' references unknown provider '{}'",
                        panel.name,
                        m.name,
                        m.provider
                    );
                }
            }
            if let Some(chair) = &panel.chair {
                if !panel.members.iter().any(|m| &m.name == chair) {
                    bail!(
                        "panel '{}' chair '{chair}' is not one of its members",
                        panel.name
                    );
                }
            }
            let mut names: Vec<&str> = panel.members.iter().map(|m| m.name.as_str()).collect();
            names.sort_unstable();
            let before = names.len();
            names.dedup();
            if names.len() != before {
                bail!("panel '{}' has duplicate member names", panel.name);
            }
        }
        Ok(())
    }

    pub fn provider(&self, name: &str) -> Result<&Provider> {
        self.providers
            .iter()
            .find(|p| p.name == name)
            .with_context(|| format!("unknown provider '{name}'"))
    }

    pub fn panel(&self, name: &str) -> Result<&Panel> {
        self.panels
            .iter()
            .find(|p| p.name == name)
            .with_context(|| {
                let have: Vec<&str> = self.panels.iter().map(|p| p.name.as_str()).collect();
                format!("unknown panel '{name}' (have: {})", have.join(", "))
            })
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir
            .clone()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".council"))
    }
}

/// Starter config. Deliberately shows three different wire/auth combinations,
/// because getting a corporate gateway working is the usual first hurdle.
pub const STARTER: &str = r#"# council config. Secrets stay in env vars - never inline them here.
max_tokens = 12000

# --- OpenAI (or any OpenAI-compatible gateway: OpenRouter, Groq, vLLM, Ollama) ---
[[providers]]
name = "openai"
api = "openai_chat"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
auth = "bearer"

# --- Anthropic direct ---
[[providers]]
name = "anthropic"
api = "anthropic_messages"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
auth = "x_api_key"
headers = { "anthropic-version" = "2023-06-01" }
# Leave true unless you specifically want extended thinking: some gateways
# spend the entire token budget on thinking and return zero text.
disable_thinking = true

# --- A corporate gateway using a non-standard auth header ---
# [[providers]]
# name = "work-claude"
# api = "anthropic_messages"
# base_url = "https://gateway.example.com/anthropic"
# api_key_env = "WORK_GATEWAY_KEY"
# auth = { header = "api-key" }
# headers = { "anthropic-version" = "2023-06-01" }

# Panels mix providers on purpose - diversity of training is the whole point.
# Give members PERSONAS, not model names: peers should argue with the argument,
# not defer to whichever model sounds most authoritative.
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

  [[panels.members]]
  name = "Chair"
  provider = "anthropic"
  model = "claude-opus-4-5"
  persona = "You weigh trade-offs and refuse to manufacture agreement."
"#;
