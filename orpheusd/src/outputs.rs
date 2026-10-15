//! Outputs: what kind of device each output of the audio system is, and
//! outputs made of chosen channels of others.
//!
//! The audio system says what it knows; the user can override that per
//! output. Overrides and created outputs are kept in `outputs.json` in
//! the state directory. A
//! sink to another node is an output too: its type is the one the peer's
//! receiver announces, unless overridden here, and it is written into the
//! sink, so a receiver playing into it passes the type on.

use std::collections::BTreeMap;
use std::path::PathBuf;

use orpheus_core::{Layout, OutputType};
use orpheus_engine::{Output, OutputSpec, ReceiverId, SenderId};
use serde::{Deserialize, Serialize};

use crate::api::Node;

/// Machine names of the sinks to other nodes: `orpheus.send.<route ID>`.
pub fn sender_sink(id: SenderId) -> String {
    format!("orpheus.send.{id}")
}

/// The sender a sink belongs to, if it is a sink to another node.
pub fn sink_sender(name: &str) -> Option<SenderId> {
    name.strip_prefix("orpheus.send.")?.parse().ok()
}

/// What the user set for outputs: their types, and the outputs made of
/// channels of others. Kept in `outputs.json`.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    /// Output types by output name.
    #[serde(default)]
    types: BTreeMap<String, OutputType>,
    #[serde(default)]
    created: Vec<Created>,
}

/// An output the user made of chosen channels of another, such as
/// channels 3 and 4 of an audio interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Created {
    /// Its machine name: `orpheus.output.<slug of the description>`.
    pub name: String,
    pub description: String,
    /// The output it plays on, by machine name.
    pub device: String,
    /// The device's channels it plays on, counted from 1.
    pub channels: Vec<usize>,
    /// What its channels carry.
    pub layout: Layout,
}

impl Created {
    pub fn spec(&self) -> OutputSpec {
        OutputSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            layout: self.layout.clone(),
            device: self.device.clone(),
            channels: self.channels.iter().map(|c| c - 1).collect(),
        }
    }
}

/// The machine name of an output created as `description`: `Living
/// room` → `orpheus.output.living-room`; `None` if nothing is left.
pub fn created_name(description: &str) -> Option<String> {
    let mut slug = String::new();
    for c in description.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    (!slug.is_empty()).then(|| format!("orpheus.output.{slug}"))
}

/// The user's output types and created outputs.
pub struct Overrides {
    saved: Saved,
    path: PathBuf,
}

impl Overrides {
    /// Reads `path`; missing or unreadable means nothing set.
    pub fn load(path: PathBuf) -> Self {
        let saved = match std::fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|e| {
                tracing::warn!(error = %e, path = %path.display(), "ignoring saved outputs");
                Saved::default()
            }),
            Err(_) => Saved::default(),
        };
        Self { saved, path }
    }

    fn save(&self) {
        let json = serde_json::to_vec_pretty(&self.saved).expect("outputs serialize");
        if let Err(e) = crate::home::write_atomic(&self.path, &json) {
            tracing::warn!(error = %e, path = %self.path.display(), "saving outputs");
        }
    }

    pub fn get(&self, name: &str) -> Option<OutputType> {
        self.saved.types.get(name).copied()
    }

    /// Sets the type of an output, or with `None` goes back to the
    /// detected one; returns whether anything changed.
    pub fn set(&mut self, name: &str, kind: Option<OutputType>) -> bool {
        let types = &mut self.saved.types;
        let changed = match kind {
            Some(k) => types.insert(name.to_owned(), k) != Some(k),
            None => types.remove(name).is_some(),
        };
        if changed {
            self.save();
        }
        changed
    }

    pub fn created(&self) -> &[Created] {
        &self.saved.created
    }

    pub fn find_created(&self, name: &str) -> Option<&Created> {
        self.saved.created.iter().find(|c| c.name == name)
    }

    /// Keeps a created output; `false` if there is one of that name.
    pub fn add_created(&mut self, created: Created) -> bool {
        if self.find_created(&created.name).is_some() {
            return false;
        }
        self.saved.created.push(created);
        self.save();
        true
    }

    /// Forgets a created output and its type.
    pub fn remove_created(&mut self, name: &str) -> Option<Created> {
        let i = self.saved.created.iter().position(|c| c.name == name)?;
        let created = self.saved.created.remove(i);
        self.saved.types.remove(name);
        self.save();
        Some(created)
    }
}

/// Creates the outputs the user made, at start.
pub fn restore(node: &Node) {
    let Some(engine) = &node.engine else {
        return;
    };
    let created = node.outputs.lock().unwrap().created().to_vec();
    let mut engine = engine.lock().unwrap();
    for c in created {
        match engine.add_output(&c.spec()) {
            Ok(()) => tracing::info!(output = %c.name, device = %c.device, "output created"),
            Err(e) => tracing::warn!(output = %c.name, error = %e, "creating an output"),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_of_created_outputs() {
        let name = |d| created_name(d);
        assert_eq!(name("Kitchen").as_deref(), Some("orpheus.output.kitchen"));
        assert_eq!(
            name(" Living room, left! ").as_deref(),
            Some("orpheus.output.living-room-left")
        );
        assert_eq!(name("Küche 2").as_deref(), Some("orpheus.output.küche-2"));
        assert_eq!(name("…"), None);
    }
}
