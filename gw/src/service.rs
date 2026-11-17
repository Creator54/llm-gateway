//! Run `gw serve` as a systemd user service so it starts at login and restarts on failure.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const NAME: &str = "gw.service";

/// Where the user unit lives.
pub fn unit_path(home: &Path) -> PathBuf {
    home.join(".config/systemd/user").join(NAME)
}

/// Unit text for `exe serve`, passing `config` through when it is not the default.
pub fn unit(exe: &Path, config: Option<&Path>) -> String {
    let mut cmd = format!("{} serve", quote(exe));
    if let Some(config) = config {
        cmd.push_str(&format!(" --config {}", quote(config)));
    }
    format!(
        "[Unit]\n\
         Description=gw LLM gateway\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={cmd}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn quote(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// Write the unit if it differs. Returns whether it changed.
pub fn write(home: &Path, text: &str) -> anyhow::Result<bool> {
    let path = unit_path(home);
    if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
        return Ok(false);
    }
    std::fs::create_dir_all(path.parent().expect("unit path has a parent"))?;
    std::fs::write(&path, text)?;
    Ok(true)
}

/// Whether a systemd user manager is reachable.
pub fn available() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .output()
        .is_ok_and(|out| !out.stdout.is_empty())
}

/// `systemctl --user <args>`, failing with its stderr.
pub fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "systemctl --user {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The path to put in the unit: the `gw` the user ran, without resolving symlinks, so a
/// rebuild or upgrade behind `~/.local/bin/gw` keeps working.
pub fn exe() -> anyhow::Result<PathBuf> {
    let arg0 = PathBuf::from(std::env::args_os().next().unwrap_or_default());
    if arg0.components().count() > 1 {
        return Ok(std::path::absolute(arg0)?);
    }
    if let Some(found) = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(&arg0))
        .find(|p| p.is_file())
    {
        return Ok(found);
    }
    Ok(std::env::current_exe()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_runs_serve_and_passes_an_explicit_config() {
        let plain = unit(Path::new("/home/u/.local/bin/gw"), None);
        assert!(plain.contains("ExecStart=\"/home/u/.local/bin/gw\" serve\n"));
        assert!(plain.contains("Restart=on-failure"));
        assert!(plain.contains("WantedBy=default.target"));

        let custom = unit(
            Path::new("/bin/gw"),
            Some(Path::new("/etc/gw/my config.toml")),
        );
        assert!(
            custom.contains("ExecStart=\"/bin/gw\" serve --config \"/etc/gw/my config.toml\"\n")
        );
    }

    #[test]
    fn write_reports_changes_only() {
        let home = std::env::temp_dir().join(format!("gw-service-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert!(write(&home, "a").unwrap());
        assert!(!write(&home, "a").unwrap());
        assert!(write(&home, "b").unwrap());
        assert_eq!(std::fs::read_to_string(unit_path(&home)).unwrap(), "b");
        std::fs::remove_dir_all(&home).unwrap();
    }
}
