mod config;
mod deliberate;
mod mcp;
mod provider;

use anyhow::Result;
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
        #[arg(short, long, default_value = "default")]
        panel: String,
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
        Cmd::Serve => {
            use rmcp::ServiceExt;
            let service = mcp::Council::new(cli.config)
                .serve((tokio::io::stdin(), tokio::io::stdout()))
                .await?;
            service.waiting().await?;
        }

        Cmd::Init { force } => {
            let path = cli.config.unwrap_or_else(Config::default_path);
            if path.exists() && !force {
                anyhow::bail!("{} already exists (use --force)", path.display());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, config::STARTER)?;
            println!("wrote {}", path.display());
            println!("Edit it, export the API keys it names, then run `council check`.");
        }

        Cmd::Ask {
            question,
            context,
            panel,
            rounds,
            transcript,
            fresh,
        } => {
            let cfg = Config::load(cli.config.as_deref())?;
            let context = match context.as_deref() {
                Some("-") => {
                    use std::io::Read;
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s)?;
                    Some(s)
                }
                other => other.map(str::to_string),
            };
            let d = Deliberation {
                question,
                context,
                rounds: rounds.clamp(1, 6),
                panel,
                resume: !fresh,
            };
            let out = d.run(&cfg).await?;
            if transcript {
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
        }

        Cmd::Panels => {
            let cfg = Config::load(cli.config.as_deref())?;
            for p in &cfg.panels {
                println!(
                    "panel {} (chair: {})",
                    p.name,
                    p.chair.as_deref().unwrap_or("last")
                );
                for m in &p.members {
                    println!("  {:<14} {}:{}", m.name, m.provider, m.model);
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
        }

        Cmd::Check => {
            let cfg = Config::load(cli.config.as_deref())?;
            println!(
                "config OK: {} providers, {} panels",
                cfg.providers.len(),
                cfg.panels.len()
            );
            let mut missing = Vec::new();
            for p in &cfg.providers {
                if std::env::var(&p.api_key_env).is_err() {
                    missing.push(format!("{} ({})", p.name, p.api_key_env));
                }
            }
            if missing.is_empty() {
                println!("all provider keys present");
            } else {
                println!("missing keys: {}", missing.join(", "));
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
