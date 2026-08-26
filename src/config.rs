//! Config loading. Providers, models and panels live in TOML; secrets live in env.
//!
//! Three layers, each referencing the one above by name:
//!   [[providers]]  a wire endpoint + auth        ("anthropic", "work-gateway")
//!   [[models]]     a named model on a provider   ("opus", "sol", "haiku")
//!   [[panels]]     a reusable roster of models   ("default", "security")
//!
//! The model registry is what makes runtime selection ergonomic: once a model is
//! registered you refer to it by a short name everywhere - `--with opus,sol` -
//! instead of repeating provider/model pairs.

use crate::provider::{Member, Provider};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A named model in the registry. `name` is the handle you use at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Short handle, e.g. "opus". This is what `--with` takes.
    pub name: String,
    /// Which `[[providers]]` entry carries it.
    pub provider: String,
    /// The provider's model id, e.g. "claude-opus-4-5".
    pub model: String,
    /// Per-model token ceiling, overriding the global `max_tokens`.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Default persona when this model sits on a panel without one.
    #[serde(default)]
    pub persona: Option<String>,
}

impl ModelEntry {
    /// Materialise as a panellist. `alias` renames it for this panel, and
    /// `persona` overrides the registry default.
    pub fn to_member(&self, alias: Option<&str>, persona: Option<String>) -> Member {
        Member {
            name: alias.unwrap_or(&self.name).to_owned(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            persona: persona.or_else(|| self.persona.clone()),
        }
    }
}

/// A panel member as written in a `[[panels]]` block.
///
/// Either a registry reference (`model = "opus"`) or a fully inline definition
/// (`provider` + `model`). The reference form is preferred - one place to change
/// a model id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelMember {
    /// Registry handle, or the raw model id when `provider` is also given.
    pub model: String,
    /// Set only for an inline member that bypasses the registry.
    #[serde(default)]
    pub provider: Option<String>,
    /// Rename this member for the transcript, e.g. `name = "Skeptic"`.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub persona: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Panel {
    pub name: String,
    #[serde(default)]
    pub members: Vec<PanelMember>,
    /// Who writes the final synthesis. Defaults to the last member.
    #[serde(default)]
    pub chair: Option<String>,
}

/// A panel with every member fully resolved against the registry. This is what
/// the engine actually runs, so resolution failures happen before any API call.
#[derive(Debug, Clone)]
pub struct ResolvedPanel {
    pub name: String,
    pub members: Vec<Member>,
    pub chair: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub providers: Vec<Provider>,
    /// The model registry: name models once, reference them everywhere.
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    #[serde(default)]
    pub panels: Vec<Panel>,
    /// Which panel `deliberate` uses when none is named. Defaults to "default".
    #[serde(default)]
    pub default_panel: Option<String>,
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
        if self.providers.is_empty() {
            bail!("config defines no providers");
        }
        for m in &self.models {
            if !self.providers.iter().any(|p| p.name == m.provider) {
                bail!(
                    "model '{}' references unknown provider '{}' (have: {})",
                    m.name,
                    m.provider,
                    self.provider_names().join(", ")
                );
            }
        }
        let mut names: Vec<&str> = self.models.iter().map(|m| m.name.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        if names.len() != before {
            bail!("duplicate model names in the registry");
        }
        // Resolving every panel proves the references are good.
        for panel in &self.panels {
            self.resolve(panel)?;
        }
        Ok(())
    }

    fn provider_names(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name.as_str()).collect()
    }

    pub fn model_names(&self) -> Vec<&str> {
        self.models.iter().map(|m| m.name.as_str()).collect()
    }

    pub fn model(&self, name: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.name == name)
    }

    /// Resolve a configured panel into runnable members.
    pub fn resolve(&self, panel: &Panel) -> Result<ResolvedPanel> {
        let mut members = Vec::with_capacity(panel.members.len());
        for pm in &panel.members {
            let mut member = match (&pm.provider, self.model(&pm.model)) {
                // Inline definition: provider given explicitly.
                (Some(provider), _) => Member {
                    name: pm.name.clone().unwrap_or_else(|| pm.model.clone()),
                    provider: provider.clone(),
                    model: pm.model.clone(),
                    max_tokens: pm.max_tokens,
                    persona: pm.persona.clone(),
                },
                // Registry reference.
                (None, Some(entry)) => entry.to_member(pm.name.as_deref(), pm.persona.clone()),
                (None, None) => bail!(
                    "panel '{}' references unknown model '{}' (registry: {})",
                    panel.name,
                    pm.model,
                    self.model_names().join(", ")
                ),
            };
            if let Some(mt) = pm.max_tokens {
                member.max_tokens = Some(mt);
            }
            members.push(member);
        }
        let resolved = ResolvedPanel {
            name: panel.name.clone(),
            members,
            chair: panel.chair.clone(),
        };
        self.check_panel(&resolved)?;
        Ok(resolved)
    }

    /// Shared sanity checks, so config panels and runtime panels fail alike.
    pub fn check_panel(&self, panel: &ResolvedPanel) -> Result<()> {
        if panel.members.len() < 2 {
            bail!(
                "panel '{}' needs at least 2 members to deliberate",
                panel.name
            );
        }
        for m in &panel.members {
            if !self.providers.iter().any(|p| p.name == m.provider) {
                bail!(
                    "panel '{}' member '{}' references unknown provider '{}' (have: {})",
                    panel.name,
                    m.name,
                    m.provider,
                    self.provider_names().join(", ")
                );
            }
        }
        if let Some(chair) = &panel.chair {
            if !panel.members.iter().any(|m| &m.name == chair) {
                let have: Vec<&str> = panel.members.iter().map(|m| m.name.as_str()).collect();
                bail!(
                    "panel '{}' chair '{chair}' is not a member (have: {})",
                    panel.name,
                    have.join(", ")
                );
            }
        }
        let mut names: Vec<&str> = panel.members.iter().map(|m| m.name.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        if names.len() != before {
            bail!(
                "panel '{}' has duplicate member names; use `name =` to disambiguate",
                panel.name
            );
        }
        Ok(())
    }

    /// Build a panel at runtime from registry handles, e.g. `["opus", "sol"]`.
    ///
    /// Each entry is a registry name, optionally `Alias=name` to rename it, or
    /// `provider:model` to sidestep the registry entirely.
    pub fn panel_from_specs(&self, specs: &[String], chair: Option<&str>) -> Result<ResolvedPanel> {
        let mut members = Vec::with_capacity(specs.len());
        for spec in specs {
            let spec = spec.trim();
            if spec.is_empty() {
                continue;
            }
            let (alias, target) = match spec.split_once('=') {
                Some((a, t)) => (Some(a.trim()), t.trim()),
                None => (None, spec),
            };
            // `provider:model` escape hatch for a one-off model not in the registry.
            if let Some((provider, model)) = target.split_once(':') {
                members.push(Member {
                    name: alias.unwrap_or(model).to_owned(),
                    provider: provider.trim().to_owned(),
                    model: model.trim().to_owned(),
                    max_tokens: None,
                    persona: None,
                });
                continue;
            }
            let entry = self.model(target).with_context(|| {
                format!(
                    "unknown model '{target}' (registry: {}); use provider:model for an unregistered one",
                    self.model_names().join(", ")
                )
            })?;
            members.push(entry.to_member(alias, None));
        }
        let resolved = ResolvedPanel {
            name: "runtime".to_owned(),
            members,
            chair: chair.map(str::to_owned),
        };
        self.check_panel(&resolved)?;
        Ok(resolved)
    }

    /// The panel to use when the caller names one, or the configured default.
    pub fn named_panel(&self, name: Option<&str>) -> Result<ResolvedPanel> {
        let wanted = name.or(self.default_panel.as_deref()).unwrap_or("default");
        let panel = self
            .panels
            .iter()
            .find(|p| p.name == wanted)
            .with_context(|| {
                let have: Vec<&str> = self.panels.iter().map(|p| p.name.as_str()).collect();
                format!("unknown panel '{wanted}' (have: {})", have.join(", "))
            })?;
        self.resolve(panel)
    }

    pub fn provider(&self, name: &str) -> Result<&Provider> {
        self.providers
            .iter()
            .find(|p| p.name == name)
            .with_context(|| {
                format!(
                    "unknown provider '{name}' (have: {})",
                    self.provider_names().join(", ")
                )
            })
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir
            .clone()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".council"))
    }
}

/// Starter config. Shows the three layers and two different auth styles, since
/// getting a corporate gateway working is usually the first hurdle.
pub const STARTER: &str = r#"# council config. Secrets stay in env vars - never inline them here.
max_tokens = 12000
default_panel = "default"

# ---------------------------------------------------------------- providers
# A provider is a wire endpoint + auth, not a vendor. Any OpenAI- or
# Anthropic-compatible endpoint works: OpenAI, Anthropic, Azure, OpenRouter,
# Groq, Together, vLLM, Ollama, LiteLLM, corporate gateways.

[[providers]]
name = "openai"
api = "openai_chat"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
auth = "bearer"

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

# A gateway with a non-standard auth header:
# [[providers]]
# name = "work"
# api = "anthropic_messages"
# base_url = "https://gateway.example.com/anthropic"
# api_key_env = "WORK_GATEWAY_KEY"
# auth = { header = "api-key" }
# headers = { "anthropic-version" = "2023-06-01" }

# ------------------------------------------------------------------- models
# The registry. Name a model once here, then refer to it by that short name:
#   council ask "..." --with opus,sol,haiku
# `council models` lists them.

[[models]]
name = "gpt"
provider = "openai"
model = "gpt-5.5"

[[models]]
name = "sonnet"
provider = "anthropic"
model = "claude-sonnet-4-5"

[[models]]
name = "opus"
provider = "anthropic"
model = "claude-opus-4-5"
# An optional default persona, used whenever this model has no panel-specific one.
persona = "You weigh trade-offs and refuse to manufacture agreement."

# ------------------------------------------------------------------- panels
# Reusable rosters. Members reference the registry by name; `name =` renames
# one for the transcript and `persona =` gives it an angle to argue.
#
# Personas matter: peers should argue with the argument, not defer to whichever
# model sounds most authoritative.

[[panels]]
name = "default"
chair = "Chair"

  [[panels.members]]
  model = "gpt"
  name = "Pragmatist"
  persona = "You optimise for what ships this week and holds in production."

  [[panels.members]]
  model = "sonnet"
  name = "Skeptic"
  persona = "You hunt unverified assumptions. Demand evidence for load-bearing claims."

  [[panels.members]]
  model = "opus"
  name = "Chair"

# A cheap panel for quick calls:
# [[panels]]
# name = "quick"
#   [[panels.members]]
#   model = "sonnet"
#   [[panels.members]]
#   model = "gpt"
"#;
