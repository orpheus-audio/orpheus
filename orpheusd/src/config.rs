//! `~/.orpheus/config.toml`. The file is optional; every field has a default.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Announced name; defaults to the hostname.
    pub name: Option<String>,
    pub api: ApiConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    /// Address the HTTP API (NMOS + /x-orpheus) listens on.
    pub listen: SocketAddr,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 7480)),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The configured name, or the hostname if none is set.
    pub fn name(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|s| s.trim().to_owned())
                .unwrap_or_else(|_| "orpheus".to_owned())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let c = Config::load(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(c.api.listen.port(), 7480);
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let c: Config = toml::from_str("name = \"Kitchen\"").unwrap();
        assert_eq!(c.name(), "Kitchen");
        assert_eq!(c.api.listen.port(), 7480);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("nmae = \"typo\"").is_err());
    }
}
