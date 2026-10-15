//! Wires: channels of a source or an input played on chosen channels of
//! an output.
//!
//! A wire goes from a source the receivers remember (see
//! [`sources`](crate::sources)), or from an input of the audio system such
//! as a microphone, to an output: a device, an output created on its
//! channels, or a sink to another node. It names pairs of channels, each
//! by number, symbol or label, or none: then the channels go where they
//! belong, and what the output lacks is folded onto it (see
//! [`auto_pairs`]). What several wires bring into one output channel is
//! summed. A source with wires plays only on them, not on an output of its
//! own.
//!
//! The wires are kept in `wires.json` in the state directory. Every
//! [`TICK`](crate::sources) they are resolved against the running
//! receivers and the inputs and outputs there are, and the engine keeps
//! their links.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use orpheus_core::{Layout, SourceId, Symbol};
use orpheus_engine::{Engine, Link};
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

/// An output channel and the input channel played on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pair {
    pub to: Channel,
    pub from: Channel,
}

/// Channels of a source or input played on an output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Ends")]
pub struct Wire {
    #[serde(flatten)]
    pub from: Origin,
    /// The output, by machine name.
    pub output: String,
    /// The channels it links; `None` for all, where they belong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<Pair>>,
}

/// A wire as written, with either end it may come from.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ends {
    source: Option<SourceId>,
    input: Option<String>,
    output: String,
    channels: Option<Vec<Pair>>,
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

    /// The sources that have wires.
    pub fn sources(&self) -> BTreeSet<SourceId> {
        self.list
            .iter()
            .filter_map(|w| match w.from {
                Origin::Source(id) => Some(id),
                Origin::Input(_) => None,
            })
            .collect()
    }
}

/// The channel pairs, input then output channel, that a wire with
/// `channels` links between these layouts; pairs naming channels that
/// aren't there are left out.
pub fn pairs(channels: Option<&[Pair]>, from: &Layout, to: &Layout) -> Vec<(usize, usize)> {
    let Some(channels) = channels else {
        return auto_pairs(from, to);
    };
    let mut pairs: Vec<(usize, usize)> = channels
        .iter()
        .filter_map(|p| Some((p.from.index(from)?, p.to.index(to)?)))
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

/// The channel pairs that play all of `from` on `to`. Mono plays on every
/// channel, and everything on a mono output. Otherwise channels go to the
/// channel of the same symbol; those the output lacks are folded onto it,
/// left and right sides onto its left and right (else its centre), centre
/// and mono onto its centre (else left and right); the LFE and narration
/// channels are left out. With no symbols in common, channels go by
/// number. Folded channels are summed without being turned down.
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
}

impl Resolved {
    pub fn links<'a>(&'a self, wire: &'a Wire) -> impl Iterator<Item = Link> + 'a {
        self.pairs.iter().filter_map(move |&(f, t)| {
            Some(Link {
                from: self.from_node.clone()?,
                from_channel: f,
                to: wire.output.clone(),
                to_channel: t,
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
            let pairs = match (&from_layout, &to_layout) {
                (Some(f), Some(t)) => pairs(w.channels.as_deref(), f, t),
                _ => Vec::new(),
            };
            Resolved {
                from_node,
                from_layout,
                to_layout,
                pairs,
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
    let mut links: Vec<Link> = wires
        .iter()
        .zip(&resolved)
        .flat_map(|(w, r)| r.links(w).collect::<Vec<_>>())
        .collect();
    links.sort_unstable();
    links.dedup();
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
            },
            Pair {
                to: Channel::Name("R".into()),
                from: Channel::Name("L".into()),
            },
            Pair {
                to: Channel::Number(3),
                from: Channel::Number(1),
            },
        ];
        assert_eq!(pairs(Some(&swap), &st, &st), [(0, 1), (1, 0)]);
    }

    #[test]
    fn wire_json() {
        let w = Wire {
            from: Origin::Input("alsa_input.usb".into()),
            output: "orpheus.output.kitchen".into(),
            channels: Some(vec![Pair {
                to: Channel::Number(1),
                from: Channel::Name("U03".into()),
            }]),
        };
        let json = serde_json::to_string(&w).unwrap();
        assert_eq!(
            json,
            r#"{"input":"alsa_input.usb","output":"orpheus.output.kitchen","channels":[{"to":1,"from":"U03"}]}"#
        );
        assert_eq!(serde_json::from_str::<Wire>(&json).unwrap(), w);
        let both = r#"{"input":"a","source":"B73AQ0P0","output":"o"}"#;
        assert!(serde_json::from_str::<Wire>(both).is_err());
        assert!(serde_json::from_str::<Wire>(r#"{"output":"o"}"#).is_err());
    }
}
