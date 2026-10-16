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
//! passed on to one by default; it waits, unlinked, for an output. A
//! source with wires (see [`wires`](crate::wires)) plays only on them.
//!
//! A stream never moves to another output because its own went: it is
//! lost, and waits there, unlinked, until the output is back, the user
//! plays it elsewhere, or (with `[receiver] when_lost = "default"`)
//! `lost_after_ms` passed. Losing its output for longer than that, or its
//! last wire, parks it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orpheus_core::{Layout, SourceId};
use orpheus_engine::{Output, Placement, ReceiverId, Volume};
use serde::{Deserialize, Serialize};

use crate::api::Node;
use crate::config::{SourcesConfig, WhenLost};
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
    /// Plays nowhere until played again: the user removed its last wire.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parked: bool,
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
    /// The running receivers whose sources have wires.
    wired: BTreeSet<ReceiverId>,
    /// The running receivers whose sources are parked.
    unwired: BTreeSet<ReceiverId>,
    /// The keys playing at the last tick.
    playing: BTreeSet<String>,
    /// The running receivers whose output went, and since when.
    lost: BTreeMap<ReceiverId, Lost>,
    /// The running receivers not linked to the output they are placed on,
    /// which is there, and since when: it went and came back between two
    /// ticks.
    unlinked: BTreeMap<ReceiverId, Instant>,
    /// What each output seen is called, by machine name: to name lost ones.
    seen: BTreeMap<String, String>,
}

/// An output a stream lost.
struct Lost {
    /// Its machine name.
    output: String,
    since: Instant,
}

/// Why a stream plays nowhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parked {
    /// Its output went, by machine name and as it was called.
    Lost { output: String, description: String },
    /// The user removed its last wire.
    Unwired,
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
            wired: BTreeSet::new(),
            unwired: BTreeSet::new(),
            playing: BTreeSet::new(),
            lost: BTreeMap::new(),
            unlinked: BTreeMap::new(),
            seen: BTreeMap::new(),
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
                    parked: false,
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

    /// Whether receiver `id`'s stream waits, unlinked, for an output;
    /// not if it plays on wires or is parked.
    pub fn held(&self, id: ReceiverId) -> bool {
        self.placed.get(&id) == Some(&Placement::Held)
            && !self.wired.contains(&id)
            && !self.unwired.contains(&id)
    }

    /// Why receiver `id`'s stream plays nowhere, if it was parked: its
    /// output gone for `lost_after` or more, or its last wire removed.
    pub fn parked(&self, id: ReceiverId, lost_after: Duration) -> Option<Parked> {
        if self.unwired.contains(&id) {
            return Some(Parked::Unwired);
        }
        let lost = self
            .lost
            .get(&id)
            .filter(|l| l.since.elapsed() >= lost_after)?;
        Some(Parked::Lost {
            output: lost.output.clone(),
            description: self.seen.get(&lost.output).unwrap_or(&lost.output).clone(),
        })
    }

    /// Parks source `id` until played again, or lets it play; returns
    /// whether it changed.
    pub fn park(&mut self, id: SourceId, parked: bool) -> bool {
        let Some(s) = self.list.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        if s.parked == parked {
            return false;
        }
        s.parked = parked;
        self.save();
        true
    }

    /// Whether receiver `id`'s stream lost its output, parked or not yet.
    pub fn lost(&self, id: ReceiverId) -> bool {
        self.lost.contains_key(&id)
    }

    /// Whether receiver `id`'s stream plays on wires.
    pub fn on_wires(&self, id: ReceiverId) -> bool {
        self.wired.contains(&id)
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
            s.parked = false;
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

    /// Notes that the user chose `output` for `key` (`None`: the default);
    /// a parked source plays again.
    pub fn choose(&mut self, key: &str, output: Option<&str>) {
        if let Some(s) = self.get_mut(key)
            && (s.output.as_deref() != output || s.parked)
        {
            s.output = output.map(str::to_owned);
            s.parked = false;
            self.save();
        }
    }

    /// Forgets sources beyond the limits, oldest first; those playing,
    /// those from pinned nodes and those with wires stay. Returns whether
    /// any went.
    fn evict(&mut self, pinned: &BTreeSet<String>, wired: &BTreeSet<SourceId>) -> bool {
        let max_age = u64::from(self.limits.max_age_days) * 86_400;
        let max_entries = self.limits.max_entries as usize;
        let now = unix_now();
        let before = self.list.len();
        self.list.sort_by_key(|s| std::cmp::Reverse(s.used));
        let mut kept = 0;
        let playing = &self.playing;
        self.list.retain(|s| {
            if playing.contains(&s.key)
                || s.node.as_ref().is_some_and(|n| pinned.contains(n))
                || wired.contains(&s.id)
            {
                return true;
            }
            kept += 1;
            kept <= max_entries && now.saturating_sub(s.used) <= max_age
        });
        self.list.len() < before
    }
}

/// Forgets source `id`'s output, volume, layout and wires; a stream playing it goes where
/// streams play by default, at full volume. Returns whether there was one.
pub fn forget(node: &Node, id: SourceId) -> bool {
    let settings = node.settings().receiver;
    let (default_output, keep) = (
        settings.default_output,
        settings.when_lost != WhenLost::Default,
    );
    let engine = node.engine.as_ref().map(|e| e.lock().unwrap());
    let mut sources = node.sources.lock().unwrap();
    let Some(source) = sources.forget(id) else {
        return false;
    };
    node.wires.lock().unwrap().remove_source(id);
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
        let want = resolve(
            None,
            default_output.as_deref(),
            engine.chosen_default().as_deref(),
            &engine.outputs(),
            keep,
        );
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
/// `default_output`, else on the default output the user chose
/// (`chosen_default`) or the audio system's, unless that is a sink to
/// another node: then nowhere. With `keep`, a missing output counts: the
/// stream waits for it rather than play elsewhere.
pub fn resolve(
    remembered: Option<&str>,
    default_output: Option<&str>,
    chosen_default: Option<&str>,
    outputs: &[Output],
    keep: bool,
) -> Placement {
    let present = |name: &str| outputs.iter().any(|o| o.name == name);
    if let Some(name) = remembered
        .into_iter()
        .chain(default_output)
        .find(|n| keep || present(n))
    {
        return Placement::Output(name.to_owned());
    }
    // Orpheus follows the default itself, as the user chose it: when it
    // goes, the audio system picks another, which the stream doesn't
    // follow.
    let default = chosen_default
        .filter(|n| keep || present(n))
        .or_else(|| outputs.iter().find(|o| o.default).map(|o| o.name.as_str()));
    match default {
        Some(name) if sink_sender(name).is_some() => Placement::Held,
        Some(name) => Placement::Output(name.to_owned()),
        // Before the audio system has said, it decides.
        None => Placement::Default,
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
            crate::wires::follow(&node);
            crate::connection::follow_sinks(&node);
        })
        .await;
    }
}

/// Notes the user's moves and volume changes, and puts each stream where
/// it should play.
pub fn follow(node: &Node) {
    let Some(engine) = &node.engine else {
        return;
    };
    let settings = node.settings().receiver;
    let default_output = settings.default_output;
    let keep = settings.when_lost != WhenLost::Default;
    let lost_after = Duration::from_millis(settings.lost_after_ms.into());
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
    let wire_outputs = node.wires.lock().unwrap().outputs();
    let wired: BTreeSet<SourceId> = wire_outputs.keys().copied().collect();
    let engine = engine.lock().unwrap();
    let outputs = engine.outputs();
    let chosen_default = engine.chosen_default();
    let present = |name: &str| outputs.iter().any(|o| o.name == name);
    let running: Vec<(ReceiverId, Option<String>)> = engine
        .status()
        .receivers
        .iter()
        .map(|&(id, _)| (id, engine.receiver_key(id).flatten().map(str::to_owned)))
        .collect();
    let mut sources = node.sources.lock().unwrap();
    let mut changed = false;
    for o in &outputs {
        sources.seen.insert(o.name.clone(), o.description.clone());
    }
    sources.wired.clear();
    sources.unwired.clear();
    for (id, key) in &running {
        let chosen = engine.receiver_chosen_output(*id);
        if let Some(source) = key.as_deref().and_then(|k| sources.get_mut(k)) {
            if let Some(output) = &chosen
                && (source.output != *output || source.parked)
            {
                tracing::info!(source = %source.id, output = ?output, "output chosen");
                source.output = output.clone();
                source.parked = false;
                changed = true;
            }
            if let Some(v) = engine.receiver_volume(*id)
                && !source.volume.as_ref().is_some_and(|old| old.same(&v))
            {
                source.volume = Some(v);
                changed = true;
            }
        }
        let source = key.as_deref().and_then(|k| sources.get(k)).cloned();
        let remembered = source
            .as_ref()
            .and_then(|s| s.output.clone())
            .or_else(|| chosen.clone().flatten());
        let wire_outputs = source.as_ref().and_then(|s| wire_outputs.get(&s.id));
        let want = if let Some(on) = wire_outputs {
            sources.wired.insert(*id);
            // Lost when none of its wires' outputs is there.
            if on.iter().any(|o| present(o)) {
                sources.lost.remove(id);
            } else {
                sources.lost.entry(*id).or_insert_with(|| Lost {
                    output: on[0].clone(),
                    since: Instant::now(),
                });
            }
            Placement::Held
        } else if source.as_ref().is_some_and(|s| s.parked) {
            sources.unwired.insert(*id);
            sources.lost.remove(id);
            Placement::Held
        } else {
            resolve(
                remembered.as_deref(),
                default_output.as_deref(),
                chosen_default.as_deref(),
                &outputs,
                keep,
            )
        };
        let placed = sources.placed.get(id).cloned();
        // Its output went: it waits there, unlinked, unless the user moved
        // it or chose another place for it since, or it may play elsewhere
        // and waited long enough.
        let gone = match &placed {
            Some(Placement::Output(name)) if !present(name) && want != Placement::Held => {
                Some(name.clone())
            }
            _ => None,
        };
        if let Some(name) = gone {
            // A choice from before stays with the output.
            let moved = chosen
                .as_ref()
                .is_some_and(|c| c.as_deref() != Some(name.as_str()));
            let lost = sources.lost.entry(*id).or_insert_with(|| {
                tracing::info!(id, output = %name, "output lost");
                Lost {
                    output: name,
                    since: Instant::now(),
                }
            });
            let waits = if keep {
                placed.as_ref() == Some(&want)
            } else {
                lost.since.elapsed() < lost_after
            };
            if !moved && waits {
                continue;
            }
        }
        // Not linked although its output is there: it went and came back
        // between two ticks. Given a moment to link after placing.
        let relink = match &placed {
            Some(Placement::Output(name)) if engine.receiver_output(*id).as_ref() != Some(name) => {
                let since = *sources.unlinked.entry(*id).or_insert_with(Instant::now);
                since.elapsed() >= TICK
            }
            _ => {
                sources.unlinked.remove(id);
                false
            }
        };
        // Back: linked again, as the audio system won't.
        let back = wire_outputs.is_none()
            && (sources.lost.remove(id).is_some() || relink)
            && placed.as_ref() == Some(&want);
        if back {
            sources.unlinked.remove(id);
        }
        if placed.as_ref() == Some(&want) && !back {
            continue;
        }
        // Where the user just put it.
        let by_user = match (&want, &chosen) {
            (Placement::Output(name), Some(Some(c))) => name == c && !back,
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
    sources
        .lost
        .retain(|id, _| running.iter().any(|(r, _)| r == id));
    sources
        .unlinked
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
    changed |= sources.evict(&pinned, &wired);
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
        let r = |remembered, default_output, chosen, outputs: &[Output], keep| {
            resolve(remembered, default_output, chosen, outputs, keep)
        };
        assert_eq!(
            r(Some("usb"), Some("hifi"), None, &outputs, false),
            at("usb")
        );
        assert_eq!(
            r(Some("gone"), Some("hifi"), None, &outputs, false),
            at("hifi")
        );
        assert_eq!(
            r(Some("gone"), None, None, &outputs, false),
            Placement::Held
        );
        assert_eq!(
            r(None, Some("gone"), None, &outputs, false),
            Placement::Held
        );
        assert_eq!(r(None, None, Some("usb"), &outputs, false), at("usb"));
        assert_eq!(
            r(None, None, Some("gone"), &outputs, false),
            Placement::Held
        );
        // Kept: it waits for its output rather than play elsewhere.
        assert_eq!(
            r(Some("gone"), Some("hifi"), None, &outputs, true),
            at("gone")
        );
        assert_eq!(r(None, Some("gone"), None, &outputs, true), at("gone"));
        assert_eq!(r(None, None, Some("gone"), &outputs, true), at("gone"));
        assert_eq!(r(None, None, None, &outputs, true), Placement::Held);
        let outputs = [output("hifi", true)];
        assert_eq!(r(None, None, None, &outputs, true), at("hifi"));
        assert_eq!(r(None, None, None, &[], true), Placement::Default);
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
                parked: false,
                used,
            });
        }
        s.playing.insert("a".into());
        assert!(s.evict(&BTreeSet::from(["PINNED".to_owned()]), &BTreeSet::new()));
        let keys: Vec<&str> = s.list.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["c", "d", "a", "b"]);
    }
}
