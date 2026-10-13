//! NMOS resource models for Orpheus nodes.
//!
//! Resource IDs are UUIDv5 from the device ID, so they stay the same across
//! restarts. The [`Store`] gives each resource a new version whenever its
//! content changes and counts changes per resource type, which IS-04
//! peer-to-peer discovery announces in the `ver_*` TXT keys. [`sdp`] writes
//! the SDP files senders link to; [`is05`] has the parameters of the
//! Connection API.

pub mod is04;
pub mod is05;
pub mod sdp;

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use orpheus_core::DeviceId;
pub use uuid::Uuid;

use crate::is04::{Device, Flow, Node, Receiver, Resource, Sender, Source, Version};

/// Namespace of the UUIDv5 of all Orpheus resources.
const NAMESPACE: Uuid = uuid::uuid!("5d3ac68e-5484-4d6e-8565-226a31722206");

/// The ID of a device's resource, e.g. `node`, `device`, `sender/3`.
pub fn resource_id(device: DeviceId, path: &str) -> Uuid {
    Uuid::new_v5(&NAMESPACE, format!("{device}/{path}").as_bytes())
}

/// All resources of a node.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub node: Node,
    pub devices: Vec<Device>,
    pub sources: Vec<Source>,
    pub flows: Vec<Flow>,
    pub senders: Vec<Sender>,
    pub receivers: Vec<Receiver>,
}

/// How often each resource type changed, modulo 256: IS-04's `ver_slf`,
/// `ver_src`, `ver_flw`, `ver_dvc`, `ver_snd` and `ver_rcv`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub slf: u8,
    pub src: u8,
    pub flw: u8,
    pub dvc: u8,
    pub snd: u8,
    pub rcv: u8,
}

/// The current resources with their versions.
#[derive(Default)]
pub struct Store {
    /// Content hash (without the version) and version, by resource ID.
    stamps: HashMap<Uuid, (u64, Version)>,
    /// Resources that get a new version at the next update, changed or not.
    touched: HashSet<Uuid>,
    current: Option<Snapshot>,
    counters: Counters,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes a fresh snapshot, whose versions are ignored: resources keep
    /// their version unless their content changed, otherwise they get
    /// `now`. Returns whether anything changed.
    pub fn update(&mut self, mut snap: Snapshot, now: Version) -> bool {
        let prev = self.current.take();
        let ids = |f: fn(&Snapshot) -> Vec<Uuid>| prev.as_ref().map(f).unwrap_or_default();
        let c = &mut self.counters;
        let s = &mut Stamps {
            stamps: &mut self.stamps,
            touched: &mut self.touched,
        };
        let mut any = false;
        let mut bump = |counter: &mut u8, changed: bool| {
            if changed {
                *counter = counter.wrapping_add(1);
                any = true;
            }
        };
        bump(
            &mut c.slf,
            stamp(
                s,
                &ids(|p| vec![p.node.base.id]),
                std::slice::from_mut(&mut snap.node),
                now,
            ),
        );
        bump(
            &mut c.dvc,
            stamp(s, &ids(|p| ids_of(&p.devices)), &mut snap.devices, now),
        );
        bump(
            &mut c.src,
            stamp(s, &ids(|p| ids_of(&p.sources)), &mut snap.sources, now),
        );
        bump(
            &mut c.flw,
            stamp(s, &ids(|p| ids_of(&p.flows)), &mut snap.flows, now),
        );
        bump(
            &mut c.snd,
            stamp(s, &ids(|p| ids_of(&p.senders)), &mut snap.senders, now),
        );
        bump(
            &mut c.rcv,
            stamp(s, &ids(|p| ids_of(&p.receivers)), &mut snap.receivers, now),
        );
        self.current = Some(snap);
        any
    }

    /// Gives a resource a new version at the next update, e.g. after an
    /// activation that left its content as it was.
    pub fn touch(&mut self, id: Uuid) {
        self.touched.insert(id);
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.current.as_ref()
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }
}

struct Stamps<'a> {
    stamps: &'a mut HashMap<Uuid, (u64, Version)>,
    touched: &'a mut HashSet<Uuid>,
}

fn ids_of<T: Resource>(items: &[T]) -> Vec<Uuid> {
    items.iter().map(|r| r.base().id).collect()
}

/// Sets the versions of `items`; true if one was added, changed or removed
/// since `prev`.
fn stamp<T: Resource>(
    Stamps { stamps, touched }: &mut Stamps,
    prev: &[Uuid],
    items: &mut [T],
    now: Version,
) -> bool {
    let mut changed = items.len() != prev.len();
    for item in items.iter_mut() {
        item.base_mut().version = Version::default();
        let hash = content_hash(item);
        let id = item.base().id;
        let touched = touched.remove(&id);
        let version = match stamps.get(&id) {
            Some(&(h, v)) if h == hash && prev.contains(&id) && !touched => v,
            old => {
                changed = true;
                // Versions only ever grow, even if the clock steps back.
                let v = old.map_or(now, |&(_, v)| now.max(v.next()));
                stamps.insert(id, (hash, v));
                v
            }
        };
        item.base_mut().version = version;
    }
    changed
}

fn content_hash<T: Resource>(item: &T) -> u64 {
    let json = serde_json::to_string(item).expect("resources serialize");
    let mut h = DefaultHasher::new();
    json.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::is04::*;

    fn snapshot(senders: &[&str]) -> Snapshot {
        let dev = DeviceId::from_bits(42);
        let device_id = resource_id(dev, "device");
        Snapshot {
            node: Node {
                base: Base::new(resource_id(dev, "node"), "Kitchen"),
                href: "http://10.0.0.1:7480/".into(),
                hostname: None,
                api: Api {
                    versions: vec![API_VERSION.into()],
                    endpoints: vec![],
                },
                caps: Empty {},
                services: vec![],
                clocks: vec![Clock::Internal {
                    name: "clk0".into(),
                }],
                interfaces: vec![],
            },
            devices: vec![],
            sources: vec![],
            flows: vec![],
            senders: senders
                .iter()
                .enumerate()
                .map(|(i, label)| Sender {
                    base: Base::new(resource_id(dev, &format!("sender/{i}")), *label),
                    flow_id: None,
                    transport: TRANSPORT_RTP,
                    device_id,
                    manifest_href: None,
                    interface_bindings: vec![],
                    subscription: SenderSubscription {
                        receiver_id: None,
                        active: true,
                    },
                })
                .collect(),
            receivers: vec![],
        }
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = DeviceId::from_bits(1);
        assert_eq!(resource_id(a, "node"), resource_id(a, "node"));
        assert_ne!(resource_id(a, "node"), resource_id(a, "device"));
        assert_ne!(
            resource_id(a, "node"),
            resource_id(DeviceId::from_bits(2), "node")
        );
        assert_eq!(resource_id(a, "node").get_version_num(), 5);
    }

    #[test]
    fn versions_change_with_content() {
        let mut store = Store::new();
        let t = |n: u64| Version::from_tai_nanos(n * 1_000_000_000);
        assert!(store.update(snapshot(&["a"]), t(1)));
        assert_eq!(store.counters().snd, 1);
        assert_eq!(store.counters().slf, 1);

        // Unchanged: same versions, no counters.
        assert!(!store.update(snapshot(&["a"]), t(2)));
        let s = store.snapshot().unwrap();
        assert_eq!(s.senders[0].base.version, t(1));
        assert_eq!(store.counters().snd, 1);

        // A new sender: only it gets the new version.
        assert!(store.update(snapshot(&["a", "b"]), t(3)));
        let s = store.snapshot().unwrap();
        assert_eq!(s.senders[0].base.version, t(1));
        assert_eq!(s.senders[1].base.version, t(3));
        assert_eq!(store.counters().snd, 2);
        assert_eq!(store.counters().slf, 1);

        // Removed, then back: a newer version, even with the clock behind.
        assert!(store.update(snapshot(&["a"]), t(4)));
        assert!(store.update(snapshot(&["a", "b"]), t(0)));
        let s = store.snapshot().unwrap();
        assert!(s.senders[1].base.version > t(3));
        assert_eq!(store.counters().snd, 4);

        // Touched: a new version without a change.
        store.touch(resource_id(DeviceId::from_bits(42), "sender/0"));
        assert!(store.update(snapshot(&["a", "b"]), t(9)));
        let s = store.snapshot().unwrap();
        assert_eq!(s.senders[0].base.version, t(9));
        assert!(!store.update(snapshot(&["a", "b"]), t(10)));
    }

    #[test]
    fn serializes_as_nmos() {
        let mut store = Store::new();
        store.update(snapshot(&["a"]), Version::from_tai_nanos(1_500_000_007));
        let s = store.snapshot().unwrap();
        let json = serde_json::to_value(&s.senders[0]).unwrap();
        assert_eq!(json["version"], "1:500000007");
        assert_eq!(json["label"], "a");
        assert_eq!(json["tags"], serde_json::json!({}));
        assert_eq!(json["manifest_href"], serde_json::Value::Null);
        let node = serde_json::to_value(&s.node).unwrap();
        assert_eq!(node["clocks"][0]["ref_type"], "internal");
        assert_eq!(node["caps"], serde_json::json!({}));
        assert!(node.get("hostname").is_none());
        assert_eq!(mac([0, 0x1b, 0x21, 0xab, 0xcd, 0xef]), "00-1b-21-ab-cd-ef");
    }
}
