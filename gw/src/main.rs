use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

const HELP_TEMPLATE: &str = "\
{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}";

const EXAMPLES: &str = "\
Examples:
  gw serve                      run the gateway
  gw connect pi                 point pi at the gateway
  gw login anthropic            log in and store the account
  gw account list               show stored accounts
  gw status                     show accounts and routes";

#[derive(Parser)]
#[command(
    name = "gw",
    version,
    about = "One local gateway for all your LLM agents.",
    styles = gw::ui::styles(),
    help_template = HELP_TEMPLATE,
    after_help = EXAMPLES
)]
struct Cli {
    /// Config file (default: $GW_CONFIG or ~/.config/gw/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the gateway
    Serve,
    /// Point an agent's config at this gateway
    Connect {
        /// One of: pi, opencode, claude, codex
        agent: String,
        /// Base URL to write (default: http://<listen>/v1)
        #[arg(long)]
        url: Option<String>,
        /// File holding the gateway key
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// Show what would change without writing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Restore the values `gw connect` replaced
    Disconnect {
        /// One of: pi, opencode, claude, codex
        agent: String,
    },
    /// Show which agents are connected
    Status,
    /// Log in to a provider and store the account
    Login {
        /// One of: anthropic, claude, openai, google-antigravity, kiro, command-code, opencode-zen
        provider: String,
        /// Force the device-code flow (openai only)
        #[arg(long)]
        device: bool,
        /// Print the URL instead of opening a browser
        #[arg(long)]
        no_browser: bool,
        /// Account label (default: the provider name)
        #[arg(long)]
        label: Option<String>,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Manage stored accounts
    Account {
        #[command(subcommand)]
        cmd: AccountCmd,
    },
}

#[derive(Subcommand)]
enum AccountCmd {
    /// Import accounts from an opencodex directory
    Import {
        /// Source directory (default: ~/.opencodex)
        #[arg(long)]
        from: Option<PathBuf>,
        /// Show what would be stored without writing
        #[arg(long)]
        dry_run: bool,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// List stored accounts
    List {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Remove one stored account
    Remove {
        provider: String,
        id: String,
        /// Machine-readable output and no confirmation prompt
        #[arg(long)]
        json: bool,
    },
}

fn main() -> anyhow::Result<()> {
    // Two workers are enough for a proxy and keep per-thread memory low. Override with GW_THREADS.
    let threads = std::env::var("GW_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let cfg = gw::config::Config::load(cli.config.as_deref())?;
    match cli.cmd {
        Cmd::Serve => gw::server::run(cfg).await,
        Cmd::Connect {
            agent,
            url,
            key_file,
            dry_run,
        } => {
            let key_file = key_file.or_else(|| cfg.api_key_file.clone());
            let opts = gw::connect::Connect {
                url: url.unwrap_or_else(|| gw::connect::default_url(cfg.listen)),
                key: read_key(key_file.as_deref())?,
                dry_run,
                state_dir: cfg.state_dir,
            };
            report(gw::connect::connect(&agent, &opts, &gw::connect::home()?)?);
            Ok(())
        }
        Cmd::Disconnect { agent } => {
            report(gw::connect::disconnect(
                &agent,
                &gw::connect::home()?,
                &cfg.state_dir,
            )?);
            Ok(())
        }
        Cmd::Status => {
            report(gw::connect::status(&gw::connect::home()?, &cfg.state_dir)?);
            report(gw::connect::usage(&cfg.state_dir)?);
            Ok(())
        }
        Cmd::Login {
            provider,
            device,
            no_browser,
            label,
            json,
        } => {
            let args = gw::login::cli::Login {
                provider,
                device,
                no_browser,
                label,
                json,
            };
            gw::login::cli::login(args, cfg.state_dir).await
        }
        Cmd::Account { cmd } => match cmd {
            AccountCmd::Import {
                from,
                dry_run,
                json,
            } => gw::login::cli::import(
                gw::login::cli::Import {
                    from,
                    dry_run,
                    json,
                },
                cfg.state_dir,
            ),
            AccountCmd::List { json } => gw::login::cli::list(json, cfg.state_dir),
            AccountCmd::Remove { provider, id, json } => {
                gw::login::cli::remove(&provider, &id, json, &cfg.state_dir)
            }
        },
    }
}

fn report(lines: Vec<String>) {
    for line in lines {
        println!("{line}");
    }
}

/// Read the gateway key, if any; a missing file is an error, a missing option is not.
fn read_key(path: Option<&Path>) -> anyhow::Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let key = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?
        .trim()
        .to_string();
    if key.is_empty() {
        anyhow::bail!("{}: empty key", path.display());
    }
    Ok(Some(key))
}
