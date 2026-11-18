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
enum ComboCmd {
    /// Show every combo and the order its targets are tried
    List,
    /// Create a combo, or replace an existing one's strategy and targets
    Set {
        name: String,
        #[arg(value_parser = PossibleValuesParser::new(gw::combo::Strategy::ALL.map(|s| s.name())))]
        strategy: String,
        /// Targets in the order to try, as provider/model (add *N to weight round_robin)
        #[arg(required = true)]
        targets: Vec<String>,
    },
    /// Delete a combo
    Rm { name: String },
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
    /// Show, create, change or delete combos (named fallback chains of provider models)
    #[command(subcommand)]
    Combo(ComboCmd),
    /// List the models the running gateway serves
    Models {
        /// Ask the providers for their live model lists first
        #[arg(long)]
        refresh: bool,
        /// Show which account can use which model (discovered providers only)
        #[arg(long, conflicts_with = "json")]
        accounts: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Log in to a provider and store the account
    Login {
        #[arg(
            value_parser = PossibleValuesParser::new(gw::providers::NAMES),
            required_unless_present = "expired"
        )]
        provider: Option<String>,
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
        /// Log in again to every account that needs it, one after another
        #[arg(long, conflicts_with_all = ["provider", "label", "device", "json"])]
        expired: bool,
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
    /// Recheck an account the provider disabled, on the next request
    Recheck {
        #[arg(value_parser = PossibleValuesParser::new(gw::providers::NAMES))]
        provider: String,
        /// Account id or label, as shown by `gw account list`
        id: String,
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
    // Die quietly when stdout closes early (`gw status | head`) instead of panicking in println!.
    // SAFETY: restoring the default disposition of a signal is async-signal-safe and runs
    // before any thread starts.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
        Cmd::Combo(cmd) => combo_cmd(&cfg, cli.config.as_deref(), cmd).await,
        Cmd::Models {
            refresh,
            accounts,
            json,
        } => {
            if refresh {
                refresh_models(&cfg).await?;
            }
            if accounts {
                return account_models(&cfg);
            }
            models(&cfg, json).await
        }
        Cmd::Setup { yes, dry_run } => {
            let config = cli
                .config
                .or_else(|| std::env::var_os("GW_CONFIG").map(PathBuf::from))
                .map(std::path::absolute)
                .transpose()?;
            setup(cfg, config.as_deref(), yes, dry_run).await
        }
        Cmd::Login {
            provider,
            label,
            device,
            no_browser,
            json,
            expired,
        } => {
            if expired {
                return gw::login::cli::relogin(no_browser, cfg.state_dir).await;
            }
            let provider = provider.expect("clap requires a provider without --expired");
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
            AccountCmd::Recheck { provider, id } => {
                let store = gw::creds::Store::load(&cfg.state_dir)?;
                anyhow::ensure!(
                    store.set_disabled_until(&provider, &id, None),
                    "no {provider} account {id}"
                );
                store.save()?;
                gw::ui::ok(&format!("{provider}/{id}: the next request rechecks it"));
                Ok(())
            }
            AccountCmd::Remove { provider, id, json } => {
                gw::login::cli::remove(&provider, &id, json, &cfg.state_dir)
            }
        },
    }
}

/// One line per provider, accounts grouped by state, then one hint per action needed.
fn accounts(state_dir: &Path, emails: bool) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let all = gw::creds::Store::load(state_dir)?.all();
    let now = gw::login::now_secs();
    // provider -> state -> names; "" is the usable group and sorts first.
    let mut by: std::collections::BTreeMap<&str, std::collections::BTreeMap<String, Vec<String>>> =
        Default::default();
    // opencode serves its free models without a login.
    if !all.iter().any(|e| e.provider == "opencode") {
        by.entry("opencode")
            .or_default()
            .insert(String::new(), vec!["free tier (no login)".into()]);
    }
    let mut login = false;
    let mut recheck: Option<u64> = None;
    for e in &all {
        // An expired mark is only awaiting its recheck on the next request.
        let state = match (
            e.needs_reauth,
            e.paused,
            e.disabled_until.filter(|until| *until > now),
        ) {
            (true, ..) => {
                login = true;
                "needs login".to_owned()
            }
            (_, true, _) => "paused".to_owned(),
            (.., Some(until)) => {
                let left = until.saturating_sub(now);
                recheck = Some(recheck.map_or(left, |r| r.max(left)));
                "disabled by provider".to_owned()
            }
            _ => String::new(),
        };
        let name = e.label.as_deref().unwrap_or(&e.id);
        let email = match e.email() {
            Some(email) if emails => format!(" <{email}>"),
            _ => String::new(),
        };
        by.entry(&e.provider)
            .or_default()
            .entry(state)
            .or_default()
            .push(format!("{name}{email}"));
    }
    let lines = by
        .into_iter()
        .map(|(provider, states)| {
            let groups: Vec<String> = states
                .into_iter()
                .map(|(state, names)| match state.as_str() {
                    "" => names.join(", "),
                    _ => format!("{state}: {}", names.join(", ")),
                })
                .collect();
            format!("{provider}: {}", groups.join("; "))
        })
        .collect();
    let mut hints = Vec::new();
    if login {
        hints.push("log in again: gw login --expired".to_owned());
    }
    if let Some(left) = recheck {
        let when = match left {
            0 => "on the next request".to_owned(),
            left => format!("within {}", gw::quota::duration(left)),
        };
        hints.push(format!(
            "disabled accounts recheck {when}, or now: gw account recheck <provider> <account>"
        ));
    }
    Ok((lines, hints))
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

/// Per-account model availability from the last discovery, one row per model.
fn account_models(cfg: &gw::config::Config) -> anyhow::Result<()> {
    let saved = gw::discover::load_accounts(&cfg.state_dir);
    if saved.is_empty() {
        anyhow::bail!("nothing discovered yet; run `gw models --refresh`");
    }
    let mut cells: Vec<Vec<String>> = Vec::new();
    for (provider, accounts) in &saved {
        let all: std::collections::BTreeSet<&String> = accounts.values().flatten().collect();
        for model in all {
            let who: Vec<&str> = accounts
                .iter()
                .filter(|(_, ids)| ids.contains(model))
                .map(|(name, _)| name.as_str())
                .collect();
            let who = if who.len() == accounts.len() {
                "all".to_owned()
            } else {
                who.join(", ")
            };
            cells.push(vec![provider.clone(), model.clone(), who]);
        }
    }
    let ui = gw::ui::Ui::from_env();
    println!(
        "{}",
        ui.render_table(
            &["provider", "model", "accounts"],
            &views(&cells).iter().map(Vec::as_slice).collect::<Vec<_>>()
        )
    );
    Ok(())
}

/// `gw setup`: detect coding tools, let the user pick, connect them with the model list.
async fn setup(
    cfg: gw::config::Config,
    config: Option<&Path>,
    yes: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let home = gw::connect::home()?;
    let url = gw::connect::default_url(cfg.listen);
    if !dry_run && gw::service::available() {
        install_service(&cfg, &home, config, yes).await?;
    }
    let models = match model_ids(&cfg).await {
        Ok(models) => models,
        Err(e) => anyhow::bail!("{e:#}\nstart it first: gw serve"),
    };
    let (accounts, _) = accounts(&cfg.state_dir, false)?;
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

/// Offer to run the gateway as a systemd user service, then make sure it is up.
async fn install_service(
    cfg: &gw::config::Config,
    home: &Path,
    config: Option<&Path>,
    yes: bool,
) -> anyhow::Result<()> {
    use gw::service;
    let text = service::unit(&service::exe()?, config);
    let installed = std::fs::read_to_string(service::unit_path(home)).is_ok();
    if !installed {
        if model_ids(cfg).await.is_ok() {
            gw::ui::hint(
                "gw is already running by hand; stop it and rerun setup to install the service",
            );
            return Ok(());
        }
        if !yes && !gw::ui::confirm("Run gw as a user service (starts at login)?", true) {
            return Ok(());
        }
    }
    let changed = service::write(home, &text)?;
    service::systemctl(&["daemon-reload"])?;
    service::systemctl(&["enable", "--quiet", service::NAME])?;
    service::systemctl(&[if changed { "restart" } else { "start" }, service::NAME])?;
    for _ in 0..20 {
        if model_ids(cfg).await.is_ok() {
            gw::ui::ok(&format!("service {}: running", service::NAME));
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    anyhow::bail!(
        "service {} did not come up; see: journalctl --user -u gw",
        service::NAME
    )
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

    let ui = gw::ui::Ui::from_env();
    ui.heading("agents");
    report(gw::connect::status(&gw::connect::home()?, state_dir)?);
    let (lines, hints) = accounts(state_dir, emails)?;
    ui.heading("\naccounts");
    report(lines);
    for hint in hints {
        ui.hint(&hint);
    }
    if rows.is_empty() {
        println!("\nno usage in the last {since}");
    } else {
        // Group by provider, then model, busiest model and account first within each.
        let mut model_requests = std::collections::HashMap::new();
        for row in &rows {
            *model_requests
                .entry((row.provider.clone(), row.model.clone()))
                .or_insert(0) += row.requests;
        }
        let busy = |row: &gw::store::UsageSummary| {
            model_requests[&(row.provider.clone(), row.model.clone())]
        };
        rows.sort_by(|a, b| {
            a.provider
                .cmp(&b.provider)
                .then(busy(b).cmp(&busy(a)))
                .then(a.model.cmp(&b.model))
                .then(b.requests.cmp(&a.requests))
        });
        let prices = gw::pricing::Prices::load(state_dir).await;
        let (headers, mut cells) = gw::metrics::stats_table(&rows, &prices);
        blank_repeats(&mut cells, 2);
        print!(
            "\n{}",
            ui.render_table(
                &headers,
                &views(&cells).iter().map(Vec::as_slice).collect::<Vec<_>>()
            )
        );
    }
    combos(cfg, state_dir, &ui).await;
    // Accounts that need a login are already flagged above; their figures are stale.
    let relogin: Vec<(String, String)> = gw::creds::Store::load(state_dir)?
        .all()
        .into_iter()
        .filter(|e| e.needs_reauth)
        .map(|e| (e.provider.clone(), e.label.unwrap_or(e.id)))
        .collect();
    let skip =
        |provider: &str, account: &str| relogin.iter().any(|(p, a)| p == provider && a == account);
    let mut windows: Vec<_> = windows
        .into_iter()
        .filter(|w| !skip(&w.provider, &w.account))
        .collect();
    windows.sort_by(|a, b| {
        (&a.provider, &a.account, &a.name).cmp(&(&b.provider, &b.account, &b.name))
    });
    if !windows.is_empty() {
        let now = gw::login::now_secs();
        let mut cells: Vec<Vec<String>> = windows
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
        blank_repeats(&mut cells, 2);
        let headers = ["provider", "account", "window", "used", "resets in"];
        print!(
            "\n{}",
            ui.render_table(
                &headers,
                &views(&cells).iter().map(Vec::as_slice).collect::<Vec<_>>()
            )
        );
    }
    for f in failures.iter().filter(|f| !skip(&f.provider, &f.account)) {
        gw::ui::warn(&format!(
            "quota: {} {}: {}",
            f.provider, f.account, f.message
        ));
    }
    Ok(())
}

/// `gw combo list|set|rm`. Edits apply when the gateway next starts.
async fn combo_cmd(
    cfg: &gw::config::Config,
    config: Option<&Path>,
    cmd: ComboCmd,
) -> anyhow::Result<()> {
    let path = gw::config::Config::path(config);
    match cmd {
        ComboCmd::List => {
            if cfg.combos.is_empty() {
                println!("no combos; create one with `gw combo set <name> <strategy> provider/model ...`");
            } else {
                combos(cfg, &cfg.state_dir, &gw::ui::Ui::from_env()).await;
            }
            return Ok(());
        }
        ComboCmd::Set {
            name,
            strategy,
            targets,
        } => {
            let strategy = gw::combo::Strategy::parse(&strategy).expect("checked by clap");
            let targets = targets
                .iter()
                .map(|t| gw::combo_edit::parse_target(t))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let known = model_ids(cfg).await.unwrap_or_default();
            for t in &targets {
                if !known.is_empty()
                    && !known
                        .iter()
                        .any(|m| m == &format!("{}/{}", t.provider, t.model) || m == &t.model)
                {
                    eprintln!(
                        "warning: `{}/{}` is not in `gw models`",
                        t.provider, t.model
                    );
                }
            }
            gw::combo_edit::set(&path, &name, strategy, &targets)?;
            println!("saved combo/{name}");
        }
        ComboCmd::Rm { name } => {
            gw::combo_edit::remove(&path, &name)?;
            println!("removed combo/{name}");
        }
    }
    println!("restart `gw serve` to apply");
    Ok(())
}

/// Combos as a table, one row per target in the order they are tried.
async fn combos(cfg: &gw::config::Config, state_dir: &Path, ui: &gw::ui::Ui) {
    if cfg.combos.is_empty() {
        return;
    }
    let prices = gw::pricing::Prices::cached(state_dir);
    let mut cells: Vec<Vec<String>> = Vec::new();
    for (name, combo) in &cfg.combos {
        let mut targets: Vec<_> = combo.targets.iter().collect();
        if combo.strategy == gw::combo::Strategy::QuotaAware {
            let windows = gw::quota::load(cfg, false)
                .await
                .map(|r| r.0)
                .unwrap_or_default();
            let room = |t: &gw::combo::Target| {
                gw::quota::headroom(&windows, &t.provider, &t.model).unwrap_or(-1.0)
            };
            targets.sort_by(|a, b| room(b).total_cmp(&room(a)));
        }
        if combo.strategy == gw::combo::Strategy::Cheapest {
            targets.sort_by(|a, b| {
                prices
                    .rank(&a.provider, &a.model)
                    .total_cmp(&prices.rank(&b.provider, &b.model))
            });
        }
        for (i, t) in targets.iter().enumerate() {
            let first = i == 0;
            cells.push(vec![
                if first {
                    format!("combo/{name}")
                } else {
                    String::new()
                },
                if first {
                    combo.strategy.name().to_owned()
                } else {
                    String::new()
                },
                (i + 1).to_string(),
                t.provider.clone(),
                t.model.clone(),
            ]);
        }
    }
    let headers = ["combo", "strategy", "#", "provider", "model"];
    print!(
        "\n{}",
        ui.render_table(
            &headers,
            &views(&cells).iter().map(Vec::as_slice).collect::<Vec<_>>()
        )
    );
}

/// Blank a leading cell that repeats the row above, for the first `depth` columns.
fn blank_repeats(cells: &mut [Vec<String>], depth: usize) {
    for row in (1..cells.len()).rev() {
        let same = (0..depth)
            .take_while(|&col| cells[row][col] == cells[row - 1][col])
            .count();
        for cell in &mut cells[row][..same] {
            cell.clear();
        }
    }
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

#[cfg(test)]
mod tests {
    #[test]
    fn blank_repeats_keeps_only_the_first_of_each_group() {
        let mut cells: Vec<Vec<String>> = [
            ["a", "x", "1"],
            ["a", "x", "2"],
            ["a", "y", "3"],
            ["b", "y", "4"],
        ]
        .iter()
        .map(|r| r.iter().map(|c| (*c).to_owned()).collect())
        .collect();
        super::blank_repeats(&mut cells, 2);
        let got: Vec<Vec<&str>> = cells
            .iter()
            .map(|r| r.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            got,
            [
                ["a", "x", "1"],
                ["", "", "2"],
                ["", "y", "3"],
                ["b", "y", "4"]
            ]
        );
    }
}
