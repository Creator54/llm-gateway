use clap::builder::PossibleValuesParser;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

const HELP_TEMPLATE: &str = "\
{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}";

const EXAMPLES: &str = "\
Examples:
  gw serve                      run the gateway
  gw login anthropic work       log in and name the account \"work\"
  gw setup                      connect the coding tools found on this machine
  gw models --refresh           update model lists from the providers
  gw status                     show agents, accounts and usage
  gw account list               show stored accounts";

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
        #[arg(value_parser = PossibleValuesParser::new(gw::connect::AGENTS))]
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
        #[arg(value_parser = PossibleValuesParser::new(gw::connect::AGENTS))]
        agent: String,
    },
    /// Find installed coding tools and point the ones you choose at this gateway
    Setup {
        /// Connect every detected tool without asking
        #[arg(long, short)]
        yes: bool,
        /// Show what would change without writing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Show connected agents, accounts, quota and recent usage
    Status {
        /// Window of usage to summarise, e.g. 24h, 7d, 30m
        #[arg(long, default_value = "7d")]
        since: String,
        /// Fetch quota from the providers instead of using the cache
        #[arg(long)]
        refresh: bool,
        /// Show each account's login email
        #[arg(long)]
        emails: bool,
        /// Print usage and quota as JSON
        #[arg(long)]
        json: bool,
    },
    /// List the models the running gateway serves
    Models {
        /// Ask the providers for their live model lists first
        #[arg(long)]
        refresh: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Log in to a provider and store the account
    Login {
        #[arg(value_parser = PossibleValuesParser::new(gw::providers::NAMES))]
        provider: String,
        /// Account label (default: the provider name)
        label: Option<String>,
        /// Force the device-code flow (openai only)
        #[arg(long)]
        device: bool,
        /// Print the URL instead of opening a browser
        #[arg(long)]
        no_browser: bool,
        /// Print JSON
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
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// List stored accounts
    List {
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove one stored account
    Remove {
        #[arg(value_parser = PossibleValuesParser::new(gw::providers::NAMES))]
        provider: String,
        /// Account id, as shown by `gw account list`
        id: String,
        /// Print JSON and skip the confirmation prompt
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
                models: model_ids(&cfg).await.unwrap_or_default(),
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
        Cmd::Status {
            since,
            refresh,
            emails,
            json,
        } => status(&cfg, &since, refresh, emails, json).await,
        Cmd::Models { refresh, json } => {
            if refresh {
                refresh_models(&cfg).await?;
            }
            models(&cfg, json).await
        }
        Cmd::Setup { yes, dry_run } => setup(cfg, yes, dry_run).await,
        Cmd::Login {
            provider,
            label,
            device,
            no_browser,
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
fn accounts(state_dir: &Path, emails: bool) -> anyhow::Result<Vec<String>> {
    let all = gw::creds::Store::load(state_dir)?.all();
    if all.is_empty() {
        return Ok(vec!["no accounts; run `gw login <provider>`".into()]);
    }
    let mut by: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    for e in &all {
        let name = e.label.as_deref().unwrap_or(&e.id);
        let flag = if e.needs_reauth { " (needs login)" } else { "" };
        let email = match e.email() {
            Some(email) if emails => format!(" <{email}>"),
            _ => String::new(),
        };
        by.entry(&e.provider)
            .or_default()
            .push(format!("{name}{email}{flag}"));
    }
    Ok(by
        .into_iter()
        .map(|(p, names)| format!("{p}: {}", names.join(", ")))
        .collect())
}

/// Ask the running gateway for its model list (raw JSON body).
async fn fetch_models(cfg: &gw::config::Config) -> anyhow::Result<Vec<u8>> {
    let mut req =
        reqwest::Client::new().get(format!("{}/models", gw::connect::default_url(cfg.listen)));
    if let Some(key) = read_key(cfg.api_key_file.as_deref())? {
        req = req.bearer_auth(key);
    }
    let response = req
        .send()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "gateway not reachable at {}: {}; is `gw serve` running?",
                cfg.listen,
                if e.is_connect() {
                    "connection refused".to_owned()
                } else {
                    e.to_string()
                }
            )
        })?
        .error_for_status()?;
    let serving = response
        .headers()
        .get("x-gw-state-dir")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Some(dir) = serving.filter(|dir| std::path::Path::new(dir) != cfg.state_dir) {
        anyhow::bail!(
            "the gateway on {} uses state dir {dir}, not {}; pass the same --config",
            cfg.listen,
            cfg.state_dir.display()
        );
    }
    Ok(response.bytes().await?.to_vec())
}

async fn model_ids(cfg: &gw::config::Config) -> anyhow::Result<Vec<String>> {
    let body: serde_json::Value = serde_json::from_slice(&fetch_models(cfg).await?)?;
    Ok(body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str().map(str::to_owned))
        .collect())
}

/// `gw models --refresh`: fetch live lists from the providers, save them, and bring every
/// connected agent's model list up to date.
async fn refresh_models(cfg: &gw::config::Config) -> anyhow::Result<()> {
    for (provider, result) in gw::discover::refresh(cfg).await? {
        match result {
            Ok(count) => gw::ui::ok(&format!("{provider}: {count} models")),
            Err(e) => gw::ui::warn(&format!("{provider}: {e:#}")),
        }
    }
    let agents = gw::connect::connected(&cfg.state_dir)?;
    if agents.is_empty() {
        return Ok(());
    }
    // The running gateway has to re-read the saved lists before it can serve them.
    let opts = gw::connect::Connect {
        url: gw::connect::default_url(cfg.listen),
        key: read_key(cfg.api_key_file.as_deref())?,
        dry_run: false,
        models: model_ids(cfg).await?,
        state_dir: cfg.state_dir.clone(),
    };
    let home = gw::connect::home()?;
    for agent in agents {
        report(gw::connect::connect(&agent, &opts, &home)?);
    }
    Ok(())
}

/// `gw models`: ask the running gateway what it serves.
async fn models(cfg: &gw::config::Config, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", String::from_utf8_lossy(&fetch_models(cfg).await?));
        return Ok(());
    }
    for id in model_ids(cfg).await? {
        println!("{id}");
    }
    Ok(())
}

/// `gw setup`: detect coding tools, let the user pick, connect them with the model list.
async fn setup(cfg: gw::config::Config, yes: bool, dry_run: bool) -> anyhow::Result<()> {
    let home = gw::connect::home()?;
    let url = gw::connect::default_url(cfg.listen);
    let models = match model_ids(&cfg).await {
        Ok(models) => models,
        Err(e) => anyhow::bail!("{e:#}\nstart it first: gw serve"),
    };
    let accounts = accounts(&cfg.state_dir, false)?;
    gw::ui::heading("Gateway");
    gw::ui::ok(&format!("{url}: running, {} models", models.len()));
    for line in &accounts {
        gw::ui::info(line);
    }
    let found = gw::connect::detect(&home);
    if found.is_empty() {
        gw::ui::warn("no coding tools found (looked for pi, opencode, claude, codex)");
        return Ok(());
    }
    gw::ui::heading("Coding tools");
    for (i, agent) in found.iter().enumerate() {
        gw::ui::info(&format!("{}. {agent}", i + 1));
    }
    let chosen: Vec<&str> = if yes {
        found.clone()
    } else {
        let answer = gw::ui::prompt("Connect which? (numbers like 1,3; Enter for all; n for none)");
        match answer.as_str() {
            "" => found.clone(),
            "n" | "none" => Vec::new(),
            list => list
                .split(',')
                .filter_map(|n| n.trim().parse::<usize>().ok())
                .filter_map(|n| n.checked_sub(1).and_then(|i| found.get(i)).copied())
                .collect(),
        }
    };
    if chosen.is_empty() {
        gw::ui::info("nothing selected");
        return Ok(());
    }
    let key_file = cfg.api_key_file.clone();
    let opts = gw::connect::Connect {
        url,
        key: read_key(key_file.as_deref())?,
        dry_run,
        models,
        state_dir: cfg.state_dir,
    };
    if !yes && !dry_run {
        for agent in &chosen {
            report(gw::connect::connect(
                agent,
                &gw::connect::Connect {
                    dry_run: true,
                    ..opts.clone()
                },
                &home,
            )?);
        }
        if !gw::ui::confirm("Apply?", true) {
            return Ok(());
        }
    }
    for agent in chosen {
        report(gw::connect::connect(agent, &opts, &home)?);
    }
    Ok(())
}

fn report(lines: Vec<String>) {
    for line in lines {
        println!("{line}");
    }
}

/// `gw status`: connected agents, accounts, local usage and subscription quota.
async fn status(
    cfg: &gw::config::Config,
    since: &str,
    refresh: bool,
    emails: bool,
    json: bool,
) -> anyhow::Result<()> {
    let state_dir = &cfg.state_dir;
    let seconds = parse_window(since)?;
    let store = gw::store::Store::open(state_dir)?;
    let mut rows = store.usage_summary(gw::store::now_secs() - seconds)?;
    gw::store::label_accounts(&mut rows, state_dir)?;
    let (windows, failures) = gw::quota::load(cfg, refresh).await?;

    if json {
        // Hand-rolled so the shape stays stable.
        let usage: Vec<serde_json::Value> = rows
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
        let quota: Vec<serde_json::Value> = windows
            .iter()
            .map(|w| {
                serde_json::json!({
                    "provider": w.provider,
                    "account": w.account,
                    "window": w.name,
                    "used_percent": w.used_percent,
                    "resets_at": w.resets_at,
                })
            })
            .collect();
        let out = serde_json::json!({ "usage": usage, "quota": quota });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    report(gw::connect::status(&gw::connect::home()?, state_dir)?);
    report(accounts(state_dir, emails)?);
    let ui = gw::ui::Ui::from_env();
    if rows.is_empty() {
        println!("\nno usage in the last {since}");
    } else {
        rows.sort_by_key(|row| std::cmp::Reverse(row.requests));
        let prices = gw::pricing::Prices::load(state_dir).await;
        let (headers, cells) = gw::metrics::stats_table(&rows, &prices);
        println!(
            "\n{}",
            ui.render_table(
                &headers,
                &views(&cells).iter().map(Vec::as_slice).collect::<Vec<_>>()
            )
        );
    }
    if !windows.is_empty() {
        let now = gw::login::now_secs();
        let cells: Vec<Vec<String>> = windows
            .iter()
            .map(|w| {
                let stale = failures
                    .iter()
                    .any(|f| f.provider == w.provider && f.account == w.account);
                vec![
                    w.provider.clone(),
                    w.account.clone(),
                    w.name.clone(),
                    format!(
                        "{:.0}%{}",
                        w.used_percent,
                        if stale { " (stale)" } else { "" }
                    ),
                    w.resets_at.map_or_else(String::new, |at| {
                        gw::quota::duration(at.saturating_sub(now))
                    }),
                ]
            })
            .collect();
        let headers = ["provider", "account", "window", "used", "resets in"];
        println!(
            "\n{}",
            ui.render_table(
                &headers,
                &views(&cells).iter().map(Vec::as_slice).collect::<Vec<_>>()
            )
        );
    }
    for f in failures {
        gw::ui::warn(&format!(
            "quota: {} {}: {}",
            f.provider, f.account, f.message
        ));
    }
    Ok(())
}

fn views(cells: &[Vec<String>]) -> Vec<Vec<&str>> {
    cells
        .iter()
        .map(|row| row.iter().map(String::as_str).collect())
        .collect()
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
