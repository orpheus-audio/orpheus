//! Settings that can be changed while the node runs, over the API.
//!
//! Each value comes from one of three layers: the built-in default,
//! `config.toml`, or a change made over the API. API changes are kept in
//! `settings.json` in the state directory and win over the file, which the
//! node never writes. Resetting a value over the API goes back to the file
//! or the default.
//!
//! Changes are JSON merge patches (RFC 7396) on the overrides: a value sets
//! an override, `null` removes it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use orpheus_clock::ptp::{PtpConfig, PtpTransport};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa::ToSchema;

use crate::config::{Accept, Claim, Config, Policy, WhenLost};
use crate::i18n::Text;

/// The settings in effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub ptp: PtpSettings,
    pub receiver: ReceiverSettings,
    pub sender: SenderSettings,
    pub ui: UiSettings,
}

/// Clock synchronization. A change restarts PTP; the clock keeps running.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PtpSettings {
    /// Network interface; `null` for the one with the default route.
    pub interface: Option<String>,
    /// `ipv4`, `ipv6`, or `auto`: IPv4 if the interface has an IPv4
    /// address.
    #[schema(value_type = String, example = "auto")]
    pub transport: PtpTransport,
    pub domain: u8,
    /// Grandmaster election: lower wins.
    pub priority1: u8,
    pub priority2: u8,
    /// Never become grandmaster.
    pub slave_only: bool,
    /// Clock offsets above this are stepped, smaller ones slewed.
    pub step_threshold_us: u32,
    /// Offset from the grandmaster under which the clock counts as settled.
    pub settle_threshold_us: u32,
}

/// The receivers. A change of `link_offset_ms` restarts running streams.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReceiverSettings {
    /// What a free receiver plays.
    pub accept: Accept,
    /// Which streams are heard when several play at once.
    pub policy: Policy,
    /// How far normal streams are turned down under a high-priority one.
    pub duck_db: f32,
    /// Playout delay after the sender's timestamp.
    pub link_offset_ms: u32,
    /// Where streams play that have no output remembered; `null` for the
    /// audio system's default output.
    pub default_output: Option<String>,
    /// What happens to a stream whose output goes away: `pause`, `park`
    /// or `default`.
    pub when_lost: WhenLost,
    /// How long an output may be gone before it counts as lost.
    pub lost_after_ms: u32,
}

/// The routes this node sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SenderSettings {
    /// When a route holds its receiver on the peer: `always`, or
    /// `on_play`, only while something plays into its sink.
    pub claim: Claim,
    /// With `on_play`: how long a sink stays silent before the peer's
    /// receiver is let go.
    pub release_after_s: u32,
    /// How long sending fades in when it starts.
    pub fade_in_ms: u32,
}

/// What Orpheus shows in the audio system. A change shows from the next
/// second on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UiSettings {
    /// The language of what Orpheus shows, such as `de`; `auto` for the
    /// environment's (`LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG`).
    pub language: String,
}

/// The longest fade-in.
const MAX_FADE_IN_MS: u32 = 1000;

/// The clock's configuration for these settings.
pub fn ptp_config(p: &PtpSettings) -> PtpConfig {
    PtpConfig {
        interface: p.interface.clone(),
        transport: p.transport,
        domain: p.domain,
        priority1: p.priority1,
        priority2: p.priority2,
        slave_only: p.slave_only,
        step_threshold_us: p.step_threshold_us,
        settle_threshold_us: p.settle_threshold_us,
    }
}

/// Where a setting's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Default,
    /// `config.toml`.
    Config,
    /// Changed over the API.
    Api,
}

/// The longest an output may be gone before it counts as lost.
const MAX_LOST_AFTER_MS: u32 = 60_000;

/// The longest link offset; the jitter buffer holds twice as much.
const MAX_LINK_OFFSET_MS: u32 = 1000;

impl Settings {
    /// The values from `config.toml`, with the defaults for what it leaves
    /// out.
    pub fn from_config(config: &Config) -> Self {
        let p = &config.ptp;
        let r = &config.receiver;
        Self {
            ptp: PtpSettings {
                interface: p.interface.clone(),
                transport: p.transport,
                domain: p.domain,
                priority1: p.priority1,
                priority2: p.priority2,
                slave_only: p.slave_only,
                step_threshold_us: p.step_threshold_us,
                settle_threshold_us: p.settle_threshold_us,
            },
            receiver: ReceiverSettings {
                accept: r.accept,
                policy: r.policy,
                duck_db: r.duck_db,
                link_offset_ms: r.link_offset_ms,
                default_output: r.default_output.clone(),
                when_lost: r.when_lost,
                lost_after_ms: r.lost_after_ms,
            },
            sender: SenderSettings {
                claim: config.sender.claim,
                release_after_s: config.sender.release_after_s,
                fade_in_ms: config.sender.fade_in_ms,
            },
            ui: UiSettings {
                language: config.ui.language.clone(),
            },
        }
    }

    /// What the types can't express.
    pub fn check(&self) -> Result<(), Text> {
        let (p, r) = (&self.ptp, &self.receiver);
        if p.interface.as_deref() == Some("") {
            return Err(Text::new("setting-empty").arg("key", "[ptp] interface"));
        }
        if p.step_threshold_us == 0 || p.settle_threshold_us == 0 {
            return Err(Text::new("setting-thresholds"));
        }
        if r.default_output.as_deref() == Some("") {
            return Err(Text::new("setting-empty").arg("key", "[receiver] default_output"));
        }
        if !(0.0..=96.0).contains(&r.duck_db) {
            return Err(range("[receiver] duck_db", 0, 96));
        }
        if !(1..=MAX_LINK_OFFSET_MS).contains(&r.link_offset_ms) {
            return Err(range("[receiver] link_offset_ms", 1, MAX_LINK_OFFSET_MS));
        }
        if r.lost_after_ms > MAX_LOST_AFTER_MS {
            return Err(range("[receiver] lost_after_ms", 0, MAX_LOST_AFTER_MS));
        }
        if self.sender.release_after_s == 0 {
            return Err(Text::new("setting-above-zero").arg("key", "[sender] release_after_s"));
        }
        if self.sender.fade_in_ms > MAX_FADE_IN_MS {
            return Err(range("[sender] fade_in_ms", 0, MAX_FADE_IN_MS));
        }
        if !crate::i18n::valid(&self.ui.language) {
            return Err(Text::new("setting-language").arg("language", &self.ui.language));
        }
        Ok(())
    }
}

/// `key` must be `min` to `max`.
fn range(key: &'static str, min: u32, max: u32) -> Text {
    Text::new("setting-range")
        .arg("key", key)
        .arg("min", min)
        .arg("max", max)
}

/// The settings in effect and the layers they come from.
pub struct Store {
    /// Defaults and `config.toml`.
    base: Settings,
    /// The keys `config.toml` sets, as `section.key`.
    in_file: Vec<String>,
    /// The API's changes: sections of keys.
    overrides: Map<String, Value>,
    current: Settings,
    path: PathBuf,
}

/// A change checked but not yet made; see [`Store::commit`].
pub struct Change {
    overrides: Map<String, Value>,
    pub settings: Settings,
}

impl Store {
    /// Takes the base from `config` and the overrides from `path`; invalid
    /// or unreadable overrides are dropped with a warning.
    pub fn load(config: &Config, path: PathBuf) -> Self {
        let base = Settings::from_config(config);
        let mut store = Self {
            current: base.clone(),
            base,
            in_file: config.keys.clone(),
            overrides: Map::new(),
            path,
        };
        let overrides = match std::fs::read(&store.path) {
            Ok(data) => serde_json::from_slice::<Map<String, Value>>(&data)
                .map_err(|e| Text::new("settings-invalid").arg("error", e.to_string())),
            Err(_) => return store,
        };
        match overrides.and_then(|o| store.resolve(o)) {
            Ok(change) => {
                store.overrides = change.overrides;
                store.current = change.settings;
            }
            Err(e) => {
                let error = crate::i18n::texts("en").text(&e);
                tracing::warn!(%error, path = %store.path.display(), "ignoring settings");
            }
        }
        store
    }

    pub fn get(&self) -> &Settings {
        &self.current
    }

    /// Where each value comes from, by section and key.
    pub fn sources(&self) -> BTreeMap<String, BTreeMap<String, Source>> {
        let Value::Object(sections) =
            serde_json::to_value(&self.current).expect("settings serialize")
        else {
            unreachable!("settings are an object");
        };
        sections
            .into_iter()
            .map(|(section, keys)| {
                let Value::Object(keys) = keys else {
                    unreachable!("sections are objects");
                };
                let sources = keys
                    .keys()
                    .map(|key| {
                        let overridden = self
                            .overrides
                            .get(&section)
                            .and_then(Value::as_object)
                            .is_some_and(|o| o.contains_key(key));
                        let source = if overridden {
                            Source::Api
                        } else if self.in_file.contains(&format!("{section}.{key}")) {
                            Source::Config
                        } else {
                            Source::Default
                        };
                        (key.clone(), source)
                    })
                    .collect();
                (section, sources)
            })
            .collect()
    }

    /// Checks a merge patch on the overrides and returns the settings it
    /// would give.
    pub fn patch(&self, patch: &Value) -> Result<Change, Text> {
        let mut overrides = Value::Object(self.overrides.clone());
        merge(&mut overrides, patch);
        let Value::Object(overrides) = overrides else {
            return Err(Text::new("settings-not-object"));
        };
        self.resolve(overrides)
    }

    /// Makes a change from [`Store::patch`] and saves it.
    pub fn commit(&mut self, change: Change) {
        self.overrides = change.overrides;
        self.current = change.settings;
        let json = serde_json::to_vec_pretty(&self.overrides).expect("settings serialize");
        if let Err(e) = crate::home::write_atomic(&self.path, &json) {
            tracing::warn!(error = %e, path = %self.path.display(), "saving settings");
        }
    }

    /// The settings `overrides` give on top of the base.
    fn resolve(&self, overrides: Map<String, Value>) -> Result<Change, Text> {
        let mut value = serde_json::to_value(&self.base).expect("settings serialize");
        for (section, keys) in &overrides {
            if !keys.is_object() {
                return Err(Text::new("settings-section-not-object").arg("section", section));
            }
            match value.get_mut(section) {
                Some(base) => merge(base, keys),
                None => return Err(Text::new("settings-unknown-section").arg("section", section)),
            }
        }
        let settings: Settings = serde_json::from_value(value)
            .map_err(|e| Text::new("settings-invalid").arg("error", e.to_string()))?;
        settings.check()?;
        // Sections emptied by the patch are dropped.
        let overrides = overrides
            .into_iter()
            .filter(|(_, keys)| keys.as_object().is_some_and(|k| !k.is_empty()))
            .collect();
        Ok(Change {
            overrides,
            settings,
        })
    }
}

/// Applies a JSON merge patch (RFC 7396) to `target`.
fn merge(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let target = target.as_object_mut().expect("just made an object");
    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            merge(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn store(toml: &str) -> Store {
        let mut config: Config = toml::from_str(toml).unwrap();
        config.keys = crate::config::keys(toml);
        let path = std::env::temp_dir().join(format!(
            "orpheus-settings-test-{}-{}.json",
            std::process::id(),
            rand_suffix()
        ));
        Store::load(&config, path)
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn layers_and_sources() {
        let mut s = store("[ptp]\ndomain = 3\n[receiver]\npolicy = \"mix\"");
        assert_eq!(s.get().ptp.domain, 3);
        assert_eq!(s.sources()["ptp"]["domain"], Source::Config);
        assert_eq!(s.sources()["ptp"]["priority1"], Source::Default);

        let change = s.patch(&json!({"ptp": {"domain": 5}})).unwrap();
        assert_eq!(change.settings.ptp.domain, 5);
        s.commit(change);
        assert_eq!(s.sources()["ptp"]["domain"], Source::Api);
        assert_eq!(s.get().receiver.policy, Policy::Mix);

        // null goes back to the file.
        let change = s.patch(&json!({"ptp": {"domain": null}})).unwrap();
        s.commit(change);
        assert_eq!(s.get().ptp.domain, 3);
        assert_eq!(s.sources()["ptp"]["domain"], Source::Config);
        assert!(s.overrides.is_empty());

        // Overrides survive a restart.
        s.commit(s.patch(&json!({"receiver": {"policy": "first"}})).unwrap());
        let mut config: Config = toml::from_str("").unwrap();
        config.keys = Vec::new();
        let again = Store::load(&config, s.path.clone());
        assert_eq!(again.get().receiver.policy, Policy::First);
        let _ = std::fs::remove_file(&s.path);
    }

    #[test]
    fn bad_changes_are_refused() {
        let s = store("");
        assert!(s.patch(&json!({"ptp": {"domian": 1}})).is_err());
        assert!(s.patch(&json!({"clock": {}})).is_err());
        assert!(s.patch(&json!({"ptp": 1})).is_err());
        assert!(s.patch(&json!({"ptp": {"domain": "one"}})).is_err());
        assert!(s.patch(&json!({"receiver": {"duck_db": 120}})).is_err());
        assert!(
            s.patch(&json!({"receiver": {"link_offset_ms": 0}}))
                .is_err()
        );
        assert!(s.patch(&json!({"sender": {"release_after_s": 0}})).is_err());
        assert!(
            s.patch(&json!({"receiver": {"when_lost": "move"}}))
                .is_err()
        );
        assert!(
            s.patch(&json!({"receiver": {"lost_after_ms": 600_000}}))
                .is_err()
        );
        assert!(s.patch(&json!({"sender": {"claim": "never"}})).is_err());
        assert!(s.patch(&json!([])).is_err());
        assert!(s.patch(&json!({"ui": {"language": "de_"}})).is_err());
        assert!(s.patch(&json!({"ui": {"language": "de-AT"}})).is_ok());
        assert!(s.patch(&json!({"ui": {"language": "auto"}})).is_ok());
    }

    #[test]
    fn interface_can_be_set_and_reset() {
        let mut s = store("[ptp]\ninterface = \"eth0\"");
        s.commit(s.patch(&json!({"ptp": {"interface": "wlan0"}})).unwrap());
        assert_eq!(s.get().ptp.interface.as_deref(), Some("wlan0"));
        s.commit(s.patch(&json!({"ptp": {"interface": null}})).unwrap());
        assert_eq!(s.get().ptp.interface.as_deref(), Some("eth0"));
    }
}
