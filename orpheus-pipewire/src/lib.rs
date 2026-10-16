//! PipeWire audio backend for Orpheus.
//!
//! PipeWire objects are not `Send`, so everything PipeWire lives on one thread
//! that runs the PipeWire main loop. [`PipeWireBackend`] is a `Send` handle
//! that talks to that thread through a PipeWire channel. The process callbacks
//! run on PipeWire's realtime data thread (`RT_PROCESS`) and only copy samples
//! between PipeWire buffers and the engine's ports.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use orpheus_core::{AudioFormat, Layout, OutputType, Symbol};
use orpheus_engine::{
    AudioBackend, BackendError, CapturePort, Input, Link, Output, OutputSpec, Placement,
    PlaybackPort, StreamControl, StreamHandle, StreamSpec, Volume,
};
use pipewire::{self as pw, properties::properties, spa};
use spa::param::audio::{AudioFormat as SpaFormat, AudioInfoRaw, MAX_CHANNELS};
use spa::pod::deserialize::PodDeserializer;
use spa::pod::{Object, Pod, Property, Value, serialize::PodSerializer};
use spa::sys as spa_sys;

/// Native-endian `f32`, so buffers can be reinterpreted without conversion.
const SAMPLE_FORMAT: SpaFormat = if cfg!(target_endian = "little") {
    SpaFormat::F32LE
} else {
    SpaFormat::F32BE
};
const SAMPLE_SIZE: usize = size_of::<f32>();

/// XDG icon names for the outputs of other nodes, by output type.
///
/// The defaults are standard names that icon themes have, and three
/// `orpheus-*` icons that ship in `orpheusd/dist/icons` for the `hicolor`
/// fallback theme. Their prefix keeps themes from replacing them with
/// unrelated icons of the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icons(BTreeMap<OutputType, String>);

impl Default for Icons {
    fn default() -> Self {
        Self(
            OutputType::ALL
                .into_iter()
                .map(|t| (t, default_icon(t).to_owned()))
                .collect(),
        )
    }
}

impl Icons {
    /// Uses `name` for outputs of type `kind`.
    pub fn set(&mut self, kind: OutputType, name: impl Into<String>) {
        self.0.insert(kind, name.into());
    }

    pub fn get(&self, kind: OutputType) -> &str {
        self.0.get(&kind).map_or(default_icon(kind), String::as_str)
    }
}

const fn default_icon(kind: OutputType) -> &'static str {
    match kind {
        OutputType::Speaker => "orpheus-speaker",
        OutputType::Headphones => "audio-headphones",
        OutputType::Headset => "audio-headset",
        OutputType::Tv => "video-display",
        OutputType::Hifi => "orpheus-hifi",
        OutputType::Computer => "computer",
        OutputType::Portable => "multimedia-player",
        OutputType::Phone => "phone",
        OutputType::Generic => "orpheus-network-speaker",
    }
}

/// PipeWire's `device.form-factor` for an output type, which desktops also
/// use to pick icons and behaviour.
const fn form_factor(kind: OutputType) -> Option<&'static str> {
    match kind {
        OutputType::Speaker => Some("speaker"),
        OutputType::Headphones => Some("headphone"),
        OutputType::Headset => Some("headset"),
        OutputType::Tv => Some("tv"),
        OutputType::Hifi => Some("hifi"),
        OutputType::Computer => Some("computer"),
        OutputType::Portable => Some("portable"),
        OutputType::Phone => Some("handset"),
        OutputType::Generic => None,
    }
}

enum Command {
    Create {
        id: u64,
        spec: Box<StreamSpec>,
        /// Icon name for `spec.remote`.
        icon: Option<String>,
        kind: Kind,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    /// Creates an output on channels of another.
    CreateOutput {
        id: u64,
        spec: Box<OutputSpec>,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    /// Removes a stream or a created output.
    Destroy(u64),
    /// Renames a stream.
    Describe(u64, String),
    /// Changes the kind of output a sink leads to: its icon name and type.
    SetRemote(u64, Option<String>, OutputType),
    /// Moves a playback stream to an output, or back to the default one,
    /// for the user.
    Move(u64, Option<String>, mpsc::SyncSender<Result<(), String>>),
    /// Plays a playback stream where Orpheus wants it.
    Place(u64, Placement, mpsc::SyncSender<Result<(), String>>),
    SetVolume(u64, Volume),
    /// Makes an output the default one, for the user.
    SetDefault(String, mpsc::SyncSender<Result<(), String>>),
    Quit,
}

enum Kind {
    Sink(CapturePort),
    Playback(PlaybackPort),
}

/// Handle to the PipeWire thread. Dropping it stops the thread and removes all
/// streams.
pub struct PipeWireBackend {
    tx: pw::channel::Sender<Command>,
    thread: Option<JoinHandle<()>>,
    next_id: u64,
    icons: Icons,
    /// The outputs and links, kept up to date by the PipeWire thread.
    graph: Arc<Mutex<Graph>>,
}

impl PipeWireBackend {
    /// Connects to the PipeWire daemon of the current user session. Streams
    /// to and from other nodes get their icons from `icons`.
    pub fn connect(icons: Icons) -> Result<Self, BackendError> {
        let (tx, rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let graph = Arc::new(Mutex::new(Graph::default()));
        let thread = thread::Builder::new()
            .name("orpheus-pipewire".into())
            .spawn({
                let graph = graph.clone();
                move || run(rx, ready_tx, graph)
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                thread: Some(thread),
                next_id: 0,
                icons,
                graph,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(format!("connecting to PipeWire: {e}").into())
            }
            Err(_) => Err("PipeWire thread exited during startup".into()),
        }
    }

    fn create(&mut self, spec: &StreamSpec, kind: Kind) -> Result<StreamHandle, BackendError> {
        let id = self.next_id;
        self.next_id += 1;
        let (reply, result) = mpsc::sync_channel(1);
        self.tx
            .send(Command::Create {
                id,
                spec: Box::new(spec.clone()),
                icon: spec.remote.map(|t| self.icons.get(t).to_owned()),
                kind,
                reply,
            })
            .map_err(|_| "PipeWire thread is gone")?;
        result
            .recv()
            .map_err(|_| "PipeWire thread is gone")?
            .map_err(|e| format!("creating PipeWire stream {}: {e}", spec.name))?;
        let close = self.tx.clone();
        Ok(StreamHandle::new(move || {
            let _ = close.send(Command::Destroy(id));
        })
        .with_control(Control {
            id,
            tx: self.tx.clone(),
            icons: self.icons.clone(),
            graph: self.graph.clone(),
        }))
    }
}

/// Changes one stream from outside the PipeWire thread.
struct Control {
    id: u64,
    tx: pw::channel::Sender<Command>,
    icons: Icons,
    graph: Arc<Mutex<Graph>>,
}

impl StreamControl for Control {
    fn describe(&self, description: &str) {
        let _ = self
            .tx
            .send(Command::Describe(self.id, description.to_owned()));
    }

    fn set_remote(&self, remote: OutputType) {
        let icon = self.icons.get(remote).to_owned();
        let _ = self
            .tx
            .send(Command::SetRemote(self.id, Some(icon), remote));
    }

    fn output(&self) -> Option<String> {
        self.graph.lock().unwrap().linked_sink(self.id)
    }

    fn set_output(&self, name: Option<&str>) -> Result<(), BackendError> {
        let (reply, result) = mpsc::sync_channel(1);
        self.tx
            .send(Command::Move(self.id, name.map(str::to_owned), reply))
            .map_err(|_| "PipeWire thread is gone")?;
        Ok(result.recv().map_err(|_| "PipeWire thread is gone")??)
    }

    fn chosen_output(&self) -> Option<Option<String>> {
        self.graph
            .lock()
            .unwrap()
            .playback
            .get(&self.id)?
            .chosen
            .clone()
    }

    fn place(&self, placement: &Placement) -> Result<(), BackendError> {
        let (reply, result) = mpsc::sync_channel(1);
        self.tx
            .send(Command::Place(self.id, placement.clone(), reply))
            .map_err(|_| "PipeWire thread is gone")?;
        Ok(result.recv().map_err(|_| "PipeWire thread is gone")??)
    }

    fn volume(&self) -> Option<Volume> {
        let graph = self.graph.lock().unwrap();
        let p = graph.playback.get(&self.id)?;
        Some(Volume {
            channels: p.channels.clone()?,
            mute: p.mute,
        })
    }

    fn set_volume(&self, volume: &Volume) {
        let _ = self.tx.send(Command::SetVolume(self.id, volume.clone()));
    }
}

impl AudioBackend for PipeWireBackend {
    fn create_sink(
        &mut self,
        spec: &StreamSpec,
        port: CapturePort,
    ) -> Result<StreamHandle, BackendError> {
        self.create(spec, Kind::Sink(port))
    }

    fn create_playback(
        &mut self,
        spec: &StreamSpec,
        port: PlaybackPort,
    ) -> Result<StreamHandle, BackendError> {
        self.create(spec, Kind::Playback(port))
    }

    fn outputs(&self) -> Vec<Output> {
        self.graph.lock().unwrap().outputs()
    }

    fn chosen_default(&self) -> Option<String> {
        self.graph.lock().unwrap().configured.clone()
    }

    fn inputs(&self) -> Vec<Input> {
        self.graph.lock().unwrap().inputs()
    }

    fn create_output(&mut self, spec: &OutputSpec) -> Result<StreamHandle, BackendError> {
        let id = self.next_id;
        self.next_id += 1;
        let (reply, result) = mpsc::sync_channel(1);
        self.tx
            .send(Command::CreateOutput {
                id,
                spec: Box::new(spec.clone()),
                reply,
            })
            .map_err(|_| "PipeWire thread is gone")?;
        result
            .recv()
            .map_err(|_| "PipeWire thread is gone")?
            .map_err(|e| format!("creating output {}: {e}", spec.name))?;
        let close = self.tx.clone();
        Ok(StreamHandle::new(move || {
            let _ = close.send(Command::Destroy(id));
        }))
    }

    fn set_links(&self, links: &[Link]) -> Result<(), BackendError> {
        let mut g = self.graph.lock().unwrap();
        if g.wires != links {
            g.wires = links.to_vec();
            g.changed = true;
        }
        Ok(())
    }

    fn links(&self) -> Vec<Link> {
        let g = self.graph.lock().unwrap();
        g.wires
            .iter()
            .filter(|l| {
                g.ports_of_link(l)
                    .is_some_and(|(_, out, _, input)| g.has_link((out, input)))
            })
            .cloned()
            .collect()
    }

    fn set_default_output(&self, name: &str) -> Result<(), BackendError> {
        let (reply, result) = mpsc::sync_channel(1);
        self.tx
            .send(Command::SetDefault(name.to_owned(), reply))
            .map_err(|_| "PipeWire thread is gone")?;
        Ok(result.recv().map_err(|_| "PipeWire thread is gone")??)
    }
}

impl Drop for PipeWireBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A stream plus its listener. Dropping it disconnects the stream first, so
/// the realtime thread is out of the callbacks before the listener goes.
struct Live {
    _listener: Box<dyn Any>,
    stream: pw::stream::StreamRc,
    /// Set for a group's triggered playback stream.
    group: Option<String>,
    /// For a playback stream: how to connect it again.
    playback: Option<Reconnect>,
}

/// What a playback stream needs to connect again, linked or not.
struct Reconnect {
    flags: pw::stream::StreamFlags,
    format: Vec<u8>,
    /// Connected without autoconnect, so the session manager leaves it
    /// unlinked.
    held: Cell<bool>,
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.stream.disconnect();
    }
}

/// Triggered playback streams by group, for the group's sink to find.
type Groups = HashMap<String, Group>;

struct Group {
    playback: pw::stream::StreamRc,
    latency: Rc<LatencyLink>,
}

/// Passes the latency downstream of a group's playback stream on to the
/// group's sink, plus the sink's own latency. The sink has no links downstream
/// of it, so PipeWire cannot tell applications how late their audio is heard
/// otherwise.
#[derive(Default)]
struct LatencyLink {
    /// Last `Latency` param the playback stream received.
    param: RefCell<Option<Vec<u8>>>,
    sink: RefCell<Option<pw::stream::StreamWeak>>,
    /// The sink's own latency in frames.
    sink_latency: Cell<u32>,
}

impl LatencyLink {
    fn forward(&self) {
        let Some(sink) = self.sink.borrow().as_ref().and_then(|s| s.upgrade()) else {
            return;
        };
        let param = latency_pod(self.param.borrow().as_deref(), self.sink_latency.get());
        if let Some(pod) = param.as_deref().and_then(Pod::from_bytes)
            && let Err(e) = sink.update_params(&mut [pod])
        {
            tracing::warn!(error = %e, "forwarding latency");
        }
    }
}

/// User data of a sink's process callback.
struct SinkData {
    port: CapturePort,
    /// Playback stream of the same group, run right after each sink cycle.
    trigger: Option<pw::stream::StreamRc>,
}

/// Body of the PipeWire thread.
fn run(
    rx: pw::channel::Receiver<Command>,
    ready: mpsc::SyncSender<Result<(), String>>,
    graph: Arc<Mutex<Graph>>,
) {
    pw::init();
    let setup = || -> Result<_, pw::Error> {
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        Ok((mainloop, context, core))
    };
    let (mainloop, _context, core) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    let metadata: Rc<RefCell<Option<(u32, pw::metadata::Metadata)>>> = Rc::default();
    let _outputs = match watch_graph(&core, graph.clone(), metadata.clone()) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "cannot follow the outputs; announcing a generic one");
            None
        }
    };

    let _core_listener = core
        .add_listener_local()
        .error(|id, seq, res, msg| {
            tracing::error!(id, seq, res, msg, "PipeWire error");
        })
        .register();

    let streams: Rc<RefCell<HashMap<u64, Live>>> = Rc::default();
    let groups: Rc<RefCell<Groups>> = Rc::default();
    let created: Rc<RefCell<HashMap<u64, Created>>> = Rc::default();
    // Links the created outputs to their devices, and the wires, as ports
    // come and go.
    let relink = mainloop.loop_().add_timer({
        let core = core.clone();
        let graph = graph.clone();
        let created = created.clone();
        let wired = RefCell::new(Made::new());
        move |_| {
            if std::mem::take(&mut graph.lock().unwrap().changed) {
                link_outputs(&core, &graph, &mut created.borrow_mut());
                let g = graph.lock().unwrap();
                let want = g.wired_links();
                reconcile(&core, &g, &want, &mut wired.borrow_mut(), "wires");
            }
        }
    });
    let _ = relink.update_timer(Some(RELINK), Some(RELINK));
    let _receiver = rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let streams = streams.clone();
        let groups = groups.clone();
        let created = created.clone();
        move |cmd| match cmd {
            Command::Create {
                id,
                spec,
                icon,
                kind,
                reply,
            } => {
                let result = create_stream(
                    &core,
                    &spec,
                    icon.as_deref(),
                    kind,
                    &mut groups.borrow_mut(),
                    (id, graph.clone()),
                );
                let _ = reply.send(result.map(|live| {
                    streams.borrow_mut().insert(id, live);
                }));
            }
            Command::CreateOutput { id, spec, reply } => {
                let result = create_output(&core, &spec).map(|node| {
                    created.borrow_mut().insert(
                        id,
                        Created {
                            spec: *spec,
                            _node: node,
                            links: HashMap::new(),
                        },
                    );
                    graph.lock().unwrap().changed = true;
                });
                let _ = reply.send(result);
            }
            Command::Destroy(id) => {
                // Its links go with it.
                created.borrow_mut().remove(&id);
                let mut g = graph.lock().unwrap();
                g.streams.remove(&id);
                g.playback.remove(&id);
                drop(g);
                let live = streams.borrow_mut().remove(&id);
                if let Some(group) = live.as_ref().and_then(|l| l.group.as_ref()) {
                    groups.borrow_mut().remove(group);
                }
            }
            Command::Describe(id, description) => {
                if let Some(live) = streams.borrow().get(&id) {
                    let d = Some(description.as_str());
                    update_properties(
                        &live.stream,
                        &[(*pw::keys::NODE_DESCRIPTION, d), (*pw::keys::MEDIA_NAME, d)],
                    );
                }
            }
            Command::SetRemote(id, icon, remote) => {
                if let Some(live) = streams.borrow().get(&id) {
                    update_properties(
                        &live.stream,
                        &[
                            (*pw::keys::DEVICE_ICON_NAME, icon.as_deref()),
                            (*pw::keys::DEVICE_FORM_FACTOR, form_factor(remote)),
                        ],
                    );
                }
            }
            Command::Move(id, name, reply) => {
                let streams = streams.borrow();
                let result = match streams.get(&id) {
                    // Linked again, there.
                    Some(live) if live.playback.as_ref().is_some_and(|p| p.held.get()) => {
                        let placement = name.clone().map_or(Placement::Default, Placement::Output);
                        place(live, id, &graph, &metadata, &placement).map(|()| {
                            graph.lock().unwrap().playback.entry(id).or_default().chosen =
                                Some(name);
                        })
                    }
                    _ => move_stream(&graph, &metadata, id, name.as_deref()).map(|()| {
                        graph.lock().unwrap().playback.entry(id).or_default().chosen = Some(name);
                    }),
                };
                let _ = reply.send(result);
            }
            Command::Place(id, placement, reply) => {
                let streams = streams.borrow();
                let result = match streams.get(&id) {
                    Some(live) => place(live, id, &graph, &metadata, &placement),
                    None => Err("no such stream".into()),
                };
                let _ = reply.send(result);
            }
            Command::SetVolume(id, volume) => {
                if let Some(live) = streams.borrow().get(&id) {
                    let mut g = graph.lock().unwrap();
                    let p = g.playback.entry(id).or_default();
                    let pending = p.pending.is_some();
                    p.restore(&volume);
                    if !pending {
                        p.pending = None;
                        drop(g);
                        set_volume(&live.stream, &volume);
                    }
                }
            }
            Command::SetDefault(name, reply) => {
                let _ = reply.send(set_default(&graph, &metadata, &name));
            }
            Command::Quit => mainloop.quit(),
        }
    });

    let _ = ready.send(Ok(()));
    mainloop.run();
    streams.borrow_mut().clear();
    groups.borrow_mut().clear();
    created.borrow_mut().clear();
}

/// How often the links of created outputs are checked, once ports changed.
const RELINK: Duration = Duration::from_millis(200);

/// An output created on channels of another: a null sink whose monitor
/// ports are linked to the other's ports.
struct Created {
    spec: OutputSpec,
    /// The links made, by output and input port, and whether each was
    /// seen in the graph since. Dropped before the node, which would take
    /// them along.
    links: Made,
    /// Keeps the node; dropping it removes it.
    _node: pw::node::Node,
}

/// Creates the null sink of an output; the session manager doesn't link it.
fn create_output(core: &pw::core::CoreRc, spec: &OutputSpec) -> Result<pw::node::Node, String> {
    if spec.layout.len() != spec.channels.len() || spec.channels.is_empty() {
        return Err(format!(
            "a layout of {} channels for {} channels",
            spec.layout.len(),
            spec.channels.len()
        ));
    }
    let positions = position_names(&spec.layout).join(",");
    let channels = spec.layout.len().to_string();
    let props = properties! {
        "factory.name" => "support.null-audio-sink",
        *pw::keys::NODE_NAME => spec.name.as_str(),
        *pw::keys::NODE_DESCRIPTION => spec.description.as_str(),
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::AUDIO_CHANNELS => channels.as_str(),
        "audio.position" => positions.as_str(),
        // The sink's volume applies to what goes on to the device.
        "monitor.channel-volumes" => "true",
        "object.linger" => "false",
    };
    core.create_object::<pw::node::Node>("adapter", &props)
        .map_err(|e| e.to_string())
}

/// Links each created output to the channels of its device that are
/// there.
fn link_outputs(
    core: &pw::core::CoreRc,
    graph: &Mutex<Graph>,
    created: &mut HashMap<u64, Created>,
) {
    let g = graph.lock().unwrap();
    for c in created.values_mut() {
        let want = g.wanted_links(&c.spec);
        reconcile(core, &g, &want, &mut c.links, &c.spec.name);
    }
}

/// Links made by Orpheus, by output and input port, and whether each was
/// seen in the graph since.
type Made = HashMap<(u32, u32), (pw::link::Link, bool)>;

/// Makes the links of `want` that are missing, drops those of `made` no
/// longer wanted, and forgets those that went: made again once their
/// ports are back. `what` names their owner in warnings.
fn reconcile(
    core: &pw::core::CoreRc,
    g: &Graph,
    want: &[(u32, u32, u32, u32)],
    made: &mut Made,
    what: &str,
) {
    made.retain(|pair, (_, seen)| {
        let present = g.has_link(*pair);
        *seen |= present;
        want.iter().any(|w| (w.1, w.3) == *pair) && (present || !*seen)
    });
    for &(out_node, out_port, in_node, in_port) in want {
        if g.has_link((out_port, in_port)) || made.contains_key(&(out_port, in_port)) {
            continue;
        }
        let ids = [out_node, out_port, in_node, in_port].map(|id| id.to_string());
        let props = properties! {
            "link.output.node" => ids[0].as_str(),
            "link.output.port" => ids[1].as_str(),
            "link.input.node" => ids[2].as_str(),
            "link.input.port" => ids[3].as_str(),
            "object.linger" => "false",
        };
        match core.create_object::<pw::link::Link>("link-factory", &props) {
            Ok(link) => {
                made.insert((out_port, in_port), (link, false));
            }
            Err(e) => tracing::warn!(%what, error = %e, "linking channels"),
        }
    }
}

/// Moves playback stream `id` to output `name`, or with `None` to the
/// default one, through the session manager's metadata, as pulseaudio
/// clients do. `-1` names no target, which also overrides one in the
/// stream's properties. The change coming back is not taken for the
/// user's; the caller says whose it is.
fn move_stream(
    graph: &Mutex<Graph>,
    metadata: &RefCell<Option<(u32, pw::metadata::Metadata)>>,
    id: u64,
    name: Option<&str>,
) -> Result<(), String> {
    let mut g = graph.lock().unwrap();
    let (node, serial) = g.target(id, name)?;
    let metadata = metadata.borrow();
    let (_, metadata) = metadata
        .as_ref()
        .ok_or("no session manager to move streams")?;
    let p = g.playback.entry(id).or_default();
    p.chosen = None;
    p.ours.push(serial.clone());
    drop(g);
    metadata.set_property(node, "target.node", None, None);
    metadata.set_property(
        node,
        "target.object",
        Some("Spa:Id"),
        Some(serial.as_deref().unwrap_or("-1")),
    );
    Ok(())
}

/// Makes output `name` the default one the way the desktop's sound
/// settings and `wpctl set-default` do: as the configured default in the
/// session manager's metadata, which the session manager keeps across
/// restarts and follows with the default it uses.
fn set_default(
    graph: &Mutex<Graph>,
    metadata: &RefCell<Option<(u32, pw::metadata::Metadata)>>,
    name: &str,
) -> Result<(), String> {
    if !graph.lock().unwrap().sinks.values().any(|s| s.name == name) {
        return Err(format!("no output {name}"));
    }
    let metadata = metadata.borrow();
    let (_, metadata) = metadata
        .as_ref()
        .ok_or("no session manager to change the default output")?;
    let value = default_json(name);
    metadata.set_property(
        0,
        "default.configured.audio.sink",
        Some("Spa:String:JSON"),
        Some(&value),
    );
    Ok(())
}

/// Plays playback stream `id` where `placement` says: holding it connects
/// it again without autoconnect, so it stays unlinked; letting it go
/// connects it again with the output as its target.
fn place(
    live: &Live,
    id: u64,
    graph: &Mutex<Graph>,
    metadata: &RefCell<Option<(u32, pw::metadata::Metadata)>>,
    placement: &Placement,
) -> Result<(), String> {
    let re = live.playback.as_ref().ok_or("not a playback stream")?;
    let held = *placement == Placement::Held;
    if !held && !re.held.get() {
        let name = match placement {
            Placement::Output(name) => Some(name.as_str()),
            _ => None,
        };
        return move_stream(graph, metadata, id, name);
    }
    if held == re.held.get() {
        return Ok(());
    }
    let target = match placement {
        Placement::Output(name) => Some(name.as_str()),
        _ => None,
    };
    update_properties(
        &live.stream,
        &[
            ("target.object", target),
            (
                *pw::keys::NODE_AUTOCONNECT,
                Some(if held { "false" } else { "true" }),
            ),
        ],
    );
    let flags = if held {
        re.flags - pw::stream::StreamFlags::AUTOCONNECT
    } else {
        re.flags | pw::stream::StreamFlags::AUTOCONNECT
    };
    live.stream.disconnect().map_err(|e| e.to_string())?;
    let mut params = [Pod::from_bytes(&re.format).ok_or("invalid format pod")?];
    live.stream
        .connect(spa::utils::Direction::Output, None, flags, &mut params)
        .map_err(|e| e.to_string())?;
    re.held.set(held);
    let volume = {
        let mut g = graph.lock().unwrap();
        let p = g.playback.entry(id).or_default();
        p.chosen = None;
        p.ours.clear();
        p.channels.clone().map(|channels| Volume {
            channels,
            mute: p.mute,
        })
    };
    // A new node starts at full volume.
    if let Some(v) = volume {
        graph
            .lock()
            .unwrap()
            .playback
            .entry(id)
            .or_default()
            .restore(&v);
    }
    Ok(())
}

/// Sets a stream's volume where its node applies it.
fn set_volume(stream: &pw::stream::Stream, volume: &Volume) {
    if let Err(e) = stream.set_control(spa_sys::SPA_PROP_channelVolumes, &volume.channels) {
        tracing::warn!(error = %e, "setting the stream's volume");
    }
    let mute = if volume.mute { 1.0 } else { 0.0 };
    if let Err(e) = stream.set_control(spa_sys::SPA_PROP_mute, &[mute]) {
        tracing::warn!(error = %e, "muting the stream");
    }
}

/// Creates a stream. A playback stream in a group is not scheduled on its own
/// but triggered by the group's sink after each of its cycles, so audio passes
/// from the sink to the playback within one cycle. Create the playback stream
/// before the sink.
fn create_stream(
    core: &pw::core::CoreRc,
    spec: &StreamSpec,
    icon: Option<&str>,
    kind: Kind,
    groups: &mut Groups,
    (id, graph): (u64, Arc<Mutex<Graph>>),
) -> Result<Live, String> {
    let channels = spec.format.channels.to_string();
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::NODE_NAME => spec.name.as_str(),
        *pw::keys::NODE_DESCRIPTION => spec.description.as_str(),
        *pw::keys::AUDIO_CHANNELS => channels.as_str(),
    };
    if let Some(group) = &spec.group {
        props.insert(*pw::keys::NODE_GROUP, group.as_str());
        // Keeps the session manager from linking group members to each other.
        props.insert(*pw::keys::NODE_LINK_GROUP, group.as_str());
    }
    if let Some(frames) = spec.block_frames {
        let latency = format!("{frames}/{}", spec.format.rate);
        props.insert(*pw::keys::NODE_LATENCY, latency.as_str());
    }

    let (direction, flags) = match &kind {
        Kind::Sink(_) => {
            props.insert(*pw::keys::MEDIA_CLASS, "Audio/Sink");
            if let Some(icon) = icon {
                props.insert(*pw::keys::DEVICE_ICON_NAME, icon);
            }
            if let Some(form_factor) = spec.remote.and_then(form_factor) {
                props.insert(*pw::keys::DEVICE_FORM_FACTOR, form_factor);
            }
            // Orpheus keeps the volume of sinks to other nodes with their
            // route, which the session manager would keep past it.
            if spec.remote.is_some() {
                props.insert("state.restore-props", "false");
            }
            // Never become the default output; that would loop playback back
            // into ourselves.
            props.insert(*pw::keys::PRIORITY_SESSION, "0");
            props.insert(*pw::keys::NODE_VIRTUAL, "true");
            (
                spa::utils::Direction::Input,
                pw::stream::StreamFlags::empty(),
            )
        }
        Kind::Playback(_) => {
            // What mixers show for a stream.
            props.insert(*pw::keys::MEDIA_NAME, spec.description.as_str());
            props.insert(*pw::keys::MEDIA_CATEGORY, "Playback");
            props.insert(*pw::keys::MEDIA_ROLE, "Music");
            // For the session manager's rules.
            if let Some(key) = &spec.key {
                props.insert("application.id", key.as_str());
            }
            // Orpheus keeps the output and volume of streams from other
            // nodes itself, by source; the session manager would by
            // stream, and remember them past what Orpheus forgets.
            if spec.remote.is_some() {
                props.insert("state.restore-props", "false");
                props.insert("state.restore-target", "false");
            }
            if let Placement::Output(name) = &spec.placement {
                props.insert("target.object", name.as_str());
            }
            // Played nowhere else when its output goes, nor destroyed: the
            // stream waits, unlinked, until Orpheus places it again.
            props.insert("node.dont-fallback", "true");
            props.insert("node.linger", "true");
            if let Some(icon) = icon {
                props.insert(*pw::keys::MEDIA_ICON_NAME, icon);
            }
            let mut flags = pw::stream::StreamFlags::empty();
            if spec.placement != Placement::Held {
                flags |= pw::stream::StreamFlags::AUTOCONNECT;
            }
            if spec.group.is_some() {
                flags |= pw::stream::StreamFlags::TRIGGER;
            }
            (spa::utils::Direction::Output, flags)
        }
    };
    let flags = flags | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS;

    let stream =
        pw::stream::StreamRc::new(core.clone(), &spec.name, props).map_err(|e| e.to_string())?;
    let name = spec.name.clone();
    let group = spec.group.as_ref().and_then(|g| groups.get(g));
    let latency_link = Rc::new(LatencyLink::default());
    let listener: Box<dyn Any> = match kind {
        Kind::Sink(port) => Box::new(
            stream
                .add_local_listener_with_user_data(SinkData {
                    port,
                    trigger: group.map(|g| g.playback.clone()),
                })
                .state_changed(on_state(name, id, graph.clone()))
                .control_info({
                    let graph = graph.clone();
                    move |stream, _, control, info| on_control(stream, &graph, id, control, info)
                })
                .process(process_sink)
                .register()
                .map_err(|e| e.to_string())?,
        ),
        Kind::Playback(port) => Box::new(
            stream
                .add_local_listener_with_user_data(port)
                .state_changed(on_state(name, id, graph.clone()))
                .control_info({
                    let graph = graph.clone();
                    move |stream, _, control, info| on_control(stream, &graph, id, control, info)
                })
                .param_changed({
                    let link = latency_link.clone();
                    move |_, _, id, param| {
                        if id == spa_sys::SPA_PARAM_Latency
                            && let Some(param) = param
                        {
                            *link.param.borrow_mut() = Some(param.as_bytes().to_vec());
                            link.forward();
                        }
                    }
                })
                .process(process_playback)
                .register()
                .map_err(|e| e.to_string())?,
        ),
    };

    let format = format_pod(spec.format, &spec.layout)?;
    let mut params = vec![Pod::from_bytes(&format).ok_or("invalid format pod")?];
    let latency = latency_pod(None, spec.process_latency);
    if direction == spa::utils::Direction::Input {
        params.push(
            latency
                .as_deref()
                .and_then(Pod::from_bytes)
                .ok_or("invalid latency pod")?,
        );
    }
    stream
        .connect(direction, None, flags, &mut params)
        .map_err(|e| e.to_string())?;
    // Without the session manager's restore, nobody else gives the node
    // its channel volumes, and mixers would have none to set.
    let volume = spec.volume.clone().or_else(|| {
        spec.remote.map(|_| Volume {
            channels: vec![1.0; spec.format.channels as usize],
            mute: false,
        })
    });
    if let Some(v) = &volume {
        graph
            .lock()
            .unwrap()
            .playback
            .entry(id)
            .or_default()
            .restore(v);
    }
    if let Some(g) = group
        && direction == spa::utils::Direction::Input
    {
        g.latency.sink_latency.set(spec.process_latency);
        *g.latency.sink.borrow_mut() = Some(stream.downgrade());
        g.latency.forward();
    }
    let group = match &spec.group {
        Some(g) if direction == spa::utils::Direction::Output => {
            groups.insert(
                g.clone(),
                Group {
                    playback: stream.clone(),
                    latency: latency_link,
                },
            );
            Some(g.clone())
        }
        _ => None,
    };
    let playback = (direction == spa::utils::Direction::Output).then(|| Reconnect {
        flags: flags - pw::stream::StreamFlags::AUTOCONNECT,
        format,
        held: Cell::new(spec.placement == Placement::Held),
    });
    Ok(Live {
        _listener: listener,
        stream,
        group,
        playback,
    })
}

/// Notes a playback stream's volume and mute as its node reports them,
/// or, the first time, sets the volume to restore.
fn on_control(
    stream: &pw::stream::Stream,
    graph: &Mutex<Graph>,
    id: u64,
    control: u32,
    info: *const pw::sys::pw_stream_control,
) {
    // SAFETY: PipeWire passes a valid control for the duration of the
    // call, whose values hold `n_values` floats.
    let values = unsafe {
        let Some(info) = info.as_ref() else {
            return;
        };
        if info.values.is_null() {
            return;
        }
        std::slice::from_raw_parts(info.values, info.n_values as usize).to_vec()
    };
    let mut g = graph.lock().unwrap();
    let p = g.playback.entry(id).or_default();
    if p.pending.is_some() {
        // Its controls exist now; the next report has these values.
        if control == spa_sys::SPA_PROP_channelVolumes {
            let v = p.pending.take().expect("pending");
            drop(g);
            set_volume(stream, &v);
        }
        return;
    }
    match control {
        spa_sys::SPA_PROP_channelVolumes if !values.is_empty() => p.channels = Some(values),
        spa_sys::SPA_PROP_mute => p.mute = values.first().is_some_and(|&m| m >= 0.5),
        _ => {}
    }
}

/// Sets or, for `None`, removes properties of a running stream.
fn update_properties(stream: &pw::stream::Stream, props: &[(&str, Option<&str>)]) {
    use std::ffi::CString;
    let strings: Vec<(CString, Option<CString>)> = props
        .iter()
        .filter_map(|(k, v)| {
            let v = match v {
                Some(v) => Some(CString::new(*v).ok()?),
                None => None,
            };
            Some((CString::new(*k).ok()?, v))
        })
        .collect();
    let items: Vec<spa_sys::spa_dict_item> = strings
        .iter()
        .map(|(k, v)| spa_sys::spa_dict_item {
            key: k.as_ptr(),
            value: v.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
        })
        .collect();
    let dict = spa_sys::spa_dict {
        flags: 0,
        n_items: items.len() as u32,
        items: items.as_ptr(),
    };
    // SAFETY: the stream is alive, and the dict and its strings outlive the
    // call, which copies them. A null value removes the key.
    let res = unsafe { pw::sys::pw_stream_update_properties(stream.as_raw_ptr(), &dict) };
    if res < 0 {
        tracing::warn!(res, "updating stream properties");
    }
}

/// What the PipeWire thread knows about the graph, for other threads.
#[derive(Default)]
struct Graph {
    /// Audio sinks by node ID.
    sinks: HashMap<u32, Sink>,
    /// Audio sources, such as microphones, by node ID.
    sources: HashMap<u32, Sink>,
    /// The default sink's node name.
    default: Option<String>,
    /// The sink the user made the default, by node name, also while it is
    /// missing.
    configured: Option<String>,
    /// Links by ID.
    links: HashMap<u32, LinkInfo>,
    /// Audio ports by ID.
    ports: HashMap<u32, Port>,
    /// Machine names of all nodes, by ID.
    names: HashMap<u32, String>,
    /// The links asked for besides those of created outputs.
    wires: Vec<Link>,
    /// Whether ports, links or wires came or went since the created
    /// outputs and the wires were last linked.
    changed: bool,
    /// Node IDs of our streams, by stream ID.
    streams: HashMap<u64, u32>,
    /// The volume of our streams and where the user moved playback
    /// streams, by stream ID.
    playback: HashMap<u64, StreamState>,
}

#[derive(Default)]
struct StreamState {
    /// Volume per channel, once the node has said.
    channels: Option<Vec<f32>>,
    mute: bool,
    /// The output the user last moved the stream to, `Some(None)` for the
    /// default one.
    chosen: Option<Option<String>>,
    /// `target.object` values Orpheus wrote and hasn't seen come back.
    ours: Vec<Option<String>>,
    /// The volume to set once the node has its controls.
    pending: Option<Volume>,
}

impl StreamState {
    /// Sets `volume` once the node can take it, and reports it meanwhile.
    fn restore(&mut self, volume: &Volume) {
        self.channels = Some(volume.channels.clone());
        self.mute = volume.mute;
        self.pending = Some(volume.clone());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LinkInfo {
    output_node: u32,
    output_port: u32,
    input_node: u32,
    input_port: u32,
}

/// A port of an audio node.
#[derive(Debug, Clone)]
struct Port {
    node: u32,
    /// Out of the node, else into it.
    output: bool,
    /// A sink's monitor, or a source's capture port of a virtual source.
    monitor: bool,
    /// The port's number on its node, in channel order.
    index: u32,
    /// The channel position, `FL`, `AUX3`.
    channel: String,
}

#[derive(Default)]
struct Sink {
    name: String,
    description: String,
    kind: OutputType,
    /// `object.serial`, by which streams are moved here.
    serial: String,
}

impl Graph {
    fn outputs(&self) -> Vec<Output> {
        let mut out: Vec<Output> = self
            .sinks
            .iter()
            .filter(|(_, s)| !s.name.is_empty())
            .map(|(&id, s)| Output {
                name: s.name.clone(),
                description: s.description.clone(),
                kind: s.kind,
                default: self.default.as_deref() == Some(s.name.as_str()),
                channels: self.layout(id, false),
            })
            .collect();
        out.sort_by(|a, b| a.description.cmp(&b.description).then(a.name.cmp(&b.name)));
        out
    }

    fn inputs(&self) -> Vec<Input> {
        let mut out: Vec<Input> = self
            .sources
            .iter()
            .filter(|(_, s)| !s.name.is_empty())
            .map(|(&id, s)| Input {
                name: s.name.clone(),
                description: s.description.clone(),
                channels: self.layout(id, true),
            })
            .collect();
        out.sort_by(|a, b| a.description.cmp(&b.description).then(a.name.cmp(&b.name)));
        out
    }

    /// The ports of `node` that carry its channels, in order: into a sink
    /// (not its monitor), out of a source.
    fn channel_ports(&self, node: u32, output: bool) -> Vec<(u32, &Port)> {
        self.ports_of(node, |p| p.output == output && (output || !p.monitor))
    }

    fn ports_of(&self, node: u32, keep: impl Fn(&Port) -> bool) -> Vec<(u32, &Port)> {
        let mut ports: Vec<(u32, &Port)> = self
            .ports
            .iter()
            .filter(|(_, p)| p.node == node && keep(p))
            .map(|(&id, p)| (id, p))
            .collect();
        ports.sort_by_key(|(_, p)| p.index);
        ports
    }

    fn layout(&self, node: u32, output: bool) -> Layout {
        let ports = self.channel_ports(node, output);
        let channels: Vec<&str> = ports.iter().map(|(_, p)| p.channel.as_str()).collect();
        layout_of(&channels)
    }

    /// The links created output `spec` needs, as far as its ports and its
    /// device's are there: output node and port, input node and port.
    fn wanted_links(&self, spec: &OutputSpec) -> Vec<(u32, u32, u32, u32)> {
        let node = |name: &str| {
            self.sinks
                .iter()
                .find(|(_, s)| s.name == name)
                .map(|(&id, _)| id)
        };
        let (Some(own), Some(device)) = (node(&spec.name), node(&spec.device)) else {
            return Vec::new();
        };
        let monitors = self.ports_of(own, |p| p.output && p.monitor);
        let inputs = self.channel_ports(device, false);
        spec.channels
            .iter()
            .enumerate()
            .filter_map(|(i, &ch)| {
                let (out_port, _) = monitors.get(i)?;
                let (in_port, _) = inputs.get(ch)?;
                Some((own, *out_port, device, *in_port))
            })
            .collect()
    }

    /// The wires' links whose ports are there: output node and port,
    /// input node and port.
    fn wired_links(&self) -> Vec<(u32, u32, u32, u32)> {
        let mut want: Vec<_> = self
            .wires
            .iter()
            .filter_map(|w| self.ports_of_link(w))
            .collect();
        want.sort_unstable();
        want.dedup();
        want
    }

    fn ports_of_link(&self, link: &Link) -> Option<(u32, u32, u32, u32)> {
        let node = |name: &str| {
            self.names
                .iter()
                .find(|(_, n)| *n == name)
                .map(|(&id, _)| id)
        };
        let (from, to) = (node(&link.from)?, node(&link.to)?);
        let (out_port, _) = *self.channel_ports(from, true).get(link.from_channel)?;
        let (in_port, _) = *self.channel_ports(to, false).get(link.to_channel)?;
        Some((from, out_port, to, in_port))
    }

    fn has_link(&self, (output_port, input_port): (u32, u32)) -> bool {
        self.links
            .values()
            .any(|l| l.output_port == output_port && l.input_port == input_port)
    }

    /// The sink stream `id` is linked to.
    fn linked_sink(&self, id: u64) -> Option<String> {
        let node = self.streams.get(&id)?;
        self.links
            .values()
            .filter(|l| l.output_node == *node)
            .find_map(|l| self.sinks.get(&l.input_node))
            .map(|s| s.name.clone())
    }

    /// Notes a metadata change of `key` for node `subject`: if it moves
    /// one of our playback streams and Orpheus didn't, the user chose
    /// that output.
    fn moved(&mut self, subject: u32, key: &str, value: Option<&str>) {
        let Some(id) = self
            .streams
            .iter()
            .find_map(|(&id, &node)| (node == subject).then_some(id))
        else {
            return;
        };
        let value = value.filter(|v| *v != "-1");
        let sink = match (key, value) {
            ("target.object", None) => None,
            ("target.object", Some(v)) => match self.sinks.values().find(|s| s.serial == v) {
                Some(s) => Some(s.name.clone()),
                None => return,
            },
            // Cleared along with every move.
            ("target.node", None) => return,
            ("target.node", Some(v)) => match v.parse().ok().and_then(|n| self.sinks.get(&n)) {
                Some(s) => Some(s.name.clone()),
                None => return,
            },
            _ => return,
        };
        let p = self.playback.entry(id).or_default();
        if key == "target.object"
            && let Some(i) = p.ours.iter().position(|o| o.as_deref() == value)
        {
            p.ours.remove(i);
            return;
        }
        p.chosen = Some(sink);
    }

    /// The node ID of stream `id` and the serial of output `name`, else of
    /// the default output, for moving the one to the other; no serial
    /// while there is no default output.
    ///
    /// Our streams don't fall back (see `node.dont-fallback`), so following
    /// the default (`-1`) would leave them unlinked; Orpheus follows it
    /// itself.
    fn target(&self, id: u64, name: Option<&str>) -> Result<(u32, Option<String>), String> {
        let node = *self
            .streams
            .get(&id)
            .ok_or("the stream is not in the audio system yet")?;
        let Some(name) = name.or(self.default.as_deref()) else {
            return Ok((node, None));
        };
        let sink = self
            .sinks
            .values()
            .find(|s| s.name == name && !s.serial.is_empty())
            .ok_or_else(|| format!("no output {name}"))?;
        Ok((node, Some(sink.serial.clone())))
    }
}

/// Follows the audio sinks, the default one and the links into them,
/// writing them to `graph`. Keep the result alive for as long as that
/// should go on.
fn watch_graph(
    core: &pw::core::CoreRc,
    graph: Arc<Mutex<Graph>>,
    // The session manager's default metadata and its global ID.
    default_metadata: Rc<RefCell<Option<(u32, pw::metadata::Metadata)>>>,
) -> Result<Box<dyn Any>, pw::Error> {
    let registry = core.get_registry_rc()?;
    // Proxies and their listeners, which only this thread may touch.
    let proxies: Rc<RefCell<HashMap<u32, Box<dyn Any>>>> = Rc::default();
    let listener = registry
        .add_listener_local()
        .global({
            let registry = registry.downgrade();
            let proxies = proxies.clone();
            let graph = graph.clone();
            let default_metadata = default_metadata.clone();
            move |global| {
                let Some(registry) = registry.upgrade() else {
                    return;
                };
                let props = global.props;
                let get = |key: &str| props.and_then(|p| p.get(key));
                let id = global.id;
                if global.type_ == pw::types::ObjectType::Node
                    && let Some(name) = get(*pw::keys::NODE_NAME)
                {
                    let mut g = graph.lock().unwrap();
                    g.names.insert(id, name.to_owned());
                    g.changed = true;
                }
                match global.type_ {
                    pw::types::ObjectType::Link => {
                        let id_of = |k| get(k).and_then(|v: &str| v.parse().ok());
                        if let (
                            Some(output_node),
                            Some(output_port),
                            Some(input_node),
                            Some(input_port),
                        ) = (
                            id_of("link.output.node"),
                            id_of("link.output.port"),
                            id_of("link.input.node"),
                            id_of("link.input.port"),
                        ) {
                            let mut g = graph.lock().unwrap();
                            g.links.insert(
                                id,
                                LinkInfo {
                                    output_node,
                                    output_port,
                                    input_node,
                                    input_port,
                                },
                            );
                            g.changed = true;
                        }
                    }
                    pw::types::ObjectType::Port => {
                        let number = |k| get(k).and_then(|v: &str| v.parse().ok());
                        let (Some(node), Some(index), Some(channel)) =
                            (number("node.id"), number("port.id"), get("audio.channel"))
                        else {
                            return;
                        };
                        let port = Port {
                            node,
                            output: get("port.direction") == Some("out"),
                            monitor: get("port.monitor") == Some("true"),
                            index,
                            channel: channel.to_owned(),
                        };
                        let mut g = graph.lock().unwrap();
                        g.ports.insert(id, port);
                        g.changed = true;
                    }
                    pw::types::ObjectType::Node
                        if get(*pw::keys::MEDIA_CLASS).is_some_and(|c| {
                            c == "Audio/Sink" || c.starts_with("Audio/Source")
                        }) =>
                    {
                        let sink = get(*pw::keys::MEDIA_CLASS) == Some("Audio/Sink");
                        let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                            return;
                        };
                        let listener = node
                            .add_listener_local()
                            .info({
                                let graph = graph.clone();
                                move |info| {
                                    // Other changes come without the properties.
                                    if !info.change_mask().contains(pw::node::NodeChangeMask::PROPS)
                                    {
                                        return;
                                    }
                                    let Some(props) = info.props() else {
                                        return;
                                    };
                                    let get = |k| props.get(k).unwrap_or_default().to_owned();
                                    let node = Sink {
                                        name: get(*pw::keys::NODE_NAME),
                                        description: get(*pw::keys::NODE_DESCRIPTION),
                                        kind: output_type(|k| props.get(k)),
                                        serial: get("object.serial"),
                                    };
                                    let mut g = graph.lock().unwrap();
                                    if sink {
                                        g.sinks.insert(id, node);
                                    } else {
                                        g.sources.insert(id, node);
                                    }
                                    // Its name is known now.
                                    g.changed = true;
                                }
                            })
                            .register();
                        let mut g = graph.lock().unwrap();
                        if sink {
                            g.sinks.insert(id, Sink::default());
                        } else {
                            g.sources.insert(id, Sink::default());
                        }
                        proxies.borrow_mut().insert(id, Box::new((node, listener)));
                    }
                    pw::types::ObjectType::Metadata if get("metadata.name") == Some("default") => {
                        let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global)
                        else {
                            return;
                        };
                        let listener = metadata
                            .add_listener_local()
                            .property({
                                let graph = graph.clone();
                                move |subject, key, _, value| {
                                    let mut graph = graph.lock().unwrap();
                                    if subject == 0 && key.is_none() {
                                        graph.default = None;
                                        graph.configured = None;
                                    } else if subject == 0 && key == Some("default.audio.sink") {
                                        graph.default = value.and_then(default_name);
                                    } else if subject == 0
                                        && key == Some("default.configured.audio.sink")
                                    {
                                        graph.configured = value.and_then(default_name);
                                    } else if let Some(key) = key {
                                        graph.moved(subject, key, value);
                                    }
                                    0
                                }
                            })
                            .register();
                        proxies.borrow_mut().insert(id, Box::new(listener));
                        *default_metadata.borrow_mut() = Some((id, metadata));
                    }
                    _ => {}
                }
            }
        })
        .global_remove({
            let proxies = proxies.clone();
            let graph = graph.clone();
            move |id| {
                proxies.borrow_mut().remove(&id);
                let mut metadata = default_metadata.borrow_mut();
                if metadata.as_ref().is_some_and(|(m, _)| *m == id) {
                    *metadata = None;
                }
                let mut g = graph.lock().unwrap();
                g.sinks.remove(&id);
                g.sources.remove(&id);
                g.names.remove(&id);
                if g.links.remove(&id).is_some() || g.ports.remove(&id).is_some() {
                    g.changed = true;
                }
            }
        })
        .register();
    Ok(Box::new((registry, listener, proxies)))
}

/// Output `name` → `{"name":"…"}`, as the default is kept in metadata.
fn default_json(name: &str) -> String {
    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{{\"name\":\"{escaped}\"}}")
}

/// `{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}` → the name.
fn default_name(json: &str) -> Option<String> {
    let rest = json
        .split_once("\"name\"")?
        .1
        .trim_start()
        .strip_prefix(':')?;
    let rest = rest.trim_start().strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_owned())
}

/// The kind of device a sink is, from its properties: PipeWire's form
/// factor, else an HDMI or DisplayPort profile as a screen.
fn output_type<'a>(get: impl Fn(&str) -> Option<&'a str>) -> OutputType {
    let by_form = match get(*pw::keys::DEVICE_FORM_FACTOR) {
        Some("speaker") => Some(OutputType::Speaker),
        Some("headphone") => Some(OutputType::Headphones),
        Some("headset" | "handsfree") => Some(OutputType::Headset),
        Some("tv") => Some(OutputType::Tv),
        Some("hifi") => Some(OutputType::Hifi),
        Some("computer" | "internal") => Some(OutputType::Computer),
        Some("portable") => Some(OutputType::Portable),
        Some("handset") => Some(OutputType::Phone),
        _ => None,
    };
    let screen =
        get("device.profile.name").is_some_and(|p| p.contains("hdmi") || p.contains("displayport"));
    by_form
        .or(screen.then_some(OutputType::Tv))
        .unwrap_or(OutputType::Generic)
}

/// Logs state changes and notes the stream's node ID once it has one.
fn on_state<D>(
    name: String,
    id: u64,
    graph: Arc<Mutex<Graph>>,
) -> impl FnMut(&pw::stream::Stream, &mut D, pw::stream::StreamState, pw::stream::StreamState) {
    let mut log = log_state(name);
    move |stream, data, old, new| {
        let node = stream.node_id();
        if node != u32::MAX {
            graph.lock().unwrap().streams.insert(id, node);
        }
        log(stream, data, old, new);
    }
}

fn log_state<D>(
    name: String,
) -> impl FnMut(&pw::stream::Stream, &mut D, pw::stream::StreamState, pw::stream::StreamState) {
    move |stream, _, old, new| match (&old, &new) {
        (_, pw::stream::StreamState::Error(e)) => {
            tracing::error!(%name, error = %e, "stream failed")
        }
        (pw::stream::StreamState::Connecting, pw::stream::StreamState::Paused) => {
            tracing::info!(%name, node = stream.node_id(), "stream connected")
        }
        _ => tracing::debug!(%name, ?old, ?new, "stream state"),
    }
}

/// Realtime: copies what applications played into the sink into the port.
fn process_sink(stream: &pw::stream::Stream, data: &mut SinkData) {
    if let Some(mut buffer) = stream.dequeue_buffer() {
        copy_in(&mut buffer, &mut data.port);
    }
    if let Some(playback) = &data.trigger {
        let _ = playback.trigger_process();
    }
}

fn copy_in(buffer: &mut pw::buffer::Buffer, port: &mut CapturePort) {
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };
    let offset = data.chunk().offset() as usize;
    let size = data.chunk().size() as usize;
    let Some(bytes) = data.data() else {
        return;
    };
    let end = (offset + size).min(bytes.len());
    let bytes = &bytes[offset.min(end)..end];
    let bytes = &bytes[..bytes.len() - bytes.len() % SAMPLE_SIZE];
    if let Ok(samples) = bytemuck::try_cast_slice::<u8, f32>(bytes) {
        port.push(samples);
    }
}

/// Realtime: fills a playback buffer from the port.
fn process_playback(stream: &pw::stream::Stream, port: &mut PlaybackPort) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let stride = SAMPLE_SIZE * port.channels();
    let requested = buffer.requested() as usize;
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };
    let Some(bytes) = data.data() else {
        return;
    };
    let capacity = bytes.len() / stride;
    let frames = if requested > 0 {
        requested.min(capacity)
    } else {
        capacity
    };
    let written = match bytemuck::try_cast_slice_mut::<u8, f32>(&mut bytes[..frames * stride]) {
        Ok(samples) => {
            port.fill(samples);
            frames
        }
        Err(_) => 0,
    };
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride as i32;
    *chunk.size_mut() = (written * stride) as u32;
}

/// Serializes an `EnumFormat` param offering interleaved native-endian `f32`.
fn format_pod(format: AudioFormat, layout: &Layout) -> Result<Vec<u8>, String> {
    let channels = format.channels as usize;
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(format!("unsupported channel count {channels}"));
    }
    let mut info = AudioInfoRaw::new();
    info.set_format(SAMPLE_FORMAT);
    info.set_rate(format.rate);
    info.set_channels(format.channels.into());
    if layout.len() != channels {
        return Err(format!(
            "a layout of {} channels for {channels}",
            layout.len()
        ));
    }
    info.set_position(channel_positions(layout));

    let value = Value::Object(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_Format,
        id: spa_sys::SPA_PARAM_EnumFormat,
        properties: info.into(),
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|e| format!("serializing format: {e:?}"))
}

/// Serializes the `Latency` param a sink reports upstream: `downstream` (a
/// `Latency` param received further down, if any) plus `frames`.
fn latency_pod(downstream: Option<&[u8]>, frames: u32) -> Option<Vec<u8>> {
    use spa_sys::*;
    let mut value = downstream
        .and_then(|b| PodDeserializer::deserialize_any_from(b).ok())
        .map(|(_, v)| v)
        .unwrap_or_else(|| {
            let props = vec![
                Property::new(
                    SPA_PARAM_LATENCY_direction,
                    Value::Id(spa::utils::Id(SPA_DIRECTION_INPUT)),
                ),
                Property::new(SPA_PARAM_LATENCY_minQuantum, Value::Float(0.0)),
                Property::new(SPA_PARAM_LATENCY_maxQuantum, Value::Float(0.0)),
                Property::new(SPA_PARAM_LATENCY_minRate, Value::Int(0)),
                Property::new(SPA_PARAM_LATENCY_maxRate, Value::Int(0)),
                Property::new(SPA_PARAM_LATENCY_minNs, Value::Long(0)),
                Property::new(SPA_PARAM_LATENCY_maxNs, Value::Long(0)),
            ];
            Value::Object(Object {
                type_: SPA_TYPE_OBJECT_ParamLatency,
                id: SPA_PARAM_Latency,
                properties: props,
            })
        });
    let Value::Object(obj) = &mut value else {
        return None;
    };
    for prop in &mut obj.properties {
        let is_rate =
            prop.key == SPA_PARAM_LATENCY_minRate || prop.key == SPA_PARAM_LATENCY_maxRate;
        if let (true, Value::Int(rate)) = (is_rate, &mut prop.value) {
            *rate = rate.saturating_add(frames as i32);
        }
    }
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .ok()
        .map(|(cursor, _)| cursor.into_inner())
}

/// The PipeWire position of each channel: where its symbol says, else
/// (and for a position already taken) a numbered AUX channel, which
/// PipeWire passes on by number.
fn channel_positions(layout: &Layout) -> [u32; MAX_CHANNELS] {
    use spa_sys::*;
    let mut pos = [0; MAX_CHANNELS];
    for (i, symbol) in layout.symbols().enumerate().take(MAX_CHANNELS) {
        let p = match symbol {
            Symbol::M1 if layout.len() == 1 => SPA_AUDIO_CHANNEL_MONO,
            Symbol::L | Symbol::Lt => SPA_AUDIO_CHANNEL_FL,
            Symbol::R | Symbol::Rt => SPA_AUDIO_CHANNEL_FR,
            Symbol::C => SPA_AUDIO_CHANNEL_FC,
            Symbol::Lfe => SPA_AUDIO_CHANNEL_LFE,
            Symbol::Ls | Symbol::Lss => SPA_AUDIO_CHANNEL_SL,
            Symbol::Rs | Symbol::Rss => SPA_AUDIO_CHANNEL_SR,
            Symbol::Lrs => SPA_AUDIO_CHANNEL_RL,
            Symbol::Rrs => SPA_AUDIO_CHANNEL_RR,
            Symbol::Lc => SPA_AUDIO_CHANNEL_FLC,
            Symbol::Rc => SPA_AUDIO_CHANNEL_FRC,
            Symbol::Cs => SPA_AUDIO_CHANNEL_RC,
            _ => SPA_AUDIO_CHANNEL_UNKNOWN,
        };
        pos[i] = if p == SPA_AUDIO_CHANNEL_UNKNOWN || pos[..i].contains(&p) {
            SPA_AUDIO_CHANNEL_AUX0 + i as u32
        } else {
            p
        };
    }
    pos
}

/// PipeWire's names of the positions [`channel_positions`] uses.
fn position_names(layout: &Layout) -> Vec<String> {
    use spa_sys::*;
    let names = [
        (SPA_AUDIO_CHANNEL_MONO, "MONO"),
        (SPA_AUDIO_CHANNEL_FL, "FL"),
        (SPA_AUDIO_CHANNEL_FR, "FR"),
        (SPA_AUDIO_CHANNEL_FC, "FC"),
        (SPA_AUDIO_CHANNEL_LFE, "LFE"),
        (SPA_AUDIO_CHANNEL_SL, "SL"),
        (SPA_AUDIO_CHANNEL_SR, "SR"),
        (SPA_AUDIO_CHANNEL_RL, "RL"),
        (SPA_AUDIO_CHANNEL_RR, "RR"),
        (SPA_AUDIO_CHANNEL_FLC, "FLC"),
        (SPA_AUDIO_CHANNEL_FRC, "FRC"),
        (SPA_AUDIO_CHANNEL_RC, "RC"),
    ];
    channel_positions(layout)[..layout.len().min(MAX_CHANNELS)]
        .iter()
        .map(|&p| match names.iter().find(|(id, _)| *id == p) {
            Some((_, name)) => (*name).to_owned(),
            None => format!("AUX{}", p - SPA_AUDIO_CHANNEL_AUX0),
        })
        .collect()
}

/// The layout of a node's channels from their PipeWire positions. Side
/// channels are the surrounds of 5.1 and the side surrounds of 7.1; rear
/// ones the surrounds of a 5.1 without sides.
fn layout_of(positions: &[&str]) -> Layout {
    use orpheus_core::layout::Channel;
    let has = |p: &str| positions.contains(&p);
    let sides = has("SL") || has("SR");
    let rears = has("RL") || has("RR");
    let channels = positions
        .iter()
        .enumerate()
        .map(|(i, &p)| {
            let symbol = match p {
                "MONO" => Symbol::M1,
                "FL" => Symbol::L,
                "FR" => Symbol::R,
                "FC" => Symbol::C,
                "LFE" => Symbol::Lfe,
                "SL" if rears => Symbol::Lss,
                "SR" if rears => Symbol::Rss,
                "SL" => Symbol::Ls,
                "SR" => Symbol::Rs,
                "RL" if sides => Symbol::Lrs,
                "RR" if sides => Symbol::Rrs,
                "RL" => Symbol::Ls,
                "RR" => Symbol::Rs,
                "FLC" => Symbol::Lc,
                "FRC" => Symbol::Rc,
                "RC" => Symbol::Cs,
                _ => {
                    let aux = p.strip_prefix("AUX").and_then(|n| n.parse::<usize>().ok());
                    Symbol::U((aux.unwrap_or(i) + 1).min(64) as u8)
                }
            };
            Channel::new(symbol)
        })
        .collect();
    Layout::new(channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_from_positions() {
        let shown = |p: &[&str]| layout_of(p).to_string();
        assert_eq!(shown(&["FL", "FR"]), "ST");
        assert_eq!(shown(&["MONO"]), "M");
        assert_eq!(shown(&["FL", "FR", "FC", "LFE", "SL", "SR"]), "51");
        assert_eq!(shown(&["FL", "FR", "FC", "LFE", "RL", "RR"]), "51");
        assert_eq!(
            shown(&["FL", "FR", "FC", "LFE", "RL", "RR", "SL", "SR"]),
            "L R C LFE Lrs Rrs Lss Rss"
        );
        assert_eq!(shown(&["AUX0", "AUX1", "AUX2"]), "U03");
        // And back.
        for layout in ["ST", "M", "51", "U04"] {
            let layout = Layout::parse(layout).unwrap();
            let names = position_names(&layout);
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            assert_eq!(layout_of(&names), layout);
        }
    }

    #[test]
    fn positions() {
        use spa_sys::*;
        let pos = |layout: &str| {
            let layout = Layout::parse(layout).unwrap();
            channel_positions(&layout)[..layout.len()].to_vec()
        };
        assert_eq!(pos("M"), [SPA_AUDIO_CHANNEL_MONO]);
        assert_eq!(pos("ST"), [SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR]);
        assert_eq!(pos("51")[4], SPA_AUDIO_CHANNEL_SL);
        // Taken twice, or nowhere to put it.
        assert_eq!(
            pos("L L M1 M2"),
            [
                SPA_AUDIO_CHANNEL_FL,
                SPA_AUDIO_CHANNEL_AUX0 + 1,
                SPA_AUDIO_CHANNEL_AUX0 + 2,
                SPA_AUDIO_CHANNEL_AUX0 + 3
            ]
        );
    }

    #[test]
    fn icons() {
        let mut icons = Icons::default();
        assert_eq!(icons.get(OutputType::Generic), "orpheus-network-speaker");
        assert_eq!(icons.get(OutputType::Headphones), "audio-headphones");
        icons.set(OutputType::Hifi, "audio-card");
        assert_eq!(icons.get(OutputType::Hifi), "audio-card");
        assert_eq!(form_factor(OutputType::Phone), Some("handset"));
        assert_eq!(form_factor(OutputType::Generic), None);
    }

    #[test]
    fn output_types() {
        let props = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| *v)
        };
        assert_eq!(
            output_type(props(&[("device.form-factor", "internal")])),
            OutputType::Computer
        );
        assert_eq!(
            output_type(props(&[("device.form-factor", "headphone")])),
            OutputType::Headphones
        );
        assert_eq!(
            output_type(props(&[("device.profile.name", "hdmi-stereo-extra1")])),
            OutputType::Tv
        );
        assert_eq!(output_type(props(&[])), OutputType::Generic);
        assert_eq!(
            default_name(r#"{ "name": "alsa_output.pci-0000_00_1f.3.analog-stereo" }"#).as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
        );
        assert_eq!(default_name("{}"), None);
        let name = "orpheus.send.0123456789";
        assert_eq!(default_name(&default_json(name)).as_deref(), Some(name));
        assert_eq!(default_json(r#"a"b\c"#), r#"{"name":"a\"b\\c"}"#);
    }
}
