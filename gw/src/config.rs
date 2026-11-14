use serde::Deserialize;
use std::{
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
    /// Where credentials and state live.
    pub state_dir: PathBuf,
    /// Largest request body accepted, in bytes.
    pub max_body_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: ([127, 0, 0, 1], 10204).into(),
            api_key_file: None,
            fallback: None,
            state_dir: home().join(".config/gw"),
            max_body_bytes: 32 << 20,
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
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
