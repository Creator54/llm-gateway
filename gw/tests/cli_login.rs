//! CLI smoke tests: no network, no real credentials.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("gw-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run the binary with an isolated home, config and state dir.
fn gw(dir: &Path, args: &[&str]) -> std::process::Output {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let config = dir.join("config.toml");
    let state = dir.join("state");
    std::fs::write(&config, format!("state_dir = {:?}\n", state)).unwrap();
    Command::new(env!("CARGO_BIN_EXE_gw"))
        .args(args)
        .env("GW_CONFIG", &config)
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .env_remove("GW_THREADS")
        .output()
        .unwrap()
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn login_help_lists_providers() {
    let dir = Temp::new("help");
    let out = gw(&dir.0, &["login", "--help"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    for provider in [
        "anthropic",
        "openai",
        "openai-api",
        "antigravity",
        "kiro",
        "command-code",
    ] {
        assert!(
            text.contains(provider),
            "help is missing {provider}: {text}"
        );
    }
}

#[test]
fn import_dry_run_on_empty_dir_is_json() {
    let dir = Temp::new("import");
    let empty = dir.0.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let out = gw(
        &dir.0,
        &[
            "account",
            "import",
            "--from",
            empty.to_str().unwrap(),
            "--dry-run",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(value, serde_json::json!([]));
    assert!(!dir.0.join("state").join("credentials.json").exists());
}

#[test]
fn empty_store_lists_empty_json() {
    let dir = Temp::new("list");
    let out = gw(&dir.0, &["account", "list", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert_eq!(text.trim(), "[]");
}
