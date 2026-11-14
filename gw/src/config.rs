use crate::combo::Combo;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Address to listen on. Non-loopback requires `api_key_file`.
    pub listen: SocketAddr,
    /// File holding the key clients must send (Bearer or `x-gw-key`).
    pub api_key_file: Option<PathBuf>,
    /// Upstream used for anything not handled natively (e.g. opencodex).
    pub fallback: Option<String>,
    pub state_dir: PathBuf,
    pub max_body_bytes: usize,
    /// Combos by name, selected with `combo/<name>`.
    pub combos: BTreeMap<String, Combo>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: ([127, 0, 0, 1], 10204).into(),
            api_key_file: None,
            fallback: None,
            state_dir: home().join(".config/gw"),
            max_body_bytes: 32 << 20,
            combos: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Load from `path`, `$GW_CONFIG`, or `~/.config/gw/config.toml`. A missing default file means defaults.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let explicit = path
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("GW_CONFIG").map(PathBuf::from));
        let file = explicit
            .clone()
            .unwrap_or_else(|| home().join(".config/gw/config.toml"));
        match std::fs::read_to_string(&file) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => {
                Ok(Self::default())
            }
            Err(e) => Err(anyhow::anyhow!("{}: {e}", file.display())),
        }
    }

    /// The combo called `name`; unknown names are an error.
    pub fn resolve(&self, name: &str) -> anyhow::Result<Combo> {
        let combo = self
            .combos
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown combo: {name}"))?;
        Ok(Combo {
            name: name.to_owned(),
            ..combo.clone()
        })
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combo::Strategy;

    #[test]
    fn parses_a_named_combo() {
        let text = r#"
[combos.pi-subagents]
strategy = "failover"
sticky_limit = 3

[[combos.pi-subagents.targets]]
provider = "command-code"
model = "deepseek/deepseek-v4.1-flash"

[[combos.pi-subagents.targets]]
provider = "openai"
model = "gpt-6-luna"
weight = 2
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        let combo = cfg.resolve("pi-subagents").unwrap();
        assert_eq!(combo.name, "pi-subagents");
        assert_eq!(combo.strategy, Strategy::Failover);
        assert_eq!(combo.sticky_limit, 3);
        assert_eq!(combo.targets.len(), 2);
        assert_eq!(combo.targets[1].provider, "openai");
        assert_eq!(combo.targets[1].weight, 2);
        // An absent weight is zero and counts as one.
        assert_eq!(combo.targets[0].weight, 0);
    }

    #[test]
    fn unknown_combo_is_an_error() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.resolve("nope").is_err());
    }
}
