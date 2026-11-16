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
  gw status                     show agents, accounts and usage
  gw models                     list models the running gateway serves";

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
    /// Show connected agents, stored accounts and recent usage
    Status,
    /// List the models the running gateway serves
    Models {
        /// Print the raw JSON list
        #[arg(long)]
        json: bool,
    },
    /// Show usage from the local database
    Stats {
        /// Window to summarise, e.g. 24h, 7d, 30m
        #[arg(long, default_value = "24h")]
        since: String,
        /// Print rows as JSON
        #[arg(long)]
        json: bool,
    },
    /// Log in to a provider and store the account
    Login {
        /// One of: anthropic, openai, antigravity, kiro, command-code, opencode
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
        /// Provider name, as shown by `gw account list`
        provider: String,
        /// Account id, as shown by `gw account list`
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
            report(accounts(&cfg.state_dir)?);
            report(gw::connect::usage(&cfg.state_dir)?);
            Ok(())
        }
        Cmd::Models { json } => models(&cfg, json).await,
        Cmd::Stats { since, json } => stats(&cfg.state_dir, &since, json),
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

/// One line per provider: how many accounts, and how many need a new login.
fn accounts(state_dir: &Path) -> anyhow::Result<Vec<String>> {
    let all = gw::creds::Store::load(state_dir)?.all();
    if all.is_empty() {
        return Ok(vec!["no accounts; run `gw login <provider>`".into()]);
    }
    let mut by: std::collections::BTreeMap<&str, (usize, usize)> = Default::default();
    for e in &all {
        let n = by.entry(&e.provider).or_default();
        n.0 += 1;
        n.1 += usize::from(e.needs_reauth);
    }
    Ok(by
        .into_iter()
        .map(|(p, (n, bad))| match bad {
            0 => format!("{p}: {n} account(s)"),
            _ => format!("{p}: {n} account(s), {bad} need login"),
        })
        .collect())
}

/// `gw models`: ask the running gateway what it serves.
async fn models(cfg: &gw::config::Config, json: bool) -> anyhow::Result<()> {
    let mut req =
        reqwest::Client::new().get(format!("{}/models", gw::connect::default_url(cfg.listen)));
    if let Some(key) = read_key(cfg.api_key_file.as_deref())? {
        req = req.bearer_auth(key);
    }
    let bytes = req
        .send()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "gateway not reachable at {}: {e}; is `gw serve` running?",
                cfg.listen
            )
        })?
        .error_for_status()?
        .bytes()
        .await?;
    if json {
        println!("{}", String::from_utf8_lossy(&bytes));
        return Ok(());
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    for id in body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str())
    {
        println!("{id}");
    }
    Ok(())
}

fn report(lines: Vec<String>) {
    for line in lines {
        println!("{line}");
    }
}

/// `gw stats`: summarise the local usage database.
fn stats(state_dir: &std::path::Path, since: &str, json: bool) -> anyhow::Result<()> {
    let seconds = parse_window(since)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let store = gw::store::Store::open(state_dir)?;
    let rows = store.usage_summary(now - seconds)?;

    if json {
        // Hand-rolled so the shape stays stable and needs no derive.
        let objects: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "provider": row.provider,
                    "model": row.model,
                    "account": row.account,
                    "requests": row.requests,
                    "errors": row.errors,
                    "input": row.input,
                    "cached_read": row.cached_read,
                    "cache_write": row.cache_write,
                    "output": row.output,
                    "cache_read_share": row.cache_read_share,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&objects)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("no usage in the last {since}");
        return Ok(());
    }
    let (headers, cells) = gw::metrics::stats_table(&rows);
    let borrowed: Vec<Vec<&str>> = cells
        .iter()
        .map(|row| row.iter().map(String::as_str).collect())
        .collect();
    let view: Vec<&[&str]> = borrowed.iter().map(Vec::as_slice).collect();
    println!("{}", gw::ui::Ui::from_env().render_table(&headers, &view));
    Ok(())
}

/// `24h`, `7d`, `90m`, `45s`, or a bare count of seconds.
fn parse_window(text: &str) -> anyhow::Result<i64> {
    let text = text.trim();
    let (value, unit) = text.split_at(text.len().saturating_sub(1));
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "" => 1,
        _ => anyhow::bail!("unknown window unit in {text:?}; use s, m, h or d"),
    };
    let count: i64 = if unit.is_empty() {
        text.parse()?
    } else {
        value.parse()?
    };
    Ok(count.saturating_mul(scale))
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
