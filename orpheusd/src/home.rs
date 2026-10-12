//! Where the node keeps its files, following the XDG base directories.
//!
//! ```text
//! ~/.config/orpheus/config.toml   optional, hand-edited
//! ~/.local/state/orpheus/
//! ├── id                          device ID, created on first start
//! └── …                           peers, aliases, routes, gains (later)
//! ~/.cache/orpheus/               anything that can be rebuilt (later)
//! ```
//!
//! Each directory is chosen by the first of these that is set:
//!
//! | | config | state | cache |
//! |---|---|---|---|
//! | 1 | | `ORPHEUS_STATE` | `ORPHEUS_CACHE` |
//! | 2 | `$ORPHEUS_HOME` | `$ORPHEUS_HOME/state` | `$ORPHEUS_HOME/cache` |
//! | 3 | `$CONFIGURATION_DIRECTORY` | `$STATE_DIRECTORY` | `$CACHE_DIRECTORY` |
//! | 4 | `$XDG_CONFIG_HOME/orpheus` | `$XDG_STATE_HOME/orpheus` | `$XDG_CACHE_HOME/orpheus` |
//! | 5 | `~/.config/orpheus` | `~/.local/state/orpheus` | `~/.cache/orpheus` |
//!
//! Row 3 is set by systemd for units with `ConfigurationDirectory=orpheus`
//! (and `StateDirectory=`, `CacheDirectory=`): `/etc/orpheus`,
//! `/var/lib/orpheus` and `/var/cache/orpheus` for a system service.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use orpheus_core::DeviceId;

pub struct Home {
    config: PathBuf,
    state: PathBuf,
    cache: PathBuf,
}

impl Home {
    /// Resolves the directories from the environment.
    pub fn from_env() -> Result<Self> {
        Self::resolve(|name| std::env::var_os(name).map(PathBuf::from))
    }

    fn resolve(env: impl Fn(&str) -> Option<PathBuf>) -> Result<Self> {
        // Empty and relative values count as unset, as the XDG spec asks.
        let var = |name: &str| env(name).filter(|p| p.is_absolute());
        // systemd passes a colon-separated list if a unit names several.
        let systemd = |name: &str| {
            let list = var(name)?;
            let first = list.to_str()?.split(':').next()?.to_owned();
            Some(PathBuf::from(first))
        };
        let home = var("ORPHEUS_HOME");
        let user = var("HOME");
        let xdg = |name: &str, fallback: &str| {
            var(name)
                .or_else(|| user.as_ref().map(|h| h.join(fallback)))
                .map(|dir| dir.join("orpheus"))
        };
        let pick = |own: Option<PathBuf>, sub: Option<&str>, unit: &str, base: &str, fallback| {
            own.or_else(|| {
                home.as_ref()
                    .map(|h| sub.map_or_else(|| h.clone(), |s| h.join(s)))
            })
            .or_else(|| systemd(unit))
            .or_else(|| xdg(base, fallback))
            .ok_or_else(|| anyhow!("none of ORPHEUS_HOME, {base} or HOME is set"))
        };
        Ok(Self {
            config: pick(
                None,
                None,
                "CONFIGURATION_DIRECTORY",
                "XDG_CONFIG_HOME",
                ".config",
            )?,
            state: pick(
                var("ORPHEUS_STATE"),
                Some("state"),
                "STATE_DIRECTORY",
                "XDG_STATE_HOME",
                ".local/state",
            )?,
            cache: pick(
                var("ORPHEUS_CACHE"),
                Some("cache"),
                "CACHE_DIRECTORY",
                "XDG_CACHE_HOME",
                ".cache",
            )?,
        })
    }

    #[cfg(test)]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            config: root.clone(),
            state: root.join("state"),
            cache: root.join("cache"),
        }
    }

    pub fn config_path(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    pub fn state_dir(&self) -> &Path {
        &self.state
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    /// Reads the device ID, generating and saving a new one on first start.
    pub fn load_or_create_id(&self) -> Result<DeviceId> {
        let path = self.state.join("id");
        match fs::read_to_string(&path) {
            Ok(s) => s
                .trim()
                .parse()
                .with_context(|| format!("corrupt device ID in {}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let id =
                    DeviceId::generate().map_err(|e| anyhow!("reading OS random source: {e}"))?;
                fs::create_dir_all(&self.state)
                    .with_context(|| format!("creating {}", self.state.display()))?;
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

    fn resolve(vars: &[(&str, &str)]) -> Home {
        Home::resolve(|name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| PathBuf::from(v))
        })
        .unwrap()
    }

    fn dirs(h: &Home) -> [&str; 3] {
        [&h.config, &h.state, &h.cache].map(|p| p.to_str().unwrap())
    }

    #[test]
    fn xdg_defaults() {
        let h = resolve(&[("HOME", "/home/a")]);
        assert_eq!(
            dirs(&h),
            [
                "/home/a/.config/orpheus",
                "/home/a/.local/state/orpheus",
                "/home/a/.cache/orpheus"
            ]
        );
        assert_eq!(
            h.config_path(),
            Path::new("/home/a/.config/orpheus/config.toml")
        );
        let h = resolve(&[
            ("HOME", "/home/a"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_STATE_HOME", "relative/is/ignored"),
            ("XDG_CACHE_HOME", ""),
        ]);
        assert_eq!(
            dirs(&h),
            [
                "/cfg/orpheus",
                "/home/a/.local/state/orpheus",
                "/home/a/.cache/orpheus"
            ]
        );
    }

    #[test]
    fn systemd_directories() {
        let h = resolve(&[
            ("HOME", "/root"),
            ("XDG_STATE_HOME", "/xdg"),
            ("CONFIGURATION_DIRECTORY", "/etc/orpheus"),
            ("STATE_DIRECTORY", "/var/lib/orpheus:/var/lib/other"),
            ("CACHE_DIRECTORY", "/var/cache/orpheus"),
        ]);
        assert_eq!(
            dirs(&h),
            ["/etc/orpheus", "/var/lib/orpheus", "/var/cache/orpheus"]
        );
    }

    #[test]
    fn orpheus_variables_win() {
        let systemd = [
            ("STATE_DIRECTORY", "/var/lib/orpheus"),
            ("CONFIGURATION_DIRECTORY", "/etc/orpheus"),
        ];
        let mut vars = vec![("HOME", "/home/a"), ("ORPHEUS_HOME", "/o")];
        vars.extend(systemd);
        assert_eq!(dirs(&resolve(&vars)), ["/o", "/o/state", "/o/cache"]);
        vars.extend([("ORPHEUS_STATE", "/s"), ("ORPHEUS_CACHE", "/c")]);
        assert_eq!(dirs(&resolve(&vars)), ["/o", "/s", "/c"]);
    }

    #[test]
    fn no_home_at_all() {
        assert!(Home::resolve(|_| None).is_err());
    }

    #[test]
    fn id_is_created_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join("orpheus"));
        let a = home.load_or_create_id().unwrap();
        let b = home.load_or_create_id().unwrap();
        assert_eq!(a, b);
        assert!(home.state_dir().join("id").exists());
        assert!(!home.state_dir().join("id.tmp").exists());
    }

    #[test]
    fn corrupt_id_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        fs::create_dir_all(home.state_dir()).unwrap();
        fs::write(home.state_dir().join("id"), "not an id\n").unwrap();
        assert!(home.load_or_create_id().is_err());
    }
}
