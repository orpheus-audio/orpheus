//! What this node remembers about other Orpheus nodes, by device ID:
//! aliases and pins, kept in `peers.json` in the state directory.
//!
//! An alias is this node's own name for a peer. Wherever this node shows
//! the peer, the alias replaces the name it announces: in the node list,
//! in the name of a sink to it (unless the user named the sink) and in the
//! name of a stream received from it. `connect` finds the peer by it too.
//!
//! A pinned peer stays listed while it is offline, also after a restart,
//! as it was last seen.

use std::collections::BTreeMap;
use std::path::PathBuf;

use orpheus_core::DeviceId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A node as this node knows it: as discovery sees it, with alias and pin.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Known {
    #[serde(flatten)]
    pub node: orpheus_discovery::Node,
    /// This node's own name for it, shown instead of the announced name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Listed even while offline, also after a restart.
    pub pinned: bool,
}

impl Known {
    /// The name to show: the alias, else the announced name.
    pub fn shown(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.node.name)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pinned: bool,
    /// How a pinned peer was last seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seen: Option<Seen>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Seen {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    host: String,
    addresses: Vec<String>,
    port: u16,
}

impl Seen {
    fn of(n: &orpheus_discovery::Node) -> Self {
        Self {
            name: n.name.clone(),
            version: n.version.clone(),
            host: n.host.clone(),
            addresses: n.addresses.clone(),
            port: n.port,
        }
    }
}

/// A change to a peer's alias or pin.
pub struct Change {
    /// `Some(None)` removes the alias.
    pub alias: Option<Option<String>>,
    pub pinned: Option<bool>,
}

/// Why a change was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not an Orpheus node this node has seen.
    Unknown,
    /// The alias is empty or only spaces.
    Empty,
    /// Another peer has the alias.
    Taken(String),
}

/// Aliases and pins. Lock it last.
pub struct Peers {
    entries: BTreeMap<String, Entry>,
    path: PathBuf,
}

impl Peers {
    /// Reads `path`; missing or unreadable means nothing remembered.
    pub fn load(path: PathBuf) -> Self {
        let entries = match std::fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|e| {
                tracing::warn!(error = %e, path = %path.display(), "ignoring saved peers");
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        };
        Self { entries, path }
    }

    fn save(&self) {
        let json = serde_json::to_vec_pretty(&self.entries).expect("peers serialize");
        if let Err(e) = crate::home::write_atomic(&self.path, &json) {
            tracing::warn!(error = %e, path = %self.path.display(), "saving peers");
        }
    }

    /// The alias of the node with device ID `id`.
    pub fn alias(&self, id: &str) -> Option<&str> {
        self.entries.get(id)?.alias.as_deref()
    }

    /// The nodes discovery sees, with their aliases and pins, and the
    /// pinned ones it doesn't see as last seen. Online first, then by the
    /// name shown.
    pub fn list(&self, seen: Vec<orpheus_discovery::Node>) -> Vec<Known> {
        let mut list: Vec<Known> = seen
            .into_iter()
            .map(|node| {
                let entry = node.id.as_ref().and_then(|id| self.entries.get(id));
                Known {
                    alias: entry.and_then(|e| e.alias.clone()),
                    pinned: entry.is_some_and(|e| e.pinned),
                    node,
                }
            })
            .collect();
        for (id, e) in &self.entries {
            let Some(s) = e.seen.as_ref().filter(|_| e.pinned) else {
                continue;
            };
            if list.iter().any(|k| k.node.id.as_ref() == Some(id)) {
                continue;
            }
            list.push(Known {
                node: orpheus_discovery::Node {
                    id: Some(id.clone()),
                    name: s.name.clone(),
                    version: s.version.clone(),
                    host: s.host.clone(),
                    addresses: s.addresses.clone(),
                    port: s.port,
                    online: false,
                    receivers: None,
                    link: None,
                },
                alias: e.alias.clone(),
                pinned: true,
            });
        }
        list.sort_by(|a, b| {
            (!a.node.online, a.shown().to_lowercase(), &a.node.id).cmp(&(
                !b.node.online,
                b.shown().to_lowercase(),
                &b.node.id,
            ))
        });
        list
    }

    /// Keeps how pinned peers look now, for when they are offline.
    pub fn remember(&mut self, nodes: &[orpheus_discovery::Node]) {
        let mut changed = false;
        for n in nodes.iter().filter(|n| n.online) {
            let Some(e) = n.id.as_ref().and_then(|id| self.entries.get_mut(id)) else {
                continue;
            };
            let now = Seen::of(n);
            if e.pinned && e.seen.as_ref() != Some(&now) {
                e.seen = Some(now);
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }

    /// Changes the alias or pin of the node with device ID `id`, which
    /// must be in `nodes` (what discovery sees) or already remembered.
    pub fn change(
        &mut self,
        id: DeviceId,
        change: Change,
        nodes: &[orpheus_discovery::Node],
    ) -> Result<(), Refused> {
        let id = id.to_string();
        let found = nodes.iter().find(|n| n.id.as_ref() == Some(&id));
        if found.is_none() && !self.entries.contains_key(&id) {
            return Err(Refused::Unknown);
        }
        let alias = match change.alias {
            Some(Some(a)) => {
                let a = a.trim().to_owned();
                if a.is_empty() {
                    return Err(Refused::Empty);
                }
                let other = self.entries.iter().find(|(i, e)| {
                    **i != id && e.alias.as_ref().is_some_and(|o| o.eq_ignore_ascii_case(&a))
                });
                if let Some((other, _)) = other {
                    return Err(Refused::Taken(other.clone()));
                }
                Some(Some(a))
            }
            other => other,
        };
        let e = self.entries.entry(id.clone()).or_default();
        if let Some(a) = alias {
            e.alias = a;
        }
        if let Some(p) = change.pinned {
            e.pinned = p;
        }
        e.seen = match (e.pinned, found) {
            (false, _) => None,
            (true, Some(n)) => Some(Seen::of(n)),
            (true, None) => e.seen.take(),
        };
        if e.alias.is_none() && !e.pinned {
            self.entries.remove(&id);
        }
        self.save();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str, online: bool) -> orpheus_discovery::Node {
        orpheus_discovery::Node {
            id: Some(id.into()),
            name: name.into(),
            version: Some("0.0.1".into()),
            host: format!("orpheus-{}.local.", id.to_lowercase()),
            addresses: vec!["10.0.0.2".into()],
            port: 7480,
            online,
            receivers: None,
            link: None,
        }
    }

    fn peers() -> (Peers, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "orpheus-peers-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers.json");
        let _ = std::fs::remove_file(&path);
        (Peers::load(path.clone()), path)
    }

    const B: &str = "B73AQ0P0";
    const C: &str = "C73AQ0P0";

    fn set(alias: Option<Option<&str>>, pinned: Option<bool>) -> Change {
        Change {
            alias: alias.map(|a| a.map(Into::into)),
            pinned,
        }
    }

    #[test]
    fn aliases_rename_and_must_be_unique() {
        let (mut p, _) = peers();
        let seen = vec![node(B, "Küche", true), node(C, "Bad", true)];
        p.change(
            B.parse().unwrap(),
            set(Some(Some(" Kitchen ")), None),
            &seen,
        )
        .unwrap();
        assert_eq!(p.alias(B), Some("Kitchen"));
        assert_eq!(
            p.change(C.parse().unwrap(), set(Some(Some("kitchen")), None), &seen)
                .unwrap_err(),
            Refused::Taken(B.into())
        );
        assert_eq!(
            p.change(C.parse().unwrap(), set(Some(Some("  ")), None), &seen)
                .unwrap_err(),
            Refused::Empty
        );
        // Sorted by the name shown.
        let names: Vec<String> = p
            .list(seen.clone())
            .iter()
            .map(|k| k.shown().into())
            .collect();
        assert_eq!(names, ["Bad", "Kitchen"]);
        p.change(B.parse().unwrap(), set(Some(None), None), &seen)
            .unwrap();
        assert_eq!(p.alias(B), None);
    }

    #[test]
    fn pinned_peers_stay_listed_across_restarts() {
        let (mut p, path) = peers();
        assert_eq!(
            p.change(B.parse().unwrap(), set(None, Some(true)), &[])
                .unwrap_err(),
            Refused::Unknown
        );
        p.change(
            B.parse().unwrap(),
            set(None, Some(true)),
            &[node(B, "Küche", true)],
        )
        .unwrap();
        // Renamed while online: remembered as it is now.
        p.remember(&[node(B, "Kochen", true)]);

        let p = Peers::load(path);
        let list = p.list(vec![node(C, "Bad", true)]);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].node.name, "Kochen");
        assert!(list[1].pinned && !list[1].node.online);
    }

    #[test]
    fn unpinned_peers_without_alias_are_forgotten() {
        let (mut p, path) = peers();
        let seen = vec![node(B, "Küche", true)];
        p.change(B.parse().unwrap(), set(None, Some(true)), &seen)
            .unwrap();
        p.change(B.parse().unwrap(), set(None, Some(false)), &seen)
            .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap().trim(), "{}");
    }
}
