//! Where the vault is.
//!
//! In order: an explicit path (a `--vault` flag), `$CCE_VAULT`, then the
//! shared cce config, `~/.config/cce/config.kdl`:
//!
//! ```kdl
//! vault {
//!     path "~/Dropbox/Apps/remotely-save/Vault 1"
//! }
//! ```
//!
//! The config is read with the `kdl` crate directly rather than through
//! cce-ui's loader, so that this crate stays free of the toolkit and a
//! shell tool or test can use it without a Wayland stack.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum ConfigError {
    /// No vault is configured anywhere.
    Unset(PathBuf),
    /// A vault is configured but is not a directory.
    Missing(PathBuf),
    Parse(PathBuf, String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Unset(cfg) => write!(
                f,
                "no vault configured: set CCE_VAULT, pass --vault, or add `vault {{ path \"…\" }}` to {}",
                cfg.display()
            ),
            ConfigError::Missing(p) => write!(f, "vault is not a directory: {}", p.display()),
            ConfigError::Parse(cfg, e) => write!(f, "{}: {e}", cfg.display()),
        }
    }
}

impl std::error::Error for ConfigError {}

pub fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"))
        .join("cce")
        .join("config.kdl")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if path == "~" => home(),
        None => PathBuf::from(path),
    }
}

/// The `vault` block's path from a config document, if it names one.
/// Accepts `vault { path "…" }` and the one-line `vault path="…"`.
pub fn path_from_kdl(text: &str) -> Result<Option<PathBuf>, String> {
    let doc: kdl::KdlDocument = text.parse().map_err(|e: kdl::KdlError| e.to_string())?;
    let Some(node) = doc.get("vault") else { return Ok(None) };
    let from_child = node.children().and_then(|c| c.get_arg("path")).and_then(|v| v.as_string());
    let from_prop = node.get("path").and_then(|e| e.value().as_string());
    Ok(from_child.or(from_prop).map(expand))
}

pub fn vault_root(explicit: Option<&Path>) -> Result<PathBuf, ConfigError> {
    let cfg = config_path();
    let chosen = if let Some(p) = explicit {
        p.to_path_buf()
    } else if let Some(p) = std::env::var_os("CCE_VAULT").filter(|v| !v.is_empty()) {
        expand(&p.to_string_lossy())
    } else {
        let text = match std::fs::read_to_string(&cfg) {
            Ok(t) => t,
            Err(_) => return Err(ConfigError::Unset(cfg)),
        };
        match path_from_kdl(&text) {
            Ok(Some(p)) => p,
            Ok(None) => return Err(ConfigError::Unset(cfg)),
            Err(e) => return Err(ConfigError::Parse(cfg, e)),
        }
    };
    if chosen.is_dir() {
        Ok(chosen)
    } else {
        Err(ConfigError::Missing(chosen))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kdl_forms() {
        let home = home();
        assert_eq!(
            path_from_kdl("layout { gap 4; }\nvault {\n    path \"~/Notes\"\n}\n").unwrap(),
            Some(home.join("Notes"))
        );
        assert_eq!(path_from_kdl("vault path=\"/v\"").unwrap(), Some(PathBuf::from("/v")));
        assert_eq!(path_from_kdl("other 1").unwrap(), None);
        assert!(path_from_kdl("vault {").is_err());
    }
}
