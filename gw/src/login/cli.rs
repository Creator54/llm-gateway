//! `gw login` and `gw account` commands.
//!
//! Only the CLI concerns itself with output: the login flows report progress on their
//! own, and this module prints the result lines. Tokens are never printed.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{providers, Flow, Options};
use crate::{creds, ui};

/// Providers the login flows know.
pub const PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "openai-api",
    "antigravity",
    "kiro",
    "command-code",
    "opencode",
];

/// Where an API key comes from.
pub enum KeySource {
    Prompt,
    Env(String),
    /// Given on the command line, so it lands in shell history.
    Value(String),
}

/// Providers that also take a direct API key.
pub const KEY_PROVIDERS: [&str; 4] = ["anthropic", "openai-api", "command-code", "opencode"];

pub struct Login {
    pub provider: String,
    pub key: Option<KeySource>,
    pub device: bool,
    pub no_browser: bool,
    pub label: Option<String>,
    pub json: bool,
}

pub struct Import {
    pub from: Option<PathBuf>,
    pub dry_run: bool,
    pub json: bool,
}

/// Run a login flow and store the account it yields.
pub async fn login(args: Login, dir: PathBuf) -> Result<()> {
    let provider = args.provider;
    if !PROVIDERS.contains(&provider.as_str()) {
        anyhow::bail!(
            "unknown provider {provider}; expected one of: {}",
            PROVIDERS.join(", ")
        );
    }
    if args.device && provider != "openai" {
        anyhow::bail!("{provider} has no device-code flow; drop --device");
    }
    if args.key.is_some() && !KEY_PROVIDERS.contains(&provider.as_str()) {
        let api = format!("{provider}-api");
        if KEY_PROVIDERS.contains(&api.as_str()) {
            anyhow::bail!(
                "{provider} takes a subscription login; API keys go to {api}: gw login {api} --key"
            );
        }
        anyhow::bail!(
            "{provider} has no API key access; keys work for: {}",
            KEY_PROVIDERS.join(", ")
        );
    }
    let label = Some(args.label.unwrap_or_else(|| provider.clone()));
    let opts = Options {
        dir,
        flow: if args.device {
            Flow::Device
        } else {
            Flow::Auto
        },
        // A JSON run must never wait on a browser or a stdin paste.
        open_browser: !args.no_browser && !args.json,
        allow_paste: !args.json,
        label,
        ..Options::default()
    };
    let http = reqwest::Client::new();
    let credential = match args.key {
        Some(source) => providers::key::login(&http, &provider, source).await?,
        None => providers::run(&provider, &http, &opts).await?,
    };
    let entry = credential.entry(&provider, opts.label.clone());
    let store = creds::Store::load(&opts.dir)?;
    store.upsert(entry.clone());
    store.save()?;
    if args.json {
        print_json(&serde_json::json!({
            "provider": entry.provider,
            "id": entry.id,
            "label": entry.label,
        }));
    } else {
        ui::ok(&format!(
            "{} account {} labeled {}",
            entry.provider,
            entry.id,
            entry.label.as_deref().unwrap_or("-")
        ));
    }
    Ok(())
}

/// Log in again to each account marked as needing it, keeping its label. Each step names
/// the email the account was stored with; signing in as another one needs a yes, and
/// one already held by a different account is refused, so accounts stay distinct.
pub async fn relogin(no_browser: bool, dir: PathBuf) -> Result<()> {
    let store = creds::Store::load(&dir)?;
    let stale: Vec<creds::Entry> = store.all().into_iter().filter(|e| e.needs_reauth).collect();
    if stale.is_empty() {
        ui::ok("no account needs a login");
        return Ok(());
    }
    let http = reqwest::Client::new();
    let total = stale.len();
    let mut done = 0;
    for (n, old) in stale.into_iter().enumerate() {
        let name = old.label.clone().unwrap_or_else(|| old.id.clone());
        let expected = old.email().map(str::to_owned);
        let target = match &expected {
            Some(email) => format!("{} {name}: sign in as {email}", old.provider),
            None => format!("{} {name}", old.provider),
        };
        ui::step(n + 1, total, &target);
        if !ui::confirm("log in now?", true) {
            continue;
        }
        let opts = Options {
            dir: dir.clone(),
            open_browser: !no_browser,
            label: Some(name.clone()),
            ..Options::default()
        };
        let entry = match providers::run(&old.provider, &http, &opts).await {
            Ok(tokens) => tokens.entry(&old.provider, opts.label.clone()),
            Err(err) => {
                ui::err(&format!("{} {name}: {err:#}", old.provider));
                continue;
            }
        };
        if let (Some(want), Some(got)) = (expected.as_deref(), entry.email()) {
            if !want.eq_ignore_ascii_case(got)
                && !ui::confirm(
                    &format!("signed in as {got}, not {want}; replace {name} with it?"),
                    false,
                )
            {
                ui::warn(&format!(
                    "{} {name}: kept; still needs a login",
                    old.provider
                ));
                continue;
            }
        }
        let store = creds::Store::load(&dir)?;
        if let Some(other) = store
            .get(&entry.provider, &entry.id)
            .filter(|e| e.id != old.id)
        {
            let other = other.label.unwrap_or(other.id);
            ui::err(&format!(
                "{} {name}: that sign-in is already account {other}; use a different one",
                old.provider
            ));
            continue;
        }
        if entry.id != old.id {
            store.remove(&old.provider, &old.id);
        }
        store.upsert(entry);
        store.save()?;
        ui::ok(&format!("{} {name} logged in", old.provider));
        done += 1;
    }
    ui::info(&format!("{done} of {total} accounts logged in"));
    Ok(())
}

/// Import opencodex credentials into the store.
pub fn import(args: Import, dir: PathBuf) -> Result<()> {
    let from = match args.from {
        Some(path) => path,
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("HOME is not set; pass --from"))?
            .join(".opencodex"),
    };
    let entries = creds::import_opencodex(&from)?;
    let store = creds::Store::load(&dir)?;
    let mut seen = HashSet::new();
    let mut rows: Vec<[String; 4]> = Vec::new();
    for entry in entries {
        // The same (provider, id) twice in one file is a duplicate, not an update.
        let key = (entry.provider.clone(), entry.id.clone());
        let action = if !seen.insert(key) {
            "duplicate"
        } else {
            match store.get(&entry.provider, &entry.id) {
                Some(_) => "replace",
                None => "add",
            }
        };
        if action != "duplicate" && !args.dry_run {
            store.upsert(entry.clone());
        }
        rows.push([
            entry.provider,
            entry.id,
            entry.label.unwrap_or_else(|| "-".to_string()),
            action.to_string(),
        ]);
    }
    if !args.dry_run && !rows.is_empty() {
        store.save()?;
    }
    if args.json {
        let items: Vec<_> = rows
            .iter()
            .map(|r| serde_json::json!({"provider": r[0], "id": r[1], "label": r[2], "action": r[3]}))
            .collect();
        print_json(&serde_json::Value::Array(items));
        return Ok(());
    }
    if rows.is_empty() {
        ui::info(&format!("no accounts found in {}", from.display()));
        return Ok(());
    }
    let headers = ["provider", "id", "label", "action"];
    let cells: Vec<Vec<&str>> = rows
        .iter()
        .map(|r| r.iter().map(String::as_str).collect())
        .collect();
    let lines: Vec<&[&str]> = cells.iter().map(|c| c.as_slice()).collect();
    print!("{}", ui::Ui::from_env().render_table(&headers, &lines));
    for r in &rows {
        match r[3].as_str() {
            "duplicate" => ui::info(&format!("skipped duplicate {} / {}", r[0], r[1])),
            "replace" => ui::info(&format!("replaced existing {} / {}", r[0], r[1])),
            _ => {}
        }
    }
    let verb = if args.dry_run {
        "would store"
    } else {
        "stored"
    };
    let stored = rows.iter().filter(|r| r[3] != "duplicate").count();
    ui::info(&format!(
        "{verb} {stored} account(s) from {}",
        from.display()
    ));
    Ok(())
}

/// Print the stored accounts.
pub fn list(json: bool, dir: PathBuf) -> Result<()> {
    let store = creds::Store::load(&dir)?;
    let entries = store.all();
    if json {
        let items: Vec<_> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "provider": e.provider,
                    "id": e.id,
                    "label": e.label,
                    "email": e.email(),
                    "paused": e.paused,
                    "needs_reauth": e.needs_reauth,
                })
            })
            .collect();
        print_json(&serde_json::Value::Array(items));
        return Ok(());
    }
    if entries.is_empty() {
        ui::info(&format!("no accounts in {}", dir.display()));
        return Ok(());
    }
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|e| {
            let mut flags = Vec::new();
            if e.paused {
                flags.push("paused");
            }
            if e.needs_reauth {
                flags.push("needs_reauth");
            }
            vec![
                e.provider.clone(),
                e.id.clone(),
                e.label.clone().unwrap_or_else(|| "-".to_string()),
                e.email().unwrap_or("-").to_owned(),
                match flags.join(",") {
                    empty if empty.is_empty() => "-".to_string(),
                    joined => joined,
                },
            ]
        })
        .collect();
    let cell_refs: Vec<Vec<&str>> = rows
        .iter()
        .map(|r| r.iter().map(String::as_str).collect())
        .collect();
    let lines: Vec<&[&str]> = cell_refs.iter().map(|c| c.as_slice()).collect();
    print!(
        "{}",
        ui::Ui::from_env().render_table(&["provider", "id", "label", "email", "flags"], &lines)
    );
    Ok(())
}

/// Remove one account, asking for confirmation unless `json` is set.
pub fn remove(provider: &str, id: &str, json: bool, dir: &Path) -> Result<()> {
    let store = creds::Store::load(dir)?;
    // The label is what `gw login` was given, so accept it as well as the opaque id.
    let id = &store
        .accounts(provider)
        .into_iter()
        .find(|e| e.id == id || e.label.as_deref() == Some(id))
        .map(|e| e.id)
        .ok_or_else(|| anyhow::anyhow!("no {provider} account {id:?}; see `gw account list`"))?;
    if !json && !ui::confirm(&format!("remove {provider} / {id}?"), false) {
        ui::info("nothing removed");
        return Ok(());
    }
    store.remove(provider, id);
    store
        .save()
        .with_context(|| format!("saving {}", dir.display()))?;
    if json {
        print_json(&serde_json::json!({
            "provider": provider,
            "id": id,
            "removed": true,
        }));
    } else {
        ui::ok(&format!("removed {provider} / {id}"));
    }
    Ok(())
}

fn print_json(value: &serde_json::Value) {
    println!("{value}");
}
