//! The node's home directory, `~/.orpheus` unless `ORPHEUS_HOME` says otherwise.
//!
//! ```text
//! ~/.orpheus/
//! ├── config.toml   optional, hand-edited
//! ├── id            device ID, created on first start
//! └── state/        peers, aliases, routes, gains (written by the daemon)
//! ```

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use orpheus_core::DeviceId;

pub struct Home {
    root: PathBuf,
}

impl Home {
    /// Resolves the home directory from `ORPHEUS_HOME` or `$HOME/.orpheus`.
    pub fn from_env() -> Result<Self> {
        let root = match std::env::var_os("ORPHEUS_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => {
                let home =
                    std::env::var_os("HOME").context("neither ORPHEUS_HOME nor HOME is set")?;
                PathBuf::from(home).join(".orpheus")
            }
        };
        Ok(Self::new(root))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// Reads the device ID, generating and saving a new one on first start.
    pub fn load_or_create_id(&self) -> Result<DeviceId> {
        let path = self.root.join("id");
        match fs::read_to_string(&path) {
            Ok(s) => s
                .trim()
                .parse()
                .with_context(|| format!("corrupt device ID in {}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let id = DeviceId::generate()
                    .map_err(|e| anyhow::anyhow!("reading OS random source: {e}"))?;
                fs::create_dir_all(&self.root)
                    .with_context(|| format!("creating {}", self.root.display()))?;
                write_atomic(&path, format!("{id}\n").as_bytes())
                    .with_context(|| format!("writing {}", path.display()))?;
                tracing::info!(%id, "generated new device ID");
                Ok(id)
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

/// Writes via a temporary file and rename, so a crash never leaves a partial file.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_created_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join(".orpheus"));
        let a = home.load_or_create_id().unwrap();
        let b = home.load_or_create_id().unwrap();
        assert_eq!(a, b);
        assert!(!home.root().join("id.tmp").exists());
    }

    #[test]
    fn corrupt_id_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("id"), "not an id\n").unwrap();
        assert!(Home::new(dir.path()).load_or_create_id().is_err());
    }
}
