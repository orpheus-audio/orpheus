//! Output types: what kind of device each output of the audio system is.
//!
//! The audio system says what it knows; the user can override that per
//! output. Overrides are kept in `outputs.json` in the state directory. A
//! sink to another node is an output too: its type is the one the peer's
//! receiver announces, unless overridden here, and it is written into the
//! sink, so a receiver playing into it passes the type on.

use std::collections::BTreeMap;
use std::path::PathBuf;

use orpheus_core::OutputType;
use orpheus_engine::{Output, ReceiverId, SenderId};

use crate::api::Node;

/// Machine names of the sinks to other nodes: `orpheus.send.<route ID>`.
pub fn sender_sink(id: SenderId) -> String {
    format!("orpheus.send.{id}")
}

/// The sender a sink belongs to, if it is a sink to another node.
pub fn sink_sender(name: &str) -> Option<SenderId> {
    name.strip_prefix("orpheus.send.")?.parse().ok()
}

/// The user's output types, by output name.
pub struct Overrides {
    types: BTreeMap<String, OutputType>,
    path: PathBuf,
}

impl Overrides {
    /// Reads `path`; missing or unreadable means no overrides.
    pub fn load(path: PathBuf) -> Self {
        let types = match std::fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|e| {
                tracing::warn!(error = %e, path = %path.display(), "ignoring output types");
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        };
        Self { types, path }
    }

    pub fn get(&self, name: &str) -> Option<OutputType> {
        self.types.get(name).copied()
    }

    /// Sets the type of an output, or with `None` goes back to the
    /// detected one; returns whether anything changed.
    pub fn set(&mut self, name: &str, kind: Option<OutputType>) -> bool {
        let changed = match kind {
            Some(k) => self.types.insert(name.to_owned(), k) != Some(k),
            None => self.types.remove(name).is_some(),
        };
        if changed {
            let json = serde_json::to_vec_pretty(&self.types).expect("types serialize");
            if let Err(e) = crate::home::write_atomic(&self.path, &json) {
                tracing::warn!(error = %e, path = %self.path.display(), "saving output types");
            }
        }
        changed
    }
}

/// The audio system's outputs and which ones the receivers play on.
pub struct View {
    pub outputs: Vec<Output>,
    /// The default output, where streams go unless moved.
    pub default: Option<String>,
    /// The output each running receiver's stream is linked to.
    pub receivers: BTreeMap<ReceiverId, String>,
}

impl View {
    pub fn of(node: &Node) -> Self {
        let Some(engine) = &node.engine else {
            return Self {
                outputs: Vec::new(),
                default: None,
                receivers: BTreeMap::new(),
            };
        };
        let engine = engine.lock().unwrap();
        let outputs = engine.outputs();
        let default = outputs.iter().find(|o| o.default).map(|o| o.name.clone());
        let receivers = engine
            .status()
            .receivers
            .iter()
            .filter_map(|&(id, _)| Some((id, engine.receiver_output(id)?)))
            .collect();
        Self {
            outputs,
            default,
            receivers,
        }
    }

    pub fn find(&self, name: &str) -> Option<&Output> {
        self.outputs.iter().find(|o| o.name == name)
    }

    /// The output receiver `id` plays on: the one its stream is linked to,
    /// else the default, where it would go.
    pub fn receiver(&self, id: ReceiverId) -> Option<&Output> {
        let name = self.receivers.get(&id).or(self.default.as_ref())?;
        self.find(name)
    }

    /// The kind of output receiver `id` plays on, as announced to other
    /// nodes.
    pub fn receiver_type(&self, node: &Node, id: ReceiverId) -> OutputType {
        self.receiver(id)
            .map_or(OutputType::Generic, |o| effective(node, o))
    }
}

/// The type of an output: the override, else what was detected.
pub fn effective(node: &Node, output: &Output) -> OutputType {
    node.outputs
        .lock()
        .unwrap()
        .get(&output.name)
        .unwrap_or(output.kind)
}
