mod config;
mod deliberate;
mod mcp;
mod provider;
mod tools;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use config::Config;
use deliberate::Deliberation;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "council",
    about = "Multi-model deliberation: several LLMs debate, a chair synthesises consensus.",
    version
)]
struct Cli {
    /// Config path (default ~/.council/config.toml)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run as an MCP server over stdio (for use from an AI session).
    Serve,
    /// Write a starter config.
    Init {
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
    },
    /// Deliberate from the terminal.
    Ask {
        question: String,
        /// Extra context; `-` reads stdin.
        #[arg(short = 'x', long)]
        context: Option<String>,
        /// Pick the council at runtime from the model registry:
        /// `--with opus,sol,haiku`. Also accepts `Alias=model` to rename and
        /// `provider:model` for something not in the registry. Overrides --panel.
        #[arg(short = 'w', long, value_delimiter = ',')]
        with: Vec<String>,
        /// Which member writes the synthesis. Defaults to the last.
        #[arg(long)]
        chair: Option<String>,
        /// Per-member token ceiling for this run.
        #[arg(long)]
        max_tokens: Option<u32>,
        /// Let panellists READ and SEARCH these directories to answer their own
        /// questions instead of speculating. Repeatable. Read-only.
        #[arg(long = "code", value_name = "DIR")]
        code: Vec<PathBuf>,
        /// Let panellists fetch URLs (specs, docs, upstream source).
        #[arg(long)]
        web: bool,
        /// Minimum milliseconds between requests to the SAME host.
        #[arg(long, default_value_t = 1000)]
        host_delay_ms: u64,
        /// Max requests to a single host per deliberation.
        #[arg(long, default_value_t = 20)]
        host_budget: u32,
        /// Seconds a fetched URL stays cached, so concurrent panellists reading
        /// the same page cost one request. 0 disables caching.
        #[arg(long, default_value_t = 600)]
        cache_ttl: u64,
        /// Named panel from config. Ignored when --with is given.
        #[arg(short, long)]
        panel: Option<String>,
        #[arg(short, long, default_value_t = 3)]
        rounds: u8,
        /// Print the full transcript too.
        #[arg(long)]
        transcript: bool,
        /// Ignore cached responses and re-run from scratch.
        #[arg(long)]
        fresh: bool,
    },
    /// Show panels, providers, and whether each API key is present.
    Panels,
    /// List the model registry: the handles `--with` accepts.
    Models,
    /// Verify config parses and every referenced provider has a key.
    Check,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // stdio MCP: logs MUST go to stderr or they corrupt the protocol stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "council=info".into()),
        )
        .init();

    match cli.cmd {
        Cmd::Serve => serve(cli.config).await?,
        Cmd::Init { force } => init(cli.config, force)?,
        Cmd::Ask {
            question,
            context,
            with,
            chair,
            max_tokens,
            code,
            web,
            host_delay_ms,
            host_budget,
            cache_ttl,
            panel,
            rounds,
            transcript,
            fresh,
        } => {
            let opts = AskOpts {
                question,
                context,
                with,
                chair,
                max_tokens,
                code,
                web,
                host_delay_ms,
                host_budget,
                cache_ttl,
                panel,
                rounds,
                transcript,
                fresh,
            };
            ask(cli.config.as_deref(), opts).await?;
        }
        Cmd::Panels => panels(cli.config.as_deref())?,
        Cmd::Models => models(cli.config.as_deref())?,
        Cmd::Check => check(cli.config.as_deref())?,
    }
    Ok(())
}

async fn serve(config: Option<PathBuf>) -> Result<()> {
    use rmcp::ServiceExt;
    let service = mcp::Council::new(config)
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}

fn init(config: Option<PathBuf>, force: bool) -> Result<()> {
    let path = config.unwrap_or_else(Config::default_path);
    if path.exists() && !force {
        anyhow::bail!("{} already exists (use --force)", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, config::STARTER)?;
    println!("wrote {}", path.display());
    println!("Edit it, export the API keys it names, then run `council check`.");
    Ok(())
}

/// CLI options for `ask`, grouped so the function stays under the arg limit.
struct AskOpts {
    question: String,
    context: Option<String>,
    with: Vec<String>,
    chair: Option<String>,
    max_tokens: Option<u32>,
    code: Vec<PathBuf>,
    web: bool,
    host_delay_ms: u64,
    host_budget: u32,
    cache_ttl: u64,
    panel: Option<String>,
    rounds: u8,
    transcript: bool,
    fresh: bool,
}

async fn ask(config: Option<&std::path::Path>, o: AskOpts) -> Result<()> {
    let cfg = Config::load(config)?;
    // --with wins: an explicit runtime roster overrides any named panel.
    let panel = if o.with.is_empty() {
        cfg.named_panel(o.panel.as_deref())?
    } else {
        cfg.panel_from_specs(&o.with, o.chair.as_deref())?
    };

    let context = match o.context.as_deref() {
        Some("-") => {
            use std::io::Read as _;
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            Some(s)
        }
        other => other.map(str::to_owned),
    };

    let tools = council_tools(&o.code, o.web, o.host_delay_ms, o.host_budget, o.cache_ttl)?;
    if !tools.is_empty() {
        eprintln!(
            "council: tools enabled — roots: [{}], web: {}{}",
            tools
                .roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            tools.web,
            if tools.web {
                format!(
                    " (max {} req/host, {}ms apart, {}s cache)",
                    tools.rate.max_per_host,
                    tools.rate.min_interval.as_millis(),
                    tools.cache.ttl().as_secs()
                )
            } else {
                String::new()
            }
        );
    }

    let roster: Vec<String> = panel
        .members
        .iter()
        .map(|m| format!("{} ({}:{})", m.name, m.provider, m.model))
        .collect();
    eprintln!(
        "council: {} rounds x {} members [{}]",
        o.rounds.clamp(1, 6),
        panel.members.len(),
        roster.join(", ")
    );

    let out = Deliberation {
        question: o.question,
        context,
        rounds: o.rounds.clamp(1, 6),
        panel,
        max_tokens: o.max_tokens,
        tools,
        resume: !o.fresh,
    }
    .run(&cfg)
    .await?;

    if o.transcript {
        println!("{}\n", out.transcript);
    }
    println!("{}", out.consensus);
    if !out.failures.is_empty() {
        eprintln!("\nfailed panellists:");
        for f in &out.failures {
            eprintln!("  - {f}");
        }
    }
    eprintln!("\nartifacts: {}", out.cache_dir.display());
    Ok(())
}

/// Build a toolbox, failing fast on a root that does not exist.
fn council_tools(
    code: &[PathBuf],
    web: bool,
    host_delay_ms: u64,
    host_budget: u32,
    cache_ttl: u64,
) -> Result<tools::Toolbox> {
    let mut roots = Vec::with_capacity(code.len());
    for dir in code {
        let real = dir
            .canonicalize()
            .with_context(|| format!("--code {}: not found", dir.display()))?;
        if !real.is_dir() {
            anyhow::bail!("--code {}: not a directory", dir.display());
        }
        roots.push(real);
    }
    Ok(tools::Toolbox {
        roots,
        web,
        max_bytes: tools::Toolbox::DEFAULT_MAX_BYTES,
        rate: tools::RateLimit::new(std::time::Duration::from_millis(host_delay_ms), host_budget),
        cache: tools::UrlCache::new(std::time::Duration::from_secs(cache_ttl)),
    })
}

fn models(config: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(config)?;
    if cfg.models.is_empty() {
        println!("registry is empty; add [[models]] entries to your config");
        return Ok(());
    }
    println!("{:<14} {:<12} MODEL", "HANDLE", "PROVIDER");
    for m in &cfg.models {
        println!("{:<14} {:<12} {}", m.name, m.provider, m.model);
    }
    println!(
        "\nuse with: council ask \"...\" --with {}",
        cfg.model_names().join(",")
    );
    Ok(())
}

fn panels(config: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(config)?;
    for p in &cfg.panels {
        println!(
            "panel {} (chair: {})",
            p.name,
            p.chair.as_deref().unwrap_or("last")
        );
        match cfg.resolve(p) {
            Ok(rp) => {
                for m in &rp.members {
                    println!("  {:<14} {}:{}", m.name, m.provider, m.model);
                }
            }
            Err(e) => println!("  UNRESOLVABLE: {e}"),
        }
    }
    println!();
    for p in &cfg.providers {
        let key = if std::env::var(&p.api_key_env).is_ok() {
            "ok"
        } else {
            "MISSING"
        };
        println!(
            "provider {:<14} {:?} {} [{}={}]",
            p.name, p.api, p.base_url, p.api_key_env, key
        );
    }
    Ok(())
}

fn check(config: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(config)?;
    println!(
        "config OK: {} providers, {} panels",
        cfg.providers.len(),
        cfg.panels.len()
    );
    let missing: Vec<String> = cfg
        .providers
        .iter()
        .filter(|p| std::env::var(&p.api_key_env).is_err())
        .map(|p| format!("{} ({})", p.name, p.api_key_env))
        .collect();
    if missing.is_empty() {
        println!("all provider keys present");
        Ok(())
    } else {
        println!("missing keys: {}", missing.join(", "));
        anyhow::bail!("{} provider key(s) missing", missing.len())
    }
}
