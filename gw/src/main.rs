use clap::{Parser, Subcommand};
use std::path::PathBuf;

const HELP_TEMPLATE: &str = "\
{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}";

const EXAMPLES: &str = "\
Examples:
  gw serve                      run the gateway
  gw connect pi                 point pi at the gateway
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
    }
}
