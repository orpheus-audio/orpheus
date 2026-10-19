//! AES70 (OCA): controllers such as AES70 Explorer see the wires and the
//! outputs they play on, and set the wires' gains.
//!
//! What controllers see is a view on the wires, built anew from them and
//! the levels every [`TICK`] while any controller is connected:
//!
//! - a block per output wires play on (role: its machine name), holding
//!   a peak meter per channel (`level.N`) and a block per wire;
//! - a block per wire (role: `source:ID` or `input:NAME`), holding its
//!   gain (`gain`), its mute (`mute`), the gain of each pair it names
//!   (`gain.TO=FROM`) and a peak meter per link (`level.TO=FROM`).
//!
//! Wiring stays with Orpheus's API: blocks can't be changed. Object
//! numbers stay the same across restarts (`aes70.json` in the state
//! directory), so that controllers' screens keep working. Controllers
//! connect over TCP (`[aes70] port`) or a WebSocket on the API's port,
//! [`PATH`]; both are announced over mDNS. Nothing asks who connects,
//! as with the API.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use orpheus_aes70::ocp1::{self, Incoming};
use orpheus_aes70::{
    Change, Device, Handled, Identity, Kind, Law, Object, Outgoing, ROOT, Session, Set, Status,
    Tree,
};
use orpheus_core::SourceId;
use orpheus_engine::gain;
use orpheus_engine::level::dbfs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

use crate::api::Node;
use crate::wires::{Channel, Origin};

/// Where the API takes AES70 over a WebSocket.
pub const PATH: &str = "/x-orpheus/v1/aes70";
/// How often the view is built anew while controllers are connected: as
/// often as the meters measure.
const TICK: Duration = orpheus_engine::level::WINDOW;
/// How often a connection looks at its keepalive.
const KEEPALIVE_TICK: Duration = Duration::from_millis(250);
/// The range of the meters, in dBFS: up to what the gains can reach
/// above full scale.
const METER_MIN: f32 = gain::MIN_DB;
const METER_MAX: f32 = gain::MAX_DB;

/// What a gain or a mute of the view sets.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    /// A wire's gain, or its mute.
    Wire { from: Origin, output: String },
    /// The gain of one of the pairs a wire names.
    Pair {
        from: Origin,
        output: String,
        to: Channel,
        channel: Channel,
    },
}

/// The object numbers given out, by what they stand for.
struct Numbers {
    path: PathBuf,
    map: BTreeMap<String, u32>,
    next: u32,
    changed: bool,
}

impl Numbers {
    fn load(path: PathBuf) -> Self {
        let map: BTreeMap<String, u32> = std::fs::read(&path)
            .ok()
            .and_then(|data| {
                serde_json::from_slice(&data)
                    .inspect_err(|e| {
                        tracing::warn!(error = %e, path = %path.display(), "ignoring AES70 object numbers")
                    })
                    .ok()
            })
            .unwrap_or_default();
        let next = map
            .values()
            .max()
            .map_or(orpheus_aes70::device::FIRST_ONO, |n| n + 1);
        Self {
            path,
            map,
            next,
            changed: false,
        }
    }

    /// The number of `key`, given out now if it has none.
    fn get(&mut self, key: String) -> u32 {
        *self.map.entry(key).or_insert_with(|| {
            self.changed = true;
            self.next += 1;
            self.next - 1
        })
    }

    fn save(&mut self) {
        if !self.changed {
            return;
        }
        self.changed = false;
        let json = serde_json::to_vec_pretty(&self.map).expect("numbers serialize");
        if let Err(e) = crate::home::write_atomic(&self.path, &json) {
            tracing::warn!(error = %e, path = %self.path.display(), "saving AES70 object numbers");
        }
    }
}

struct State {
    device: Device,
    targets: BTreeMap<u32, Target>,
    numbers: Numbers,
}

/// The device controllers see.
pub struct Aes70 {
    state: Mutex<State>,
    /// What changed, for every connection.
    changes: broadcast::Sender<Arc<[Change]>>,
}

impl Aes70 {
    /// Loads the object numbers from `path`.
    pub fn new(identity: Identity, path: PathBuf) -> Self {
        Self {
            state: Mutex::new(State {
                device: Device::new(identity, Tree::new()),
                targets: BTreeMap::new(),
                numbers: Numbers::load(path),
            }),
            changes: broadcast::Sender::new(64),
        }
    }
}

/// What this node says about itself.
pub fn identity(node_name: &str, id: orpheus_core::DeviceId) -> Identity {
    Identity {
        name: node_name.into(),
        serial: id.to_string(),
        manufacturer: "Orpheus".into(),
        website: env!("CARGO_PKG_REPOSITORY").into(),
        product: "Orpheus".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    }
}

/// Builds the view anew and tells the connections what changed.
fn refresh(node: &Node, aes70: &Aes70) {
    let mut state = aes70.state.lock().unwrap();
    let (tree, targets) = build(node, &mut state.numbers);
    state.numbers.save();
    state.targets = targets;
    let changes = state.device.set_tree(tree);
    if !changes.is_empty() {
        let _ = aes70.changes.send(changes.into());
    }
}

/// Keeps the view current while controllers are connected.
pub async fn run(node: Arc<Node>) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(aes70) = &node.aes70 else {
            return;
        };
        if aes70.changes.receiver_count() == 0 {
            continue;
        }
        let node = node.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Some(aes70) = &node.aes70 {
                refresh(&node, aes70);
            }
        })
        .await;
    }
}

/// A wire's end as a role and a key: `source:ID` or `input:NAME`.
fn origin_key(from: &Origin) -> String {
    match from {
        Origin::Source(id) => format!("source:{id}"),
        Origin::Input(name) => format!("input:{name}"),
    }
}

/// A level in dBFS to 0.1 dB, so that meters notify only of what shows.
fn peak_db(peak: f32) -> Option<f32> {
    dbfs(peak).map(|db| (db * 10.0).round() / 10.0 + 0.0)
}

fn meter(role: String, label: String, peak: Option<f32>) -> Object {
    Object {
        role,
        label,
        kind: Kind::Level {
            dbfs: peak.and_then(peak_db),
            min: METER_MIN,
            max: METER_MAX,
            law: Law::Peak,
        },
    }
}

fn gain_object(role: String, label: String, db: f32) -> Object {
    Object {
        role,
        label,
        kind: Kind::Gain {
            db,
            min: gain::MIN_DB,
            max: gain::MAX_DB,
        },
    }
}

/// The view on the wires as they are now, and what its gains and mutes
/// set.
fn build(node: &Node, numbers: &mut Numbers) -> (Tree, BTreeMap<u32, Target>) {
    let wires = node.wires.lock().unwrap().list().to_vec();
    let (resolved, measured, outputs, inputs) = match &node.engine {
        Some(engine) => {
            let engine = engine.lock().unwrap();
            (
                crate::wires::resolve(node, &engine, &wires),
                engine.levels(),
                engine.outputs(),
                engine.inputs(),
            )
        }
        None => Default::default(),
    };
    let names: BTreeMap<SourceId, String> = node
        .sources
        .lock()
        .unwrap()
        .list()
        .iter()
        .map(|s| (s.id, s.name.clone()))
        .collect();
    let mut tree = Tree::new();
    let mut targets = BTreeMap::new();
    // The outputs wires play on, by their name.
    let mut wired: Vec<(String, &str)> = wires
        .iter()
        .map(|w| {
            let label = outputs
                .iter()
                .find(|o| o.name == w.output)
                .map_or_else(|| w.output.clone(), |o| o.description.clone());
            (label, w.output.as_str())
        })
        .collect();
    wired.sort();
    wired.dedup();
    for (label, output) in wired {
        let block = numbers.get(format!("output/{output}"));
        tree.add(
            ROOT,
            block,
            Object {
                role: output.into(),
                label,
                kind: Kind::Block,
            },
        );
        let layout = outputs
            .iter()
            .find(|o| o.name == output)
            .map(|o| &o.channels);
        if let Some(m) = measured.outputs.iter().find(|m| m.name == output) {
            for (i, level) in m.channels.iter().enumerate() {
                let label = layout.map_or_else(|| format!("Channel {}", i + 1), |l| l.label(i));
                tree.add(
                    block,
                    numbers.get(format!("output/{output}/level/{}", i + 1)),
                    meter(format!("level.{}", i + 1), label, Some(level.peak)),
                );
            }
        }
        for (w, r) in wires
            .iter()
            .zip(&resolved)
            .filter(|(w, _)| w.output == output)
        {
            let key = format!("wire/{}/{output}", origin_key(&w.from));
            let from_name = match &w.from {
                Origin::Source(id) => names.get(id).cloned().unwrap_or_else(|| id.to_string()),
                Origin::Input(name) => inputs
                    .iter()
                    .find(|i| i.name == *name)
                    .map_or_else(|| name.clone(), |i| i.description.clone()),
            };
            let wire = numbers.get(key.clone());
            tree.add(
                block,
                wire,
                Object {
                    role: origin_key(&w.from),
                    label: from_name,
                    kind: Kind::Block,
                },
            );
            let target = Target::Wire {
                from: w.from.clone(),
                output: w.output.clone(),
            };
            let ono = numbers.get(format!("{key}/gain"));
            tree.add(
                wire,
                ono,
                gain_object("gain".into(), "Gain".into(), w.gain_db),
            );
            targets.insert(ono, target.clone());
            let ono = numbers.get(format!("{key}/mute"));
            tree.add(
                wire,
                ono,
                Object {
                    role: "mute".into(),
                    label: "Mute".into(),
                    kind: Kind::Mute(w.mute),
                },
            );
            targets.insert(ono, target);
            for p in w.channels.iter().flatten() {
                let pair = format!("{}={}", p.to, p.from);
                let ono = numbers.get(format!("{key}/gain/{pair}"));
                let label = format!("Gain {} ← {}", p.to, p.from);
                if tree.add(
                    wire,
                    ono,
                    gain_object(format!("gain.{pair}"), label, p.gain_db),
                ) {
                    targets.insert(
                        ono,
                        Target::Pair {
                            from: w.from.clone(),
                            output: w.output.clone(),
                            to: p.to.clone(),
                            channel: p.from.clone(),
                        },
                    );
                }
            }
            for l in r.links(w) {
                let Some((_, level)) = measured.links.iter().find(|(m, _)| *m == l) else {
                    continue;
                };
                let pair = format!("{}={}", l.to_channel + 1, l.from_channel + 1);
                let label = match (&r.from_layout, &r.to_layout) {
                    (Some(f), Some(t)) => {
                        format!(
                            "Level {} ← {}",
                            t.label(l.to_channel),
                            f.label(l.from_channel)
                        )
                    }
                    _ => format!("Level {pair}"),
                };
                tree.add(
                    wire,
                    numbers.get(format!("{key}/level/{pair}")),
                    meter(format!("level.{pair}"), label, Some(level.peak)),
                );
            }
        }
    }
    (tree, targets)
}

/// Carries out a controller's change.
fn apply(node: &Node, aes70: &Aes70, ono: u32, set: Set) -> Status {
    let Some(target) = aes70.state.lock().unwrap().targets.get(&ono).cloned() else {
        return Status::BadONo;
    };
    let wire = {
        let mut wires = node.wires.lock().unwrap();
        match (&target, set) {
            (Target::Wire { from, output }, Set::Gain(db)) => {
                wires.set_gain(from, output, Some(db), None)
            }
            (Target::Wire { from, output }, Set::Mute(m)) => {
                wires.set_gain(from, output, None, Some(m))
            }
            (
                Target::Pair {
                    from,
                    output,
                    to,
                    channel,
                },
                Set::Gain(db),
            ) => wires.set_pair_gain(from, output, to, channel, db),
            (Target::Pair { .. }, Set::Mute(_)) => return Status::InvalidRequest,
        }
    };
    // Gone since the controller looked.
    let Some(wire) = wire else {
        return Status::BadONo;
    };
    tracing::info!(from = ?wire.from, output = %wire.output, ?set, "wire gain over AES70");
    crate::wires::follow(node);
    Status::Ok
}

/// One controller's connection, whatever carries it.
struct Connection {
    session: Session,
    /// What came in but isn't a whole message yet.
    pending: Vec<u8>,
    /// The longest the controller stays silent, and so do we.
    keepalive: Option<Duration>,
}

impl Connection {
    fn new() -> Self {
        Self {
            session: Session::new(),
            pending: Vec::new(),
            keepalive: None,
        }
    }

    /// Takes `data`, carrying out the commands it completes; the messages
    /// to send back, `None` if the controller doesn't speak OCP.1.
    /// Blocking.
    fn receive(&mut self, node: &Node, aes70: &Aes70, data: &[u8]) -> Option<Vec<Vec<u8>>> {
        self.pending.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            let (pdus, used) = match ocp1::decode(&self.pending) {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error = ?e, "AES70: not OCP.1");
                    return None;
                }
            };
            self.pending.drain(..used);
            for pdu in pdus {
                match pdu {
                    Incoming::KeepAlive(millis) => {
                        self.keepalive = (millis > 0).then(|| Duration::from_millis(millis.into()));
                        out.push(Outgoing::KeepAlive(millis).encode());
                    }
                    Incoming::Command(cmd) => {
                        let handled = aes70
                            .state
                            .lock()
                            .unwrap()
                            .device
                            .handle(&mut self.session, &cmd);
                        match handled {
                            Handled::Reply(reply) => out.extend(reply.map(|r| r.encode())),
                            Handled::Set {
                                ono,
                                set,
                                handle,
                                response,
                            } => {
                                let status = apply(node, aes70, ono, set);
                                // Everyone hears of it at once, the
                                // controller that set it too.
                                refresh(node, aes70);
                                if response {
                                    out.push(Outgoing::status(handle, status).encode());
                                }
                            }
                        }
                    }
                }
            }
        }
        Some(out)
    }
}

/// What carries a connection's messages.
trait Carrier {
    /// The next bytes; `None` once the controller is gone.
    async fn recv(&mut self) -> Option<Vec<u8>>;
    /// `false` if they couldn't go.
    async fn send(&mut self, messages: Vec<Vec<u8>>) -> bool;
}

struct Tcp {
    stream: TcpStream,
    buf: Box<[u8]>,
}

impl Carrier for Tcp {
    async fn recv(&mut self) -> Option<Vec<u8>> {
        match self.stream.read(&mut self.buf).await {
            Ok(0) | Err(_) => None,
            Ok(n) => Some(self.buf[..n].to_vec()),
        }
    }

    async fn send(&mut self, messages: Vec<Vec<u8>>) -> bool {
        self.stream.write_all(&messages.concat()).await.is_ok()
    }
}

impl Carrier for WebSocket {
    async fn recv(&mut self) -> Option<Vec<u8>> {
        loop {
            match WebSocket::recv(self).await? {
                Ok(Message::Binary(data)) => return Some(data.to_vec()),
                // Pings are answered by the socket itself.
                Ok(Message::Ping(_) | Message::Pong(_)) => {}
                Ok(Message::Text(_) | Message::Close(_)) | Err(_) => return None,
            }
        }
    }

    async fn send(&mut self, messages: Vec<Vec<u8>>) -> bool {
        // Each message in a frame of its own.
        for m in messages {
            if WebSocket::send(self, Message::Binary(m.into()))
                .await
                .is_err()
            {
                return false;
            }
        }
        true
    }
}

/// Serves one controller until it goes away.
async fn serve(node: Arc<Node>, mut carrier: impl Carrier) {
    let Some(aes70) = &node.aes70 else {
        return;
    };
    let mut changes = aes70.changes.subscribe();
    {
        let node = node.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Some(aes70) = &node.aes70 {
                refresh(&node, aes70);
            }
        })
        .await;
    }
    let mut conn = Some(Connection::new());
    let mut tick = tokio::time::interval(KEEPALIVE_TICK);
    let (mut heard, mut said) = (Instant::now(), Instant::now());
    loop {
        let out = tokio::select! {
            data = carrier.recv() => {
                let Some(data) = data else { break };
                heard = Instant::now();
                let (node, mut c) = (node.clone(), conn.take().expect("connection"));
                let Ok((c, out)) = tokio::task::spawn_blocking(move || {
                    let out = node.aes70.as_ref().and_then(|a| c.receive(&node, a, &data));
                    (c, out)
                })
                .await
                else {
                    break;
                };
                conn = Some(c);
                match out {
                    Some(out) => out,
                    None => break,
                }
            }
            changed = changes.recv() => match changed {
                Ok(changed) => conn
                    .as_ref()
                    .expect("connection")
                    .session
                    .notify(&changed)
                    .iter()
                    .map(Outgoing::encode)
                    .collect(),
                // A slow controller misses changes, as it would values
                // it reads.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = tick.tick() => {
                let Some(keepalive) = conn.as_ref().and_then(|c| c.keepalive) else {
                    continue;
                };
                if heard.elapsed() > 3 * keepalive {
                    tracing::debug!("AES70: controller went silent");
                    break;
                }
                if said.elapsed() < keepalive / 2 {
                    continue;
                }
                vec![Outgoing::KeepAlive(keepalive.as_millis() as u32).encode()]
            }
        };
        if out.is_empty() {
            continue;
        }
        if !carrier.send(out).await {
            break;
        }
        said = Instant::now();
    }
}

/// Takes controllers' TCP connections.
pub async fn listen(node: Arc<Node>, listener: TcpListener) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        tracing::debug!(%peer, "AES70 controller connected");
        let carrier = Tcp {
            stream,
            buf: vec![0; 64 * 1024].into(),
        };
        tokio::spawn(serve(node.clone(), carrier));
    }
}

/// Serves a controller over a WebSocket.
pub async fn websocket(socket: WebSocket, node: Arc<Node>) {
    serve(node, socket).await;
}
