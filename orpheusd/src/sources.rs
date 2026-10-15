//! What the receivers remember of each source they played: the output it
//! plays on, its volume and, if the user set one, the layout of its
//! channels.
//!
//! A source is recognized by what its stream announces (see
//! [`key`](crate::connection)): the sender's NMOS source ID, else its SDP
//! session, else the address it comes from. The memory is kept in
//! `sources.json` in the state directory, under a short ID of its own, and
//! independent of the routes and pins of the sending node; it goes after
//! the `[sources]` limits or when forgotten.
//!
//! The audio system restores nothing for these streams. Every [`TICK`] this
//! module notes where the user moved each stream and how loud it plays, and
//! puts it on the output it should play on: the one remembered, else
//! `[receiver] default_output`, else the audio system's default, unless
//! that is a sink to another node. A stream from another node is never
//! passed on to one by default; it waits, unlinked, for an output.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orpheus_core::{Layout, SourceId};
use orpheus_engine::{Output, Placement, ReceiverId, Volume};
use serde::{Deserialize, Serialize};

use crate::api::Node;
use crate::config::SourcesConfig;
use crate::outputs::sink_sender;

/// How often the streams are checked.
const TICK: Duration = Duration::from_secs(1);

/// A source the receivers remember.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: SourceId,
    /// What the stream is recognized by: `orpheus.source.<NMOS source
    /// ID>`, `orpheus.session.<SDP origin>` or `orpheus.from.<address>`.
    pub key: String,
    /// The name its stream had when it last played.
    pub name: String,
    /// The sending Orpheus node's device ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The output the user chose for it, by machine name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<Volume>,
    /// What its channels carry, as the user set it over what its stream
    /// says: for senders that say nothing, or say it wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    /// When it last played, in seconds since the Unix epoch.
    pub used: u64,
}

/// The remembered sources and where the running streams are placed.
pub struct Sources {
    path: PathBuf,
    limits: SourcesConfig,
    list: Vec<Source>,
    /// Where each running receiver's stream was put last.
    placed: BTreeMap<ReceiverId, Placement>,
    /// The keys playing at the last tick.
    playing: BTreeSet<String>,
}

impl Sources {
    /// Reads `path`; missing or unreadable means nothing remembered.
    pub fn load(path: PathBuf, limits: SourcesConfig) -> Self {
        let list = match std::fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|e| {
                tracing::warn!(error = %e, path = %path.display(), "ignoring saved sources");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        Self {
            path,
            limits,
            list,
            placed: BTreeMap::new(),
            playing: BTreeSet::new(),
        }
    }

    fn save(&self) {
        let json = serde_json::to_vec_pretty(&self.list).expect("sources serialize");
        if let Err(e) = crate::home::write_atomic(&self.path, &json) {
            tracing::warn!(error = %e, path = %self.path.display(), "saving sources");
        }
    }

    /// The remembered sources, most recently played first.
    pub fn list(&self) -> &[Source] {
        &self.list
    }

    pub fn get(&self, key: &str) -> Option<&Source> {
        self.list.iter().find(|s| s.key == key)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut Source> {
        self.list.iter_mut().find(|s| s.key == key)
    }

    /// Notes that a receiver starts to play `key`, remembering it if it is
    /// new; returns what is remembered.
    pub fn played(&mut self, key: &str, name: &str, node: Option<&str>) -> Source {
        let now = unix_now();
        let i = match self.list.iter().position(|s| s.key == key) {
            Some(i) => i,
            None => {
                let id = loop {
                    let id = SourceId::generate().expect("OS random source");
                    if self.list.iter().all(|s| s.id != id) {
                        break id;
                    }
                };
                self.list.push(Source {
                    id,
                    key: key.to_owned(),
                    name: String::new(),
                    node: None,
                    output: None,
                    volume: None,
                    layout: None,
                    used: now,
                });
                self.list.len() - 1
            }
        };
        let mut source = self.list.remove(i);
        source.name = name.to_owned();
        source.node = node.map(str::to_owned).or(source.node);
        source.used = now;
        self.list.insert(0, source.clone());
        self.save();
        source
    }

    /// Notes where receiver `id`'s stream is put.
    pub fn place(&mut self, id: ReceiverId, placement: Placement) {
        self.placed.insert(id, placement);
    }

    /// Whether receiver `id`'s stream waits, unlinked, for an output.
    pub fn held(&self, id: ReceiverId) -> bool {
        self.placed.get(&id) == Some(&Placement::Held)
    }

    /// Forgets a source's output and volume. One that plays stays listed,
    /// as new; others go.
    pub fn forget(&mut self, id: SourceId) -> Option<Source> {
        let i = self.list.iter().position(|s| s.id == id)?;
        let source = if self.playing.contains(&self.list[i].key) {
            let s = &mut self.list[i];
            let old = s.clone();
            s.output = None;
            s.volume = None;
            s.layout = None;
            old
        } else {
            self.list.remove(i)
        };
        self.save();
        Some(source)
    }

    /// Sets the layout of source `id`, `None` for what its stream says;
    /// `false` if there is no such source.
    pub fn set_layout(&mut self, id: SourceId, layout: Option<Layout>) -> bool {
        let Some(s) = self.list.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        if s.layout != layout {
            s.layout = layout;
            self.save();
        }
        true
    }

    /// Notes that the user chose `output` for `key` (`None`: the default).
    pub fn choose(&mut self, key: &str, output: Option<&str>) {
        if let Some(s) = self.get_mut(key)
            && s.output.as_deref() != output
        {
            s.output = output.map(str::to_owned);
            self.save();
        }
    }

    /// Forgets sources beyond the limits, oldest first; those playing and
    /// those from pinned nodes stay. Returns whether any went.
    fn evict(&mut self, pinned: &BTreeSet<String>) -> bool {
        let max_age = u64::from(self.limits.max_age_days) * 86_400;
        let max_entries = self.limits.max_entries as usize;
        let now = unix_now();
        let before = self.list.len();
        self.list.sort_by_key(|s| std::cmp::Reverse(s.used));
        let mut kept = 0;
        let playing = &self.playing;
        self.list.retain(|s| {
            if playing.contains(&s.key) || s.node.as_ref().is_some_and(|n| pinned.contains(n)) {
                return true;
            }
            kept += 1;
            kept <= max_entries && now.saturating_sub(s.used) <= max_age
        });
        self.list.len() < before
    }
}

/// Forgets source `id`'s output and volume; a stream playing it goes where
/// streams play by default, at full volume. Returns whether there was one.
pub fn forget(node: &Node, id: SourceId) -> bool {
    let default_output = node.settings().receiver.default_output;
    let engine = node.engine.as_ref().map(|e| e.lock().unwrap());
    let mut sources = node.sources.lock().unwrap();
    let Some(source) = sources.forget(id) else {
        return false;
    };
    tracing::info!(%id, name = %source.name, "source forgotten");
    let Some(engine) = engine else {
        return true;
    };
    let playing = engine
        .status()
        .receivers
        .iter()
        .map(|&(r, _)| r)
        .find(|&r| engine.receiver_key(r).flatten() == Some(source.key.as_str()));
    if let Some(r) = playing {
        let want = resolve(None, default_output.as_deref(), &engine.outputs());
        if let Err(e) = engine.place_receiver(r, &want) {
            tracing::warn!(id = r, error = %e, "placing the stream");
        }
        sources.placed.insert(r, want);
        if let Some(v) = engine.receiver_volume(r) {
            engine.set_receiver_volume(
                r,
                &Volume {
                    channels: vec![1.0; v.channels.len()],
                    mute: false,
                },
            );
        }
    }
    true
}

/// Where a stream plays whose source remembers `remembered`: there, else on
/// `default_output`, else on the audio system's default, unless that is a
/// sink to another node (or there is none of these): then nowhere.
pub fn resolve(
    remembered: Option<&str>,
    default_output: Option<&str>,
    outputs: &[Output],
) -> Placement {
    let present = |name: &str| outputs.iter().any(|o| o.name == name);
    if let Some(name) = remembered
        .into_iter()
        .chain(default_output)
        .find(|n| present(n))
    {
        return Placement::Output(name.to_owned());
    }
    match outputs.iter().find(|o| o.default) {
        Some(o) if sink_sender(&o.name).is_some() => Placement::Held,
        // Before the audio system has said, it decides.
        _ => Placement::Default,
    }
}

/// Follows the receivers' streams, and the volume of the senders' sinks,
/// every [`TICK`].
pub async fn run(node: Arc<Node>) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let node = node.clone();
        let _ = tokio::task::spawn_blocking(move || {
            follow(&node);
            crate::connection::follow_sinks(&node);
        })
        .await;
    }
}

/// Notes the user's moves and volume changes, and puts each stream where
/// it should play.
fn follow(node: &Node) {
    let Some(engine) = &node.engine else {
        return;
    };
    let default_output = node.settings().receiver.default_output;
    let pinned: BTreeSet<String> = {
        let peers = node.peers.lock().unwrap();
        let sources = node.sources.lock().unwrap();
        sources
            .list
            .iter()
            .filter_map(|s| s.node.clone())
            .filter(|n| peers.pinned(n))
            .collect()
    };
    let engine = engine.lock().unwrap();
    let outputs = engine.outputs();
    let running: Vec<(ReceiverId, Option<String>)> = engine
        .status()
        .receivers
        .iter()
        .map(|&(id, _)| (id, engine.receiver_key(id).flatten().map(str::to_owned)))
        .collect();
    let mut sources = node.sources.lock().unwrap();
    let mut changed = false;
    for (id, key) in &running {
        let chosen = engine.receiver_chosen_output(*id);
        if let Some(source) = key.as_deref().and_then(|k| sources.get_mut(k)) {
            if let Some(output) = &chosen
                && source.output != *output
            {
                tracing::info!(source = %source.id, output = ?output, "output chosen");
                source.output = output.clone();
                changed = true;
            }
            if let Some(v) = engine.receiver_volume(*id)
                && !source.volume.as_ref().is_some_and(|old| old.same(&v))
            {
                source.volume = Some(v);
                changed = true;
            }
        }
        let remembered = key
            .as_deref()
            .and_then(|k| sources.get(k))
            .and_then(|s| s.output.clone())
            .or_else(|| chosen.clone().flatten());
        let want = resolve(remembered.as_deref(), default_output.as_deref(), &outputs);
        if sources.placed.get(id) == Some(&want) {
            continue;
        }
        // Where the user just put it.
        let by_user = match (&want, &chosen) {
            (Placement::Output(name), Some(Some(c))) => name == c,
            _ => false,
        };
        if by_user {
            sources.placed.insert(*id, want);
            continue;
        }
        match engine.place_receiver(*id, &want) {
            Ok(_) => {
                tracing::info!(id, placement = ?want, "stream placed");
                sources.placed.insert(*id, want);
            }
            Err(e) => tracing::debug!(id, error = %e, "placing the stream"),
        }
    }
    drop(engine);
    sources
        .placed
        .retain(|id, _| running.iter().any(|(r, _)| r == id));
    let playing: BTreeSet<String> = running.into_iter().filter_map(|(_, k)| k).collect();
    if playing != sources.playing {
        // Ended streams keep when they last played.
        let now = unix_now();
        for key in sources.playing.clone() {
            if let Some(s) = sources.get_mut(&key) {
                s.used = now;
            }
        }
        sources.playing = playing;
        changed = true;
    }
    changed |= sources.evict(&pinned);
    if changed {
        sources.save();
    }
}

/// Seconds since the Unix epoch.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orpheus_core::OutputType;

    fn output(name: &str, default: bool) -> Output {
        Output {
            name: name.into(),
            description: name.into(),
            kind: OutputType::Generic,
            default,
            channels: orpheus_core::Layout::default_for(2),
        }
    }

    #[test]
    fn resolves_outputs() {
        let outputs = [
            output("hifi", false),
            output("usb", false),
            output("orpheus.send.K7QM3XRA", true),
        ];
        let at = |n: &str| Placement::Output(n.into());
        assert_eq!(resolve(Some("usb"), Some("hifi"), &outputs), at("usb"));
        assert_eq!(resolve(Some("gone"), Some("hifi"), &outputs), at("hifi"));
        assert_eq!(resolve(Some("gone"), None, &outputs), Placement::Held);
        assert_eq!(resolve(None, Some("gone"), &outputs), Placement::Held);
        let outputs = [output("hifi", true)];
        assert_eq!(resolve(None, None, &outputs), Placement::Default);
        assert_eq!(resolve(None, None, &[]), Placement::Default);
    }

    #[test]
    fn evicts_old_and_many() {
        let dir = std::env::temp_dir().join(format!("orpheus-sources-{}", std::process::id()));
        let mut s = Sources::load(
            dir.join("sources.json"),
            SourcesConfig {
                max_age_days: 30,
                max_entries: 2,
            },
        );
        let now = unix_now();
        let day = 86_400;
        for (key, used, node) in [
            ("a", now - 40 * day, None),
            ("b", now - 400 * day, Some("PINNED")),
            ("c", now - day, None),
            ("d", now - 2 * day, None),
            ("e", now - 3 * day, None),
        ] {
            s.list.push(Source {
                id: SourceId::generate().unwrap(),
                key: key.into(),
                name: key.into(),
                node: node.map(Into::into),
                output: None,
                volume: None,
                layout: None,
                used,
            });
        }
        s.playing.insert("a".into());
        assert!(s.evict(&BTreeSet::from(["PINNED".to_owned()])));
        let keys: Vec<&str> = s.list.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["c", "d", "a", "b"]);
    }
}
