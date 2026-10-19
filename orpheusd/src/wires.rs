//! Wires: channels of a source or an input played on chosen channels of
//! an output.
//!
//! A wire goes from a source the receivers remember (see
//! [`sources`](crate::sources)), or from an input of the audio system such
//! as a microphone, to an output: a device, an output created on its
//! channels, or a sink to another node. It names pairs of channels, each
//! by number, symbol or label, or none: then the channels go where they
//! belong, and what the output lacks is folded onto it (see
//! [`auto_pairs`]). A wire has a gain, and so has each pair; folded
//! channels are turned down by [`FOLD_DB`] unless the pairs are chosen.
//! What several wires bring into one output channel is summed. A source
//! with wires plays only on them, not on an output of its own.
//!
//! The wires are kept in `wires.json` in the state directory. Every
//! [`TICK`](crate::sources) they are resolved against the running
//! receivers and the inputs and outputs there are, and the engine keeps
//! their links.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use orpheus_core::{Layout, SourceId, Symbol};
use orpheus_engine::{Engine, Link, gain};
use serde::{Deserialize, Serialize};

use crate::api::Node;

/// Where a wire comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// A source the receivers remember, by ID.
    Source(SourceId),
    /// An input of the audio system, by machine name.
    Input(String),
}

/// A channel of a wire's end: a number counted from 1, or a symbol or
/// label of the end's layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Channel {
    Number(usize),
    Name(String),
}

impl Channel {
    /// Its index in `layout`: by number, else by symbol, else by label,
    /// ignoring case.
    pub fn index(&self, layout: &Layout) -> Option<usize> {
        match self {
            Self::Number(n) => (1..=layout.len()).contains(n).then(|| n - 1),
            Self::Name(name) => {
                let symbol: Option<Symbol> = name.parse().ok();
                layout
                    .symbols()
                    .position(|s| Some(s) == symbol)
                    .or_else(|| {
                        (0..layout.len()).find(|&i| layout.label(i).eq_ignore_ascii_case(name))
                    })
            }
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::Name(name) => f.write_str(name),
        }
    }
}

/// How much a channel folded onto another is turned down, in dB, such as
/// the centre onto left and right.
pub const FOLD_DB: f32 = -3.0;

pub fn is_zero(db: &f32) -> bool {
    *db == 0.0
}

/// An output channel and the input channel played on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pair {
    pub to: Channel,
    pub from: Channel,
    /// Its gain in dB, on top of the wire's.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gain_db: f32,
}

/// Channels of a source or input played on an output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Ends")]
pub struct Wire {
    #[serde(flatten)]
    pub from: Origin,
    /// The output, by machine name.
    pub output: String,
    /// The channels it links; `None` for all, where they belong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<Pair>>,
    /// Its gain in dB: 0 plays the channels as they are.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gain_db: f32,
    /// Silent, whatever the gain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mute: bool,
}

impl Wire {
    /// Whether its gains are ones Orpheus takes; else what is wrong.
    pub fn check_gains(&self) -> Result<(), String> {
        let gains =
            std::iter::once(self.gain_db).chain(self.channels.iter().flatten().map(|p| p.gain_db));
        for db in gains {
            if !gain::valid(db) {
                return Err(format!(
                    "a gain of {db} dB; gains go from {} to +{} dB",
                    gain::MIN_DB,
                    gain::MAX_DB
                ));
            }
        }
        Ok(())
    }
}

/// A wire as written, with either end it may come from.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ends {
    source: Option<SourceId>,
    input: Option<String>,
    output: String,
    channels: Option<Vec<Pair>>,
    #[serde(default)]
    gain_db: f32,
    #[serde(default)]
    mute: bool,
}

impl TryFrom<Ends> for Wire {
    type Error = &'static str;

    fn try_from(e: Ends) -> Result<Self, &'static str> {
        let from = match (e.source, e.input) {
            (Some(id), None) => Origin::Source(id),
            (None, Some(name)) => Origin::Input(name),
            _ => return Err("a wire comes from either a source or an input"),
        };
        Ok(Self {
            from,
            output: e.output,
            channels: e.channels,
            gain_db: e.gain_db,
            mute: e.mute,
        })
    }
}

/// The wires, kept in `wires.json`.
pub struct Wires {
    path: PathBuf,
    list: Vec<Wire>,
}

impl Wires {
    /// Reads `path`; missing or unreadable means no wires.
    pub fn load(path: PathBuf) -> Self {
        let list = match std::fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|e| {
                tracing::warn!(error = %e, path = %path.display(), "ignoring saved wires");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        Self { path, list }
    }

    fn save(&self) {
        let json = serde_json::to_vec_pretty(&self.list).expect("wires serialize");
        if let Err(e) = crate::home::write_atomic(&self.path, &json) {
            tracing::warn!(error = %e, path = %self.path.display(), "saving wires");
        }
    }

    pub fn list(&self) -> &[Wire] {
        &self.list
    }

    /// Adds `wire`, replacing the one between the same ends; returns
    /// whether anything changed.
    pub fn set(&mut self, wire: Wire) -> bool {
        match self
            .list
            .iter_mut()
            .find(|w| w.from == wire.from && w.output == wire.output)
        {
            Some(w) if *w == wire => return false,
            Some(w) => *w = wire,
            None => self.list.push(wire),
        }
        self.save();
        true
    }

    /// Sets the gain and muting of the wire from `from` to `output`, as far
    /// as given; `None` if there is no such wire, else the wire.
    pub fn set_gain(
        &mut self,
        from: &Origin,
        output: &str,
        gain_db: Option<f32>,
        mute: Option<bool>,
    ) -> Option<Wire> {
        let w = self
            .list
            .iter_mut()
            .find(|w| w.from == *from && w.output == output)?;
        let before = (w.gain_db, w.mute);
        w.gain_db = gain_db.unwrap_or(w.gain_db);
        w.mute = mute.unwrap_or(w.mute);
        let wire = w.clone();
        if (wire.gain_db, wire.mute) != before {
            self.save();
        }
        Some(wire)
    }

    /// Sets the gain of the pair that plays `channel` on `to` in the wire
    /// from `from` to `output`; `None` if there is no such pair, else the
    /// wire.
    pub fn set_pair_gain(
        &mut self,
        from: &Origin,
        output: &str,
        to: &Channel,
        channel: &Channel,
        gain_db: f32,
    ) -> Option<Wire> {
        let w = self
            .list
            .iter_mut()
            .find(|w| w.from == *from && w.output == output)?;
        let p = w
            .channels
            .iter_mut()
            .flatten()
            .find(|p| p.to == *to && p.from == *channel)?;
        let changed = p.gain_db != gain_db;
        p.gain_db = gain_db;
        let wire = w.clone();
        if changed {
            self.save();
        }
        Some(wire)
    }

    /// Removes the wire from `from` to `output`; `false` if there is none.
    pub fn remove(&mut self, from: &Origin, output: &str) -> bool {
        let before = self.list.len();
        self.list
            .retain(|w| !(w.from == *from && w.output == output));
        let removed = self.list.len() < before;
        if removed {
            self.save();
        }
        removed
    }

    /// Removes the wires of source `id`.
    pub fn remove_source(&mut self, id: SourceId) {
        let before = self.list.len();
        self.list.retain(|w| w.from != Origin::Source(id));
        if self.list.len() < before {
            self.save();
        }
    }

    /// The outputs each source with wires has them to.
    pub fn outputs(&self) -> BTreeMap<SourceId, Vec<String>> {
        let mut outputs: BTreeMap<SourceId, Vec<String>> = BTreeMap::new();
        for w in &self.list {
            if let Origin::Source(id) = w.from {
                outputs.entry(id).or_default().push(w.output.clone());
            }
        }
        outputs
    }
}

/// The channel pairs, input then output channel, that a wire with
/// `channels` links between these layouts, and the gain of each in dB;
/// pairs naming channels that aren't there are left out. Without chosen
/// pairs, folded channels are turned down by [`FOLD_DB`].
pub fn pairs(
    channels: Option<&[Pair]>,
    from: &Layout,
    to: &Layout,
) -> (Vec<(usize, usize)>, Vec<f32>) {
    let Some(channels) = channels else {
        let pairs = auto_pairs(from, to);
        let folded = crate::mix::wired(&pairs, from, to, true).folded;
        let gains = folded
            .into_iter()
            .map(|f| if f { FOLD_DB } else { 0.0 })
            .collect();
        return (pairs, gains);
    };
    let mut pairs: Vec<((usize, usize), f32)> = channels
        .iter()
        .filter_map(|p| Some(((p.from.index(from)?, p.to.index(to)?), p.gain_db)))
        .collect();
    // The first of two naming the same channels counts.
    pairs.sort_by_key(|(p, _)| *p);
    pairs.dedup_by_key(|(p, _)| *p);
    pairs.into_iter().unzip()
}

/// The channel pairs that play all of `from` on `to`. Mono plays on every
/// channel, and everything on a mono output. Otherwise channels go to the
/// channel of the same symbol; those the output lacks are folded onto it,
/// left and right sides onto its left and right (else its centre), centre
/// and mono onto its centre (else left and right); the LFE and narration
/// channels are left out. With no symbols in common, channels go by
/// number.
pub fn auto_pairs(from: &Layout, to: &Layout) -> Vec<(usize, usize)> {
    use Symbol::*;
    if from.len() == 1 {
        return (0..to.len()).map(|t| (0, t)).collect();
    }
    if to.len() == 1 {
        return (0..from.len()).map(|f| (f, 0)).collect();
    }
    let targets: Vec<Symbol> = to.symbols().collect();
    let find = |s: Symbol| targets.iter().position(|&t| t == s);
    let mut pairs = Vec::new();
    let mut rest = Vec::new();
    for (i, s) in from.symbols().enumerate() {
        match find(s) {
            Some(t) => pairs.push((i, t)),
            None => rest.push((i, s)),
        }
    }
    if pairs.is_empty() {
        return (0..from.len().min(to.len())).map(|i| (i, i)).collect();
    }
    let (l, r, c) = (find(L), find(R), find(C));
    for (i, s) in rest {
        let onto = match s {
            L | Ls | Lss | Lrs | Lc | Lt | Lst => vec![l.or(c)],
            R | Rs | Rss | Rrs | Rc | Rt | Rst => vec![r.or(c)],
            C | Cs | S | M1 | M2 => match c {
                Some(c) => vec![Some(c)],
                None => vec![l, r],
            },
            _ => Vec::new(),
        };
        pairs.extend(onto.into_iter().flatten().map(|t| (i, t)));
    }
    pairs.sort_unstable();
    pairs
}

/// A wire resolved against what there is now.
pub struct Resolved {
    /// The machine name of the node it comes from: the receiver's stream
    /// or the input; `None` while the source plays nowhere or the input
    /// is missing.
    pub from_node: Option<String>,
    pub from_layout: Option<Layout>,
    /// `None` while the output is missing.
    pub to_layout: Option<Layout>,
    /// The channel pairs it links, input then output channel.
    pub pairs: Vec<(usize, usize)>,
    /// The gain of each pair in dB, besides the wire's.
    pub gains: Vec<f32>,
}

impl Resolved {
    /// The gain of pair `i` in dB, with the wire's.
    pub fn gain_db(&self, wire: &Wire, i: usize) -> f32 {
        wire.gain_db + self.gains.get(i).copied().unwrap_or(0.0)
    }

    pub fn links<'a>(&'a self, wire: &'a Wire) -> impl Iterator<Item = Link> + 'a {
        self.pairs
            .iter()
            .enumerate()
            .filter_map(move |(i, &(f, t))| {
                Some(Link {
                    from: self.from_node.clone()?,
                    from_channel: f,
                    to: wire.output.clone(),
                    to_channel: t,
                    gain: if wire.mute {
                        0.0
                    } else {
                        gain::linear(self.gain_db(wire, i))
                    },
                })
            })
    }
}

/// Resolves `wires` against the running receivers and the inputs and
/// outputs there are.
pub fn resolve(node: &Node, engine: &Engine, wires: &[Wire]) -> Vec<Resolved> {
    // The running receiver playing each source.
    let playing: BTreeMap<SourceId, u32> = {
        let sources = node.sources.lock().unwrap();
        engine
            .status()
            .receivers
            .iter()
            .filter_map(|&(r, _)| {
                let key = engine.receiver_key(r).flatten()?;
                Some((sources.get(key)?.id, r))
            })
            .collect()
    };
    let inputs = engine.inputs();
    let outputs = engine.outputs();
    wires
        .iter()
        .map(|w| {
            let (from_node, from_layout) = match &w.from {
                Origin::Source(id) => match playing.get(id) {
                    Some(&r) => (
                        Some(crate::connection::receiver_stream(r)),
                        engine.receiver_layout(r).cloned(),
                    ),
                    None => (None, None),
                },
                Origin::Input(name) => match inputs.iter().find(|i| i.name == *name) {
                    Some(i) => (Some(i.name.clone()), Some(i.channels.clone())),
                    None => (None, None),
                },
            };
            let to_layout = outputs
                .iter()
                .find(|o| o.name == w.output)
                .map(|o| o.channels.clone());
            let (pairs, gains) = match (&from_layout, &to_layout) {
                (Some(f), Some(t)) => pairs(w.channels.as_deref(), f, t),
                _ => Default::default(),
            };
            Resolved {
                from_node,
                from_layout,
                to_layout,
                pairs,
                gains,
            }
        })
        .collect()
}

/// Hands the links of all wires to the engine.
pub fn follow(node: &Node) {
    let Some(engine) = &node.engine else {
        return;
    };
    let wires = node.wires.lock().unwrap().list().to_vec();
    let engine = engine.lock().unwrap();
    let resolved = resolve(node, &engine, &wires);
    let links: Vec<Link> = wires
        .iter()
        .zip(&resolved)
        .flat_map(|(w, r)| r.links(w).collect::<Vec<_>>())
        .collect();
    if let Err(e) = engine.set_links(&links)
        && !wires.is_empty()
    {
        tracing::debug!(error = %e, "linking the wires");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(s: &str) -> Layout {
        Layout::parse(s).unwrap()
    }

    #[test]
    fn auto_pairs_fold() {
        assert_eq!(auto_pairs(&layout("ST"), &layout("ST")), [(0, 0), (1, 1)]);
        assert_eq!(auto_pairs(&layout("M"), &layout("ST")), [(0, 0), (0, 1)]);
        assert_eq!(auto_pairs(&layout("ST"), &layout("M")), [(0, 0), (1, 0)]);
        // L R C LFE Ls Rs onto L R: C on both, LFE left out.
        assert_eq!(
            auto_pairs(&layout("51"), &layout("ST")),
            [(0, 0), (1, 1), (2, 0), (2, 1), (4, 0), (5, 1)]
        );
        // Stereo on 5.1 stays on the front.
        assert_eq!(auto_pairs(&layout("ST"), &layout("51")), [(0, 0), (1, 1)]);
        // Nothing in common: by number.
        assert_eq!(auto_pairs(&layout("DM"), &layout("ST")), [(0, 0), (1, 1)]);
        assert_eq!(auto_pairs(&layout("U04"), &layout("ST")), [(0, 0), (1, 1)]);
    }

    #[test]
    fn channels_by_number_symbol_or_label() {
        let mut l = layout("U02");
        let mics = Layout::new(
            l.channels()
                .iter()
                .enumerate()
                .map(|(i, c)| orpheus_core::layout::Channel {
                    symbol: c.symbol,
                    label: Some(format!("Mic {}", i + 1)),
                })
                .collect(),
        );
        l = mics;
        let st = layout("ST");
        assert_eq!(Channel::Number(2).index(&l), Some(1));
        assert_eq!(Channel::Number(3).index(&l), None);
        assert_eq!(Channel::Number(0).index(&l), None);
        assert_eq!(Channel::Name("U02".into()).index(&l), Some(1));
        assert_eq!(Channel::Name("mic 2".into()).index(&l), Some(1));
        assert_eq!(Channel::Name("R".into()).index(&st), Some(1));
        assert_eq!(Channel::Name("right".into()).index(&st), Some(1));
        let swap = [
            Pair {
                to: Channel::Name("L".into()),
                from: Channel::Name("R".into()),
                gain_db: 0.0,
            },
            Pair {
                to: Channel::Name("R".into()),
                from: Channel::Name("L".into()),
                gain_db: -6.0,
            },
            Pair {
                to: Channel::Number(3),
                from: Channel::Number(1),
                gain_db: 0.0,
            },
        ];
        assert_eq!(
            pairs(Some(&swap), &st, &st),
            (vec![(0, 1), (1, 0)], vec![-6.0, 0.0])
        );
    }

    #[test]
    fn folded_channels_are_turned_down() {
        // L R C LFE Ls Rs onto L R: C, Ls and Rs folded.
        let (p, gains) = pairs(None, &layout("51"), &layout("ST"));
        assert_eq!(p, [(0, 0), (1, 1), (2, 0), (2, 1), (4, 0), (5, 1)]);
        assert_eq!(gains, [0.0, 0.0, FOLD_DB, FOLD_DB, FOLD_DB, FOLD_DB]);
        // Mono spread onto stereo, stereo summed onto mono.
        assert_eq!(
            pairs(None, &layout("M"), &layout("ST")).1,
            [FOLD_DB, FOLD_DB]
        );
        assert_eq!(
            pairs(None, &layout("ST"), &layout("M")).1,
            [FOLD_DB, FOLD_DB]
        );
        // Where they belong, or by number: as they are.
        assert_eq!(pairs(None, &layout("ST"), &layout("51")).1, [0.0, 0.0]);
        assert_eq!(pairs(None, &layout("DM"), &layout("ST")).1, [0.0, 0.0]);
    }

    #[test]
    fn links_play_at_the_wire_and_pair_gains() {
        let mut wire = Wire {
            from: Origin::Input("mic".into()),
            output: "out".into(),
            channels: None,
            gain_db: -6.0,
            mute: false,
        };
        let (pairs, gains) = pairs(None, &layout("M"), &layout("ST"));
        let r = Resolved {
            from_node: Some("mic".into()),
            from_layout: Some(layout("M")),
            to_layout: Some(layout("ST")),
            pairs,
            gains,
        };
        assert_eq!(r.gain_db(&wire, 0), -9.0);
        let links: Vec<Link> = r.links(&wire).collect();
        assert_eq!(links.len(), 2);
        assert!((links[1].gain - gain::linear(-9.0)).abs() < 1e-6);
        assert_eq!((links[1].from_channel, links[1].to_channel), (0, 1));
        wire.mute = true;
        assert!(r.links(&wire).all(|l| l.gain == 0.0));
        assert_eq!(r.gain_db(&wire, 0), -9.0, "muting keeps the gain");
    }

    #[test]
    fn gains_out_of_range() {
        let mut wire = Wire {
            from: Origin::Input("mic".into()),
            output: "out".into(),
            channels: Some(vec![Pair {
                to: Channel::Number(1),
                from: Channel::Number(1),
                gain_db: 12.0,
            }]),
            gain_db: -90.0,
            mute: false,
        };
        assert!(wire.check_gains().is_ok());
        wire.gain_db = 13.0;
        assert!(wire.check_gains().unwrap_err().contains("13 dB"));
        wire.gain_db = 0.0;
        wire.channels.as_mut().unwrap()[0].gain_db = f32::NEG_INFINITY;
        assert!(wire.check_gains().is_err());
    }

    #[test]
    fn wire_json() {
        let w = Wire {
            from: Origin::Input("alsa_input.usb".into()),
            output: "orpheus.output.kitchen".into(),
            channels: Some(vec![
                Pair {
                    to: Channel::Number(1),
                    from: Channel::Name("U03".into()),
                    gain_db: 0.0,
                },
                Pair {
                    to: Channel::Number(2),
                    from: Channel::Number(4),
                    gain_db: -3.5,
                },
            ]),
            gain_db: 0.0,
            mute: false,
        };
        let json = serde_json::to_string(&w).unwrap();
        assert_eq!(
            json,
            r#"{"input":"alsa_input.usb","output":"orpheus.output.kitchen","channels":[{"to":1,"from":"U03"},{"to":2,"from":4,"gain_db":-3.5}]}"#
        );
        assert_eq!(serde_json::from_str::<Wire>(&json).unwrap(), w);
        let w = Wire {
            gain_db: -6.0,
            mute: true,
            channels: None,
            ..w
        };
        let json = serde_json::to_string(&w).unwrap();
        assert_eq!(
            json,
            r#"{"input":"alsa_input.usb","output":"orpheus.output.kitchen","gain_db":-6.0,"mute":true}"#
        );
        assert_eq!(serde_json::from_str::<Wire>(&json).unwrap(), w);
        let both = r#"{"input":"a","source":"B73AQ0P0","output":"o"}"#;
        assert!(serde_json::from_str::<Wire>(both).is_err());
        assert!(serde_json::from_str::<Wire>(r#"{"output":"o"}"#).is_err());
    }
}
