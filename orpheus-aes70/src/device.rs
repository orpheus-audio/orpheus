//! A device as controllers see it: the managers every device has, a tree
//! of blocks holding gains, mutes and level meters, and the subscriptions
//! of each connection.
//!
//! The host builds the [`Tree`] from its own state and replaces it
//! whenever that changes; [`Device::set_tree`] tells which properties
//! changed, and each [`Session`] turns those into notifications for what
//! its controller subscribed to. A controller's attempt to change a gain
//! or a mute comes back to the host as a [`Set`] to carry out, so that it
//! goes the way any other change does.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::ocp1::{Command, Event, Id, Outgoing, Reader, Status, Writer};

/// The root block's object number.
pub const ROOT: u32 = 100;
/// Object numbers of the managers.
pub const DEVICE_MANAGER: u32 = 1;
pub const FIRMWARE_MANAGER: u32 = 3;
pub const SUBSCRIPTION_MANAGER: u32 = 4;
/// Object numbers below this are the device's own; the host numbers its
/// objects from here.
pub const FIRST_ONO: u32 = 4096;

/// The AES70 version the classes are from: AES70-2023.
const OCA_VERSION: u16 = 3;
/// The version of every class here.
const CLASS_VERSION: u16 = 3;
/// The event every object emits when one of its properties changes.
const PROPERTY_CHANGED: Id = Id(1, 1);
/// The longest subscriber context taken.
const MAX_CONTEXT: usize = 256;

/// Class IDs.
const BLOCK: &[u16] = &[1, 1, 3];
const GAIN: &[u16] = &[1, 1, 1, 5];
const MUTE: &[u16] = &[1, 1, 1, 2];
const AUDIO_LEVEL_SENSOR: &[u16] = &[1, 1, 2, 2, 1];
const DEVICE_MANAGER_CLASS: &[u16] = &[1, 3, 1];
const FIRMWARE_MANAGER_CLASS: &[u16] = &[1, 3, 3];
const SUBSCRIPTION_MANAGER_CLASS: &[u16] = &[1, 3, 4];

/// Properties.
const ROLE: Id = Id(1, 5);
const LABEL: Id = Id(2, 3);
const MEMBERS: Id = Id(3, 2);
const GAIN_VALUE: Id = Id(4, 1);
const MUTE_STATE: Id = Id(4, 1);
const READING_STATE: Id = Id(3, 1);
const READING: Id = Id(4, 1);
const DEVICE_NAME: Id = Id(3, 4);

/// What the device says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Identity {
    /// The name its user gave it.
    pub name: String,
    /// What tells it from every other device of its kind.
    pub serial: String,
    pub manufacturer: String,
    pub website: String,
    /// The product's name.
    pub product: String,
    /// The software's version, such as `0.0.1`.
    pub version: String,
}

/// How a level meter measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Law {
    Rms = 6,
    Peak = 7,
}

/// What an object does.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// Holds other objects.
    Block,
    /// A gain in dB, from `min` to `max`.
    Gain { db: f32, min: f32, max: f32 },
    /// Whether it is muted.
    Mute(bool),
    /// A level meter, in dBFS from `min` to `max`; `None` while silent.
    Level {
        dbfs: Option<f32>,
        min: f32,
        max: f32,
        law: Law,
    },
}

/// An object of the tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    /// Its name among its block's members, for programs.
    pub role: String,
    /// Its name for people.
    pub label: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
struct Node {
    object: Object,
    /// The block it is in; 0 for the root.
    owner: u32,
    members: Vec<u32>,
}

/// The device's blocks and what is in them.
#[derive(Debug, Clone, PartialEq)]
pub struct Tree {
    nodes: BTreeMap<u32, Node>,
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    /// An empty root block.
    pub fn new() -> Self {
        let root = Node {
            object: Object {
                role: "Root".into(),
                label: String::new(),
                kind: Kind::Block,
            },
            owner: 0,
            members: Vec::new(),
        };
        Self {
            nodes: BTreeMap::from([(ROOT, root)]),
        }
    }

    /// Adds `object` as `ono` to block `owner`; `false`, leaving the tree
    /// as it was, if `ono` is taken or `owner` is no block.
    pub fn add(&mut self, owner: u32, ono: u32, object: Object) -> bool {
        if ono < FIRST_ONO || self.nodes.contains_key(&ono) {
            return false;
        }
        match self.nodes.get_mut(&owner) {
            Some(block) if block.object.kind == Kind::Block => block.members.push(ono),
            _ => return false,
        }
        self.nodes.insert(
            ono,
            Node {
                object,
                owner,
                members: Vec::new(),
            },
        );
        true
    }

    pub fn get(&self, ono: u32) -> Option<&Object> {
        self.nodes.get(&ono).map(|n| &n.object)
    }

    /// The blocks `ono` is in, from the root on.
    fn container_path(&self, ono: u32) -> Vec<u32> {
        let mut path = Vec::new();
        let mut at = self.nodes.get(&ono).map_or(0, |n| n.owner);
        while let Some(n) = self.nodes.get(&at) {
            path.push(at);
            at = n.owner;
        }
        path.reverse();
        path
    }

    /// The members of block `ono` and theirs, each block before its
    /// members.
    fn members_recursive(&self, ono: u32, out: &mut Vec<(u32, u32)>) {
        for &m in self.nodes.get(&ono).map_or(&[][..], |n| &n.members) {
            out.push((m, ono));
            self.members_recursive(m, out);
        }
    }
}

fn class(kind: &Kind) -> &'static [u16] {
    match kind {
        Kind::Block => BLOCK,
        Kind::Gain { .. } => GAIN,
        Kind::Mute(_) => MUTE,
        Kind::Level { .. } => AUDIO_LEVEL_SENSOR,
    }
}

/// A change a controller asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Set {
    Gain(f32),
    Mute(bool),
}

/// A property of an object that changed, and its new value.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub ono: u32,
    pub property: Id,
    /// Encoded.
    pub value: Vec<u8>,
}

/// What became of a command.
#[derive(Debug, Clone, PartialEq)]
pub enum Handled {
    /// Send this; `None` where the controller wants no response.
    Reply(Option<Outgoing>),
    /// The controller asks to change object `ono`. The host does it, then
    /// answers with [`Outgoing::status`] if `response`.
    Set {
        ono: u32,
        set: Set,
        handle: u32,
        response: bool,
    },
}

/// The device: its identity and its tree.
#[derive(Debug, Clone)]
pub struct Device {
    identity: Identity,
    tree: Tree,
}

/// What a method returns: how many values, encoded.
type Values = (u8, Vec<u8>);

fn values(count: u8, f: impl FnOnce(&mut Writer)) -> Result<Values, Status> {
    let mut w = Writer::new();
    f(&mut w);
    Ok((count, w.into_bytes()))
}

impl Device {
    pub fn new(identity: Identity, tree: Tree) -> Self {
        Self { identity, tree }
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Replaces the identity; the changes to notify.
    pub fn set_identity(&mut self, identity: Identity) -> Vec<Change> {
        let mut changes = Vec::new();
        if identity.name != self.identity.name {
            let mut w = Writer::new();
            w.string(&identity.name);
            changes.push(Change {
                ono: DEVICE_MANAGER,
                property: DEVICE_NAME,
                value: w.into_bytes(),
            });
        }
        self.identity = identity;
        changes
    }

    /// Replaces the tree; the changes to notify. Objects that went away
    /// show as changed members of their block.
    pub fn set_tree(&mut self, tree: Tree) -> Vec<Change> {
        let mut changes = Vec::new();
        for (&ono, node) in &tree.nodes {
            let Some(old) = self.tree.nodes.get(&ono) else {
                continue;
            };
            if class(&old.object.kind) != class(&node.object.kind) {
                continue;
            }
            let before = self.properties(&self.tree, ono, old);
            for (property, value) in self.properties(&tree, ono, node) {
                if before.iter().all(|(p, v)| *p != property || *v != value) {
                    changes.push(Change {
                        ono,
                        property,
                        value,
                    });
                }
            }
        }
        self.tree = tree;
        changes
    }

    /// The properties of `node` that notify when they change.
    fn properties(&self, tree: &Tree, ono: u32, node: &Node) -> Vec<(Id, Vec<u8>)> {
        let encode = |f: &dyn Fn(&mut Writer)| {
            let mut w = Writer::new();
            f(&mut w);
            w.into_bytes()
        };
        let o = &node.object;
        let mut props = Vec::from([
            (ROLE, encode(&|w| _ = w.string(&o.role))),
            (LABEL, encode(&|w| _ = w.string(&o.label))),
        ]);
        match &o.kind {
            Kind::Block => props.push((MEMBERS, encode(&|w| self.write_members(w, tree, ono)))),
            Kind::Gain { db, .. } => props.push((GAIN_VALUE, encode(&|w| _ = w.f32(*db)))),
            Kind::Mute(muted) => {
                props.push((MUTE_STATE, encode(&|w| _ = w.u8(mute_state(*muted)))))
            }
            Kind::Level { dbfs, min, max, .. } => {
                let (reading, state) = reading(*dbfs, *min, *max);
                props.push((READING_STATE, encode(&|w| _ = w.u8(state))));
                props.push((READING, encode(&|w| _ = w.f32(reading))));
            }
        }
        props
    }

    /// An `OcaList<OcaObjectIdentification>` of block `ono`'s members.
    fn write_members(&self, w: &mut Writer, tree: &Tree, ono: u32) {
        let members = tree.nodes.get(&ono).map_or(&[][..], |n| &n.members);
        w.count(members.len());
        for m in members {
            w.u32(*m)
                .class(class(&tree.nodes[m].object.kind), CLASS_VERSION);
        }
    }

    /// Carries out `cmd` as far as the device can; subscriptions go to
    /// `session`.
    pub fn handle(&self, session: &mut Session, cmd: &Command) -> Handled {
        let mut r = Reader::new(&cmd.params);
        let result = match cmd.target {
            DEVICE_MANAGER => self.device_manager(cmd.method),
            FIRMWARE_MANAGER => self.firmware_manager(cmd.method),
            SUBSCRIPTION_MANAGER => session.call(self, cmd.method, &mut r),
            ono => match self.tree.nodes.get(&ono) {
                Some(node) => match self.object(ono, node, cmd.method, &mut r) {
                    Err(Call::Set(set)) => {
                        return Handled::Set {
                            ono,
                            set,
                            handle: cmd.handle,
                            response: cmd.response,
                        };
                    }
                    Err(Call::Status(s)) => Err(s),
                    Ok(v) => Ok(v),
                },
                None => Err(Status::BadONo),
            },
        };
        if !cmd.response {
            return Handled::Reply(None);
        }
        Handled::Reply(Some(match result {
            Ok((count, params)) => Outgoing::Response {
                handle: cmd.handle,
                status: Status::Ok,
                count,
                params,
            },
            Err(status) => Outgoing::status(cmd.handle, status),
        }))
    }

    /// `OcaRoot`'s methods, which every object has.
    fn root(role: &str, class: &[u16], method: Id) -> Result<Values, Status> {
        match method {
            Id(1, 1) => values(1, |w| _ = w.class(class, CLASS_VERSION)),
            // Lockable: no.
            Id(1, 2) => values(1, |w| _ = w.bool(false)),
            Id(1, 5) => values(1, |w| _ = w.string(role)),
            // The lock state: no lock.
            Id(1, 7) => values(1, |w| _ = w.u8(0)),
            m => Err(unknown(m, class)),
        }
    }

    fn device_manager(&self, method: Id) -> Result<Values, Status> {
        let id = &self.identity;
        match method {
            Id(3, 1) => values(1, |w| _ = w.u16(OCA_VERSION)),
            // The model GUID: no manufacturer code.
            Id(3, 2) => values(1, |w| _ = w.bytes(&[0; 8])),
            Id(3, 3) => values(1, |w| _ = w.string(&id.serial)),
            Id(3, 4) => values(1, |w| _ = w.string(&id.name)),
            Id(3, 6) => values(1, |w| {
                w.string(&id.manufacturer)
                    .string(&id.product)
                    .string(&id.version);
            }),
            // The device role and the user inventory code: none.
            Id(3, 7) | Id(3, 9) | Id(3, 17) => values(1, |w| _ = w.string("")),
            // Enabled.
            Id(3, 11) => values(1, |w| _ = w.bool(true)),
            // The state: operational.
            Id(3, 13) => values(1, |w| _ = w.u16(1)),
            // Why it last reset: it was switched on.
            Id(3, 15) => values(1, |w| _ = w.u8(0)),
            Id(3, 19) => values(1, |w| {
                let managers = [
                    (DEVICE_MANAGER, "DeviceManager", DEVICE_MANAGER_CLASS),
                    (FIRMWARE_MANAGER, "FirmwareManager", FIRMWARE_MANAGER_CLASS),
                    (
                        SUBSCRIPTION_MANAGER,
                        "SubscriptionManager",
                        SUBSCRIPTION_MANAGER_CLASS,
                    ),
                ];
                w.count(managers.len());
                for (ono, name, class) in managers {
                    w.u32(ono).string(name).class(class, CLASS_VERSION);
                }
            }),
            Id(3, 20) => values(1, |w| _ = w.string(&id.version)),
            Id(3, 21) => values(1, |w| {
                // No organization ID; no contacts.
                w.string(&id.manufacturer)
                    .bytes(&[0; 3])
                    .string(&id.website)
                    .string("")
                    .string("");
            }),
            Id(3, 22) => values(1, |w| {
                w.string(&id.product)
                    .string(&id.product)
                    .string(&id.version)
                    .string(&id.manufacturer)
                    .string("")
                    .string("");
            }),
            // Normal operation, no details.
            Id(3, 23) => values(1, |w| _ = w.u8(0).blob(&[])),
            m => Self::root("DeviceManager", DEVICE_MANAGER_CLASS, m),
        }
    }

    fn firmware_manager(&self, method: Id) -> Result<Values, Status> {
        match method {
            Id(3, 1) => values(1, |w| {
                let mut parts = self.identity.version.split(['.', '-', '+']);
                let mut part = || parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
                let (major, minor, build) = (part(), part(), part());
                // One component, the software; 0 would be the boot loader.
                w.count(1).u32(major).u32(minor).u32(build).u16(1);
            }),
            m => Self::root("FirmwareManager", FIRMWARE_MANAGER_CLASS, m),
        }
    }

    /// The methods of an object of the tree.
    fn object(&self, ono: u32, node: &Node, method: Id, r: &mut Reader) -> Result<Values, Call> {
        let o = &node.object;
        let class = class(&o.kind);
        let v = match (method, &o.kind) {
            // OcaWorker
            (Id(2, 1), _) => values(1, |w| _ = w.bool(true)),
            (Id(2, 5), _) => values(1, |w| _ = w.count(0)),
            (Id(2, 8), _) => values(1, |w| _ = w.string(&o.label)),
            (Id(2, 10), _) => values(1, |w| _ = w.u32(node.owner)),
            (Id(2, 13), _) => {
                let mut path = self.tree.container_path(ono);
                path.push(ono);
                // The root block isn't part of a path.
                path.retain(|&p| p != ROOT);
                values(2, |w| {
                    w.count(path.len());
                    for p in &path {
                        w.string(&self.tree.nodes[p].object.role);
                    }
                    w.count(path.len());
                    for p in &path {
                        w.u32(*p);
                    }
                })
            }
            // OcaBlock
            (Id(3, 1), Kind::Block) => values(1, |w| _ = w.u32(1)),
            (Id(3, 5), Kind::Block) => values(1, |w| self.write_members(w, &self.tree, ono)),
            (Id(3, 6), Kind::Block) => {
                let mut members = Vec::new();
                self.tree.members_recursive(ono, &mut members);
                values(1, |w| {
                    w.count(members.len());
                    for (m, owner) in members {
                        let class = class_of(&self.tree, m);
                        w.u32(m).class(class, CLASS_VERSION).u32(owner);
                    }
                })
            }
            // No signal paths.
            (Id(3, 9) | Id(3, 10), Kind::Block) => values(1, |w| _ = w.count(0)),
            (Id(3, 17..=19), Kind::Block) => self.find(ono, method, r),
            (Id(3, 20), Kind::Block) => self.find_by_path(ono, r),
            // Nothing can be added to or taken from a block: its members
            // are what the host has.
            (Id(3, 21), Kind::Block) => values(1, |w| _ = w.u16(0)),
            // OcaGain
            (Id(4, 1), Kind::Gain { db, min, max }) => {
                values(3, |w| _ = w.f32(*db).f32(*min).f32(*max))
            }
            (Id(4, 2), Kind::Gain { min, max, .. }) => {
                let db = r.f32()?;
                if !r.is_empty() {
                    return Err(Status::BadFormat.into());
                }
                if !(*min..=*max).contains(&db) {
                    return Err(Status::ParameterOutOfRange.into());
                }
                return Err(Call::Set(Set::Gain(db)));
            }
            // OcaMute
            (Id(4, 1), Kind::Mute(muted)) => values(1, |w| _ = w.u8(mute_state(*muted))),
            (Id(4, 2), Kind::Mute(_)) => {
                let muted = match r.u8()? {
                    1 => true,
                    2 => false,
                    _ => return Err(Status::ParameterOutOfRange.into()),
                };
                if !r.is_empty() {
                    return Err(Status::BadFormat.into());
                }
                return Err(Call::Set(Set::Mute(muted)));
            }
            // OcaSensor, OcaLevelSensor, OcaAudioLevelSensor
            (
                Id(3, 1) | Id(4, 1) | Id(5, 1),
                Kind::Level {
                    dbfs,
                    min,
                    max,
                    law,
                },
            ) => {
                let (reading, state) = reading(*dbfs, *min, *max);
                match method.0 {
                    3 => values(1, |w| _ = w.u8(state)),
                    4 => values(3, |w| _ = w.f32(reading).f32(*min).f32(*max)),
                    _ => values(1, |w| _ = w.u8(*law as u8)),
                }
            }
            (m, _) => Self::root(&o.role, class, m),
        };
        v.map_err(Call::Status)
    }

    /// `FindActionObjectsByRole`, `…ByRoleRecursive` and
    /// `…ByLabelRecursive`.
    fn find(&self, ono: u32, method: Id, r: &mut Reader) -> Result<Values, Status> {
        let text = r.string()?;
        let comparison = r.u8()?;
        let class = class_id(r)?;
        let _flags = r.u16()?;
        if !r.is_empty() {
            return Err(Status::BadFormat);
        }
        let mut members = Vec::new();
        if method == Id(3, 17) {
            let direct = self.tree.nodes.get(&ono).map_or(&[][..], |n| &n.members);
            members.extend(direct.iter().map(|&m| (m, ono)));
        } else {
            self.tree.members_recursive(ono, &mut members);
        }
        let mut found = Vec::new();
        for (m, _) in members {
            let o = &self.tree.nodes[&m].object;
            let name = if method == Id(3, 19) {
                &o.label
            } else {
                &o.role
            };
            if compare(comparison, name, &text)? && self::class(&o.kind).starts_with(&class) {
                found.push(m);
            }
        }
        values(1, |w| self.write_results(w, &found))
    }

    /// `FindActionObjectsByRolePath`: the object whose roles from block
    /// `ono` down are the path.
    fn find_by_path(&self, ono: u32, r: &mut Reader) -> Result<Values, Status> {
        let n = r.count()?;
        let mut at = ono;
        for _ in 0..n {
            let role = r.string()?;
            let members = self.tree.nodes.get(&at).map_or(&[][..], |n| &n.members);
            match members
                .iter()
                .find(|&&m| self.tree.nodes[&m].object.role == role)
            {
                Some(&m) => at = m,
                None => {
                    at = 0;
                    break;
                }
            }
        }
        let _flags = r.u16()?;
        let found: Vec<u32> = (at != 0 && n > 0).then_some(at).into_iter().collect();
        values(1, |w| self.write_results(w, &found))
    }

    /// An `OcaList<OcaActionObjectSearchResult>`, with every field.
    fn write_results(&self, w: &mut Writer, found: &[u32]) {
        w.count(found.len());
        for &m in found {
            let o = &self.tree.nodes[&m].object;
            w.u32(m).class(class(&o.kind), CLASS_VERSION);
            let path = self.tree.container_path(m);
            w.count(path.len());
            for p in path {
                w.u32(p);
            }
            w.string(&o.role).string(&o.label);
        }
    }
}

fn class_of(tree: &Tree, ono: u32) -> &'static [u16] {
    class(&tree.nodes[&ono].object.kind)
}

/// Why a method of an object returned no values.
enum Call {
    Status(Status),
    Set(Set),
}

impl From<Status> for Call {
    fn from(s: Status) -> Self {
        Call::Status(s)
    }
}

impl From<crate::ocp1::Error> for Call {
    fn from(e: crate::ocp1::Error) -> Self {
        Call::Status(e.into())
    }
}

/// The status for a method the device doesn't have: one of the class not
/// implemented, else not a method of it.
fn unknown(method: Id, class: &[u16]) -> Status {
    if usize::from(method.0) <= class.len() {
        Status::NotImplemented
    } else {
        Status::BadMethod
    }
}

fn mute_state(muted: bool) -> u8 {
    if muted { 1 } else { 2 }
}

/// A level meter's reading and its state: below its range while silent.
fn reading(dbfs: Option<f32>, min: f32, max: f32) -> (f32, u8) {
    match dbfs {
        Some(db) if db < min => (min, 2),
        Some(db) if db > max => (max, 3),
        Some(db) => (db, 1),
        None => (min, 2),
    }
}

/// A class ID as a parameter, `String16` in AES70.js's terms.
fn class_id(r: &mut Reader) -> Result<Vec<u16>, Status> {
    let n = r.count()?;
    (0..n).map(|_| r.u16().map_err(Into::into)).collect()
}

/// Whether `name` matches `text` the `OcaStringComparisonType` way.
fn compare(comparison: u8, name: &str, text: &str) -> Result<bool, Status> {
    let lower = |s: &str| s.to_lowercase();
    Ok(match comparison {
        0 => name == text,
        1 => name.starts_with(text),
        2 => name.contains(text),
        3 => lower(name) == lower(text),
        4 => lower(name).starts_with(&lower(text)),
        5 => lower(name).contains(&lower(text)),
        _ => return Err(Status::ParameterOutOfRange),
    })
}

/// How a controller hears of an event.
#[derive(Debug, Clone, PartialEq)]
enum Delivery {
    /// `AddSubscription`: by a call of its method, with its context.
    Method {
        target: u32,
        method: Id,
        context: Vec<u8>,
    },
    /// `AddSubscription2`.
    Event2,
}

#[derive(Debug, Clone, PartialEq)]
struct Subscription {
    event: Event,
    /// Only changes of this property.
    property: Option<Id>,
    delivery: Delivery,
}

/// A controller's connection: what it subscribed to.
#[derive(Debug, Clone)]
pub struct Session {
    subscriptions: Vec<Subscription>,
    /// False while the controller disabled notifications.
    enabled: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            subscriptions: Vec::new(),
            enabled: true,
        }
    }
}

/// The only delivery mode: over the controller's connection.
const RELIABLE: u8 = 1;

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether it subscribed to anything of object `ono`.
    pub fn watches(&self, ono: u32) -> bool {
        self.subscriptions.iter().any(|s| s.event.emitter == ono)
    }

    /// The notifications `changes` make for this controller.
    pub fn notify(&self, changes: &[Change]) -> Vec<Outgoing> {
        if !self.enabled {
            return Vec::new();
        }
        let mut out = Vec::new();
        for c in changes {
            let event = Event {
                emitter: c.ono,
                id: PROPERTY_CHANGED,
            };
            // OcaPropertyChangedEventData: the property, its value, and
            // that the value changed.
            let mut w = Writer::new();
            w.id(c.property).bytes(&c.value).u8(1);
            let data = w.into_bytes();
            for s in &self.subscriptions {
                if s.event != event || s.property.is_some_and(|p| p != c.property) {
                    continue;
                }
                out.push(match &s.delivery {
                    Delivery::Method {
                        target,
                        method,
                        context,
                    } => Outgoing::Notification {
                        target: *target,
                        method: *method,
                        context: context.clone(),
                        event,
                        data: data.clone(),
                    },
                    Delivery::Event2 => Outgoing::Notification2 {
                        event,
                        data: data.clone(),
                    },
                });
            }
        }
        out
    }

    fn add(&mut self, s: Subscription) {
        if !self.subscriptions.contains(&s) {
            self.subscriptions.push(s);
        }
    }

    fn remove(&mut self, event: Event, property: Option<Id>, method: Option<(u32, Id)>) {
        self.subscriptions.retain(|s| {
            let same_delivery = match (&s.delivery, method) {
                (Delivery::Method { target, method, .. }, Some(m)) => (*target, *method) == m,
                (Delivery::Event2, None) => true,
                _ => false,
            };
            !(s.event == event && s.property == property && same_delivery)
        });
    }

    /// `OcaSubscriptionManager`'s methods.
    fn call(&mut self, device: &Device, method: Id, r: &mut Reader) -> Result<Values, Status> {
        let exists = |ono: u32| {
            matches!(
                ono,
                DEVICE_MANAGER | FIRMWARE_MANAGER | SUBSCRIPTION_MANAGER
            ) || device.tree.nodes.contains_key(&ono)
        };
        let method_of = |r: &mut Reader| -> Result<(u32, Id), Status> { Ok((r.u32()?, r.id()?)) };
        let context = |r: &mut Reader| -> Result<Vec<u8>, Status> {
            let c = r.blob()?;
            if c.len() > MAX_CONTEXT {
                return Err(Status::ParameterOutOfRange);
            }
            Ok(c.into())
        };
        // The delivery mode and its destination, which only the
        // lightweight mode (over UDP) needs.
        let delivery = |r: &mut Reader| -> Result<(), Status> {
            if r.u8()? != RELIABLE {
                return Err(Status::NotImplemented);
            }
            r.blob()?;
            Ok(())
        };
        let done = |r: &Reader| {
            if r.is_empty() {
                Ok((0, Vec::new()))
            } else {
                Err(Status::BadFormat)
            }
        };
        match method {
            // AddSubscription
            Id(3, 1) => {
                let event = r.event()?;
                let (target, m) = method_of(r)?;
                let context = context(r)?;
                delivery(r)?;
                if !exists(event.emitter) {
                    return Err(Status::BadONo);
                }
                self.add(Subscription {
                    event,
                    property: None,
                    delivery: Delivery::Method {
                        target,
                        method: m,
                        context,
                    },
                });
                done(r)
            }
            // RemoveSubscription
            Id(3, 2) => {
                let event = r.event()?;
                let m = method_of(r)?;
                self.remove(event, None, Some(m));
                done(r)
            }
            // DisableNotifications, ReEnableNotifications
            Id(3, 3) | Id(3, 4) => {
                self.enabled = method == Id(3, 4);
                done(r)
            }
            // AddPropertyChangeSubscription
            Id(3, 5) => {
                let emitter = r.u32()?;
                let property = r.id()?;
                let (target, m) = method_of(r)?;
                let context = context(r)?;
                delivery(r)?;
                if !exists(emitter) {
                    return Err(Status::BadONo);
                }
                self.add(Subscription {
                    event: Event {
                        emitter,
                        id: PROPERTY_CHANGED,
                    },
                    property: Some(property),
                    delivery: Delivery::Method {
                        target,
                        method: m,
                        context,
                    },
                });
                done(r)
            }
            // RemovePropertyChangeSubscription
            Id(3, 6) => {
                let emitter = r.u32()?;
                let property = r.id()?;
                let m = method_of(r)?;
                let event = Event {
                    emitter,
                    id: PROPERTY_CHANGED,
                };
                self.remove(event, Some(property), Some(m));
                done(r)
            }
            // GetMaximumSubscriberContextLength
            Id(3, 7) => values(1, |w| _ = w.u16(MAX_CONTEXT as u16)),
            // AddSubscription2, RemoveSubscription2
            Id(3, 8) | Id(3, 9) => {
                let event = r.event()?;
                delivery(r)?;
                if method == Id(3, 9) {
                    self.remove(event, None, None);
                } else if exists(event.emitter) {
                    self.add(Subscription {
                        event,
                        property: None,
                        delivery: Delivery::Event2,
                    });
                } else {
                    return Err(Status::BadONo);
                }
                done(r)
            }
            // AddPropertyChangeSubscription2, RemovePropertyChangeSubscription2
            Id(3, 10) | Id(3, 11) => {
                let emitter = r.u32()?;
                let property = r.id()?;
                delivery(r)?;
                let event = Event {
                    emitter,
                    id: PROPERTY_CHANGED,
                };
                if method == Id(3, 11) {
                    self.remove(event, Some(property), None);
                } else if exists(emitter) {
                    self.add(Subscription {
                        event,
                        property: Some(property),
                        delivery: Delivery::Event2,
                    });
                } else {
                    return Err(Status::BadONo);
                }
                done(r)
            }
            // AddSubscription2List, RemoveSubscription2List
            Id(3, 12) | Id(3, 13) => {
                let n = r.count()?;
                let events = (0..n).map(|_| r.event()).collect::<Result<Vec<_>, _>>()?;
                delivery(r)?;
                if !r.is_empty() {
                    return Err(Status::BadFormat);
                }
                if method == Id(3, 13) {
                    for e in events {
                        self.remove(e, None, None);
                    }
                    return Ok((0, Vec::new()));
                }
                let statuses: Vec<Status> = events
                    .into_iter()
                    .map(|event| {
                        if !exists(event.emitter) {
                            return Status::BadONo;
                        }
                        self.add(Subscription {
                            event,
                            property: None,
                            delivery: Delivery::Event2,
                        });
                        Status::Ok
                    })
                    .collect();
                values(1, |w| {
                    w.count(statuses.len());
                    for s in statuses {
                        w.u8(s as u8);
                    }
                })
            }
            m => Device::root("SubscriptionManager", SUBSCRIPTION_MANAGER_CLASS, m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn device() -> Device {
        let mut tree = Tree::new();
        let block = |role: &str| Object {
            role: role.into(),
            label: "Speakers".into(),
            kind: Kind::Block,
        };
        assert!(tree.add(ROOT, 4096, block("output")));
        assert!(tree.add(
            4096,
            4097,
            Object {
                role: "gain".into(),
                label: "Gain".into(),
                kind: Kind::Gain {
                    db: -6.0,
                    min: -90.0,
                    max: 12.0,
                },
            },
        ));
        assert!(tree.add(
            4096,
            4098,
            Object {
                role: "level.1".into(),
                label: "Left".into(),
                kind: Kind::Level {
                    dbfs: None,
                    min: -90.0,
                    max: 0.0,
                    law: Law::Peak,
                },
            },
        ));
        // Taken, or not in a block.
        assert!(!tree.add(ROOT, 4097, block("again")));
        assert!(!tree.add(4097, 4099, block("in a gain")));
        Device::new(
            Identity {
                name: "Kitchen".into(),
                ..Identity::default()
            },
            tree,
        )
    }

    fn call(d: &Device, s: &mut Session, target: u32, method: Id, params: Vec<u8>) -> Handled {
        d.handle(
            s,
            &Command {
                handle: 1,
                target,
                method,
                params,
                response: true,
            },
        )
    }

    fn reply(h: Handled) -> (Status, Vec<u8>) {
        match h {
            Handled::Reply(Some(Outgoing::Response { status, params, .. })) => (status, params),
            h => panic!("{h:?}"),
        }
    }

    fn f32s(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_be_bytes()).collect()
    }

    #[test]
    fn answers_for_objects_and_managers() {
        let d = device();
        let mut s = Session::new();
        let (status, params) = reply(call(&d, &mut s, ROOT, Id(1, 1), vec![]));
        assert_eq!(
            (status, params),
            (Status::Ok, vec![0, 3, 0, 1, 0, 1, 0, 3, 0, 3])
        );
        let (_, params) = reply(call(&d, &mut s, 4097, Id(4, 1), vec![]));
        assert_eq!(params, f32s(&[-6.0, -90.0, 12.0]));
        // A silent meter reads its lowest value, below its range.
        let (_, params) = reply(call(&d, &mut s, 4098, Id(4, 1), vec![]));
        assert_eq!(params, f32s(&[-90.0, -90.0, 0.0]));
        assert_eq!(reply(call(&d, &mut s, 4098, Id(3, 1), vec![])).1, vec![2]);
        let (_, params) = reply(call(&d, &mut s, DEVICE_MANAGER, Id(3, 4), vec![]));
        assert_eq!(params, b"\0\x07Kitchen");
        assert_eq!(
            reply(call(&d, &mut s, 9999, Id(1, 1), vec![])).0,
            Status::BadONo
        );
        assert_eq!(
            reply(call(&d, &mut s, 4097, Id(2, 12), vec![])).0,
            Status::NotImplemented
        );
        assert_eq!(
            reply(call(&d, &mut s, 4097, Id(5, 1), vec![])).0,
            Status::BadMethod
        );
    }

    #[test]
    fn lists_members_recursively() {
        let d = device();
        let (_, params) = reply(call(&d, &mut Session::new(), ROOT, Id(3, 6), vec![]));
        let mut r = Reader::new(&params);
        assert_eq!(r.count(), Ok(3));
        let mut owners = Vec::new();
        for _ in 0..3 {
            let ono = r.u32().unwrap();
            let n = r.count().unwrap();
            r.bytes(2 * n + 2).unwrap();
            owners.push((ono, r.u32().unwrap()));
        }
        assert_eq!(owners, [(4096, ROOT), (4097, 4096), (4098, 4096)]);
    }

    #[test]
    fn finds_by_role_and_path() {
        let d = device();
        let mut w = Writer::new();
        // Role "GAIN", case-insensitive, any class, all fields.
        w.string("GAIN").u8(3).count(0).u16(0x1F);
        let (_, params) = reply(call(
            &d,
            &mut Session::new(),
            ROOT,
            Id(3, 18),
            w.into_bytes(),
        ));
        assert_eq!(&params[..6], &[0, 1, 0, 0, 0x10, 0x01]);
        let mut w = Writer::new();
        w.count(2).string("output").string("level.1").u16(1);
        let (_, params) = reply(call(
            &d,
            &mut Session::new(),
            ROOT,
            Id(3, 20),
            w.into_bytes(),
        ));
        assert_eq!(&params[..6], &[0, 1, 0, 0, 0x10, 0x02]);
    }

    #[test]
    fn hands_sets_to_the_host() {
        let d = device();
        let mut s = Session::new();
        let set = call(&d, &mut s, 4097, Id(4, 2), f32s(&[-3.0]));
        assert_eq!(
            set,
            Handled::Set {
                ono: 4097,
                set: Set::Gain(-3.0),
                handle: 1,
                response: true,
            }
        );
        let too_loud = call(&d, &mut s, 4097, Id(4, 2), f32s(&[20.0]));
        assert_eq!(reply(too_loud).0, Status::ParameterOutOfRange);
    }

    #[test]
    fn notifies_subscribers_of_changes() {
        let mut d = device();
        let mut s = Session::new();
        let mut w = Writer::new();
        // AddSubscription2 to the gain's PropertyChanged.
        w.u32(4097).id(PROPERTY_CHANGED).u8(RELIABLE).blob(&[]);
        assert_eq!(
            reply(call(
                &d,
                &mut s,
                SUBSCRIPTION_MANAGER,
                Id(3, 8),
                w.into_bytes()
            ))
            .0,
            Status::Ok
        );
        let mut w = Writer::new();
        // AddSubscription of the block's, through method 1.1 of object 1055.
        w.u32(4096)
            .id(PROPERTY_CHANGED)
            .u32(1055)
            .id(Id(1, 1))
            .blob(&[9])
            .u8(RELIABLE)
            .blob(&[]);
        assert_eq!(
            reply(call(
                &d,
                &mut s,
                SUBSCRIPTION_MANAGER,
                Id(3, 1),
                w.into_bytes()
            ))
            .0,
            Status::Ok
        );

        let mut tree = d.tree().clone();
        tree.nodes.get_mut(&4097).unwrap().object.kind = Kind::Gain {
            db: -3.0,
            min: -90.0,
            max: 12.0,
        };
        // The meter goes away.
        tree.nodes.remove(&4098);
        tree.nodes
            .get_mut(&4096)
            .unwrap()
            .members
            .retain(|&m| m != 4098);
        let changes = d.set_tree(tree);
        assert_eq!(changes.len(), 2);
        let sent = s.notify(&changes);
        assert_eq!(sent.len(), 2);
        let mut gain = vec![0, 4, 0, 1];
        gain.extend((-3.0f32).to_be_bytes());
        gain.push(1);
        assert!(sent.contains(&Outgoing::Notification2 {
            event: Event {
                emitter: 4097,
                id: PROPERTY_CHANGED
            },
            data: gain,
        }));
        assert!(sent.iter().any(|n| matches!(
            n,
            Outgoing::Notification { target: 1055, context, event, .. }
                if *context == [9] && event.emitter == 4096
        )));
        // Nothing changed, nothing to say.
        let same = d.tree().clone();
        assert!(d.set_tree(same).is_empty());
        // Unless it's unsubscribed.
        let mut w = Writer::new();
        w.u32(4097).id(PROPERTY_CHANGED).u8(RELIABLE).blob(&[]);
        reply(call(
            &d,
            &mut s,
            SUBSCRIPTION_MANAGER,
            Id(3, 9),
            w.into_bytes(),
        ));
        assert_eq!(s.notify(&changes).len(), 1);
    }
}
