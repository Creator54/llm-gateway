use crate::{cache, combo::Combo, tidy};
/// Serde default that keeps a switch on unless the config says otherwise.
fn yes() -> bool {
    true
}

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
    /// Provider a bare model id (no `provider/` prefix) routes to; `None` forwards it.
    pub default_provider: Option<String>,
    pub state_dir: PathBuf,
    pub max_body_bytes: usize,
    /// Combos by name, selected with `combo/<name>`.
    pub combos: BTreeMap<String, Combo>,
    /// Models to list per provider, beyond the combo targets (`provider = [\"model\", …]`).
    pub models: BTreeMap<String, Vec<String>>,
    /// Per-provider endpoint overrides, e.g. a fake upstream in tests.
    #[serde(default)]
    pub endpoints: BTreeMap<String, String>,
    /// `default` runs the token-efficiency edits; `parity` turns every one off.
    pub profile: Profile,
    #[serde(default)]
    pub transforms: Transforms,
    /// Serve `GET /metrics` on the same port; still behind the gateway key.
    #[serde(default = "yes")]
    pub metrics: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: ([127, 0, 0, 1], 10204).into(),
            api_key_file: None,
            fallback: None,
            default_provider: Some("openai".to_owned()),
            state_dir: home().join(".config/gw"),
            max_body_bytes: 32 << 20,
            combos: BTreeMap::new(),
            models: BTreeMap::new(),
            endpoints: BTreeMap::new(),
            profile: Profile::Default,
            metrics: true,
            transforms: Transforms::default(),
        }
    }
}

/// Behaviour profile: `parity` turns off every optional transform (cache edits, tidy),
/// so a passthrough forwards the client's bytes. Adapters still apply the wire edits
/// their upstream requires, such as the bare model id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    #[default]
    Default,
    Parity,
}

/// One switch per edit, each falling back to the module default when unset.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Transforms {
    /// Run the cache edits as a group.
    pub cache: Option<bool>,
    /// Run tool-result cleanup.
    pub tidy: Option<bool>,
    pub markers: Option<bool>,
    pub ttl_order: Option<bool>,
    pub ttl_1h: Option<bool>,
    pub strip_volatile: Option<bool>,
    pub openai_cache_key: Option<bool>,
    pub strip_ansi: Option<bool>,
    pub collapse_redraws: Option<bool>,
    pub collapse_repeats: Option<bool>,
    pub trim_trailing: Option<bool>,
    pub max_lines: Option<usize>,
}

impl Config {
    /// Load from `path`, `$GW_CONFIG`, or `~/.config/gw/config.toml`. A missing default file means defaults.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let explicit = Self::explicit_path(path);
        let file = Self::path(path);
        match std::fs::read_to_string(&file) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => {
                Ok(Self::default())
            }
            Err(e) => Err(anyhow::anyhow!("{}: {e}", file.display())),
        }
    }

    fn explicit_path(path: Option<&Path>) -> Option<PathBuf> {
        path.map(PathBuf::from)
            .or_else(|| std::env::var_os("GW_CONFIG").map(PathBuf::from))
    }

    /// The config file in use: `path`, `$GW_CONFIG`, or the default location.
    pub fn path(path: Option<&Path>) -> PathBuf {
        Self::explicit_path(path).unwrap_or_else(|| home().join(".config/gw/config.toml"))
    }

    /// Cache edits in force for this profile.
    pub fn cache_options(&self) -> cache::Options {
        let defaults = cache::Options::default();
        let t = &self.transforms;
        if self.profile == Profile::Parity || t.cache == Some(false) {
            return cache::Options::parity();
        }
        cache::Options {
            markers: t.markers.unwrap_or(defaults.markers),
            ttl_order: t.ttl_order.unwrap_or(defaults.ttl_order),
            ttl_1h: t.ttl_1h.unwrap_or(defaults.ttl_1h),
            strip_volatile: t.strip_volatile.unwrap_or(defaults.strip_volatile),
            openai_cache_key: t.openai_cache_key.unwrap_or(defaults.openai_cache_key),
        }
    }

    /// Tool-result cleanup in force for this profile.
    pub fn tidy_options(&self) -> tidy::Options {
        let defaults = tidy::Options::default();
        let t = &self.transforms;
        if self.profile == Profile::Parity || t.tidy == Some(false) {
            return tidy::Options::parity();
        }
        tidy::Options {
            strip_ansi: t.strip_ansi.unwrap_or(defaults.strip_ansi),
            collapse_redraws: t.collapse_redraws.unwrap_or(defaults.collapse_redraws),
            collapse_repeats: t.collapse_repeats.unwrap_or(defaults.collapse_repeats),
            trim_trailing: t.trim_trailing.unwrap_or(defaults.trim_trailing),
            max_lines: t.max_lines.or(defaults.max_lines),
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
    fn bare_models_default_to_openai() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("openai"));
        let cfg: Config = toml::from_str("default_provider = \"opencode\"").unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("opencode"));
    }

    #[test]
    fn unknown_combo_is_an_error() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.resolve("nope").is_err());
    }
}

#[cfg(test)]
mod models_tests {
    use super::*;

    #[test]
    fn parses_per_provider_model_lists() {
        let cfg: Config = toml::from_str("[models]\nanthropic = [\"claude-haiku-5-5\"]\n").unwrap();
        assert_eq!(cfg.models["anthropic"], ["claude-haiku-5-5"]);
    }
}
