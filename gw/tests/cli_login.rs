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
fn restart_uses_the_systemd_user_service() {
    let dir = Temp::new("restart");
    let bin = dir.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let marker = dir.0.join("systemctl-args");
    let systemctl = bin.join("systemctl");
    std::fs::write(
        &systemctl,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            marker.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o700)).unwrap();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_gw"))
        .args(["restart"])
        .env("PATH", path)
        .env("HOME", dir.0.join("home"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        "--user\nrestart\ngw.service\n"
    );
}

#[test]
fn account_rename_updates_the_persisted_label() {
    let dir = Temp::new("rename");
    let state = dir.0.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let file = state.join("credentials.json");
    std::fs::write(
        &file,
        r#"{"version":1,"accounts":[{"provider":"openai","id":"account-1","label":"old","paused":false,"needs_reauth":false,"credential":{"type":"api_key","key":"sk-fake"}}]}"#,
    )
    .unwrap();
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();

    let out = gw(&dir.0, &["account", "rename", "openai", "old", "primary"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stored: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    assert_eq!(stored["accounts"][0]["label"], "primary");
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

#[test]
fn a_key_for_a_subscription_provider_points_at_its_api_provider() {
    let dir = Temp::new("keyhint");
    let out = gw(&dir.0, &["login", "openai", "--key=sk-x"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("gw login openai-api --key"), "{err}");
}

#[test]
fn status_flags_a_key_stored_under_a_subscription_provider() {
    let dir = Temp::new("keystatus");
    let state = dir.0.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let file = state.join("credentials.json");
    std::fs::write(
        &file,
        r#"{"version":1,"accounts":[{"provider":"openai","id":"k1","label":"old","paused":false,"needs_reauth":false,"credential":{"type":"api_key","key":"sk-fake"}}]}"#,
    )
    .unwrap();
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let out = gw(&dir.0, &["status"]);
    let text = stdout(&out);
    assert!(text.contains("unusable key: old (key)"), "{text}");
    assert!(
        text.contains("gw login openai-api --key, then gw account remove openai k1"),
        "{text}"
    );
}
