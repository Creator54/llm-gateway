//! `gw combo set|rm`: edit the `[combos]` table of the config file, keeping its comments.

use crate::combo::{Strategy, Target};
use crate::{config::Config, providers};
use anyhow::{bail, Context};
use std::path::Path;
use toml_edit::{value, Array, DocumentMut, InlineTable, Item, Table};

/// Parse `provider/model` or `provider/model*weight`.
pub fn parse_target(spec: &str) -> anyhow::Result<Target> {
    let (provider, rest) = spec
        .split_once('/')
        .with_context(|| format!("target `{spec}` must look like provider/model"))?;
    let provider = providers::canonical(provider);
    if !providers::is_provider(provider) {
        bail!(
            "unknown provider `{provider}` in `{spec}` (one of: {})",
            providers::NAMES.join(", ")
        );
    }
    let (model, weight) = match rest.rsplit_once('*') {
        Some((model, w)) => (
            model,
            w.parse::<u32>()
                .with_context(|| format!("weight in `{spec}` must be a number"))?,
        ),
        None => (rest, 0),
    };
    if model.is_empty() {
        bail!("target `{spec}` has no model");
    }
    Ok(Target {
        provider: provider.to_owned(),
        model: model.to_owned(),
        weight,
    })
}

/// Create the combo `name`, or replace its strategy and targets, in the file at `path`.
pub fn set(path: &Path, name: &str, strategy: Strategy, targets: &[Target]) -> anyhow::Result<()> {
    if name.is_empty() || name.contains(|c: char| c == '/' || c.is_whitespace()) {
        bail!("combo name `{name}` must not be empty or contain `/` or spaces");
    }
    if targets.is_empty() {
        bail!("a combo needs at least one target");
    }
    let mut doc = read(path)?;
    let combos = table(&mut doc, "combos")?;
    let entry = combos
        .entry(name)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .with_context(|| format!("combos.{name} is not a table"))?;
    entry.set_implicit(false);
    entry.insert("strategy", value(strategy.name()));
    let mut list = Array::new();
    for t in targets {
        let mut row = InlineTable::new();
        row.insert("provider", t.provider.as_str().into());
        row.insert("model", t.model.as_str().into());
        if t.weight > 0 {
            row.insert("weight", i64::from(t.weight).into());
        }
        list.push(row);
    }
    list.set_trailing_comma(true);
    list.set_trailing("\n");
    entry.insert("targets", value(list));
    write(path, &doc)
}

/// Delete the combo `name`.
pub fn remove(path: &Path, name: &str) -> anyhow::Result<()> {
    let mut doc = read(path)?;
    let removed = doc
        .get_mut("combos")
        .and_then(Item::as_table_mut)
        .and_then(|t| t.remove(name));
    if removed.is_none() {
        bail!("no combo named `{name}`");
    }
    write(path, &doc)
}

fn table<'a>(doc: &'a mut DocumentMut, key: &str) -> anyhow::Result<&'a mut Table> {
    let item = doc.entry(key).or_insert_with(|| Item::Table(Table::new()));
    item.as_table_mut()
        .with_context(|| format!("`{key}` in the config is not a table"))
}

fn read(path: &Path) -> anyhow::Result<DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse()
            .with_context(|| format!("{}: not valid TOML", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

/// Refuse to save anything the gateway would fail to load.
fn write(path: &Path, doc: &DocumentMut) -> anyhow::Result<()> {
    let text = doc.to_string();
    toml::from_str::<Config>(&text).context("the edited config would not load")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text).with_context(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(spec: &str) -> Target {
        parse_target(spec).unwrap()
    }

    #[test]
    fn parses_targets() {
        let t = target("google-antigravity/gemini-3.8-flash*2");
        assert_eq!(
            (t.provider.as_str(), t.model.as_str(), t.weight),
            ("antigravity", "gemini-3.8-flash", 2)
        );
        assert_eq!(
            target("command-code/deepseek/deepseek-v4.1-flash").model,
            "deepseek/deepseek-v4.1-flash"
        );
        assert!(parse_target("nope/x").is_err());
        assert!(parse_target("openai").is_err());
        assert!(parse_target("openai/").is_err());
    }

    #[test]
    fn set_and_remove_keep_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("gw-combo-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# mine\nlisten = \"127.0.0.1:1\"\n").unwrap();

        set(
            &path,
            "a",
            Strategy::Failover,
            &[target("openai/gpt-6-luna")],
        )
        .unwrap();
        set(
            &path,
            "a",
            Strategy::RoundRobin,
            &[target("openai/x*3"), target("kiro/y")],
        )
        .unwrap();
        let cfg = Config::load(Some(&path)).unwrap();
        let combo = cfg.resolve("a").unwrap();
        assert_eq!(combo.strategy, Strategy::RoundRobin);
        assert_eq!(combo.targets.len(), 2);
        assert_eq!(combo.targets[0].weight, 3);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("# mine"));

        remove(&path, "a").unwrap();
        assert!(Config::load(Some(&path)).unwrap().combos.is_empty());
        assert!(remove(&path, "a").is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
