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

use orpheus_core::{AudioFormat, OutputType};
use orpheus_engine::{
    AudioBackend, BackendError, CapturePort, Output, PlaybackPort, StreamControl, StreamHandle,
    StreamSpec,
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
    Destroy(u64),
    /// Renames a stream.
    Describe(u64, String),
    /// Changes the kind of output a sink leads to: its icon name and type.
    SetRemote(u64, Option<String>, OutputType),
    /// Moves a playback stream to an output, or back to the default one.
    Move(u64, Option<String>, mpsc::SyncSender<Result<(), String>>),
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
    let _receiver =
        rx.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            let streams = streams.clone();
            let groups = groups.clone();
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
                Command::Destroy(id) => {
                    graph.lock().unwrap().streams.remove(&id);
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
                    let result = graph.lock().unwrap().target(id, name.as_deref()).and_then(
                        |(node, serial)| {
                            let metadata = metadata.borrow();
                            let (_, metadata) = metadata
                                .as_ref()
                                .ok_or("no session manager to move streams")?;
                            // As pulseaudio clients do; the session manager links
                            // the stream there and remembers it.
                            metadata.set_property(node, "target.node", None, None);
                            metadata.set_property(
                                node,
                                "target.object",
                                serial.is_some().then_some("Spa:Id"),
                                serial.as_deref(),
                            );
                            Ok(())
                        },
                    );
                    let _ = reply.send(result);
                }
                Command::Quit => mainloop.quit(),
            }
        });

    let _ = ready.send(Ok(()));
    mainloop.run();
    streams.borrow_mut().clear();
    groups.borrow_mut().clear();
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
            // The session manager keys what it restores on this first.
            if let Some(key) = &spec.restore_key {
                props.insert("application.id", key.as_str());
            }
            if let Some(icon) = icon {
                props.insert(*pw::keys::MEDIA_ICON_NAME, icon);
            }
            let mut flags = pw::stream::StreamFlags::AUTOCONNECT;
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
                .state_changed(on_state(name, id, graph))
                .process(process_sink)
                .register()
                .map_err(|e| e.to_string())?,
        ),
        Kind::Playback(port) => Box::new(
            stream
                .add_local_listener_with_user_data(port)
                .state_changed(on_state(name, id, graph))
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

    let format = format_pod(spec.format)?;
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
    Ok(Live {
        _listener: listener,
        stream,
        group,
    })
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
    /// The default sink's node name.
    default: Option<String>,
    /// Links by ID: output node, input node.
    links: HashMap<u32, (u32, u32)>,
    /// Node IDs of our streams, by stream ID.
    streams: HashMap<u64, u32>,
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
            .values()
            .filter(|s| !s.name.is_empty())
            .map(|s| Output {
                name: s.name.clone(),
                description: s.description.clone(),
                kind: s.kind,
                default: self.default.as_deref() == Some(s.name.as_str()),
            })
            .collect();
        out.sort_by(|a, b| a.description.cmp(&b.description).then(a.name.cmp(&b.name)));
        out
    }

    /// The sink stream `id` is linked to.
    fn linked_sink(&self, id: u64) -> Option<String> {
        let node = self.streams.get(&id)?;
        self.links
            .values()
            .filter(|(out, _)| out == node)
            .find_map(|(_, input)| self.sinks.get(input))
            .map(|s| s.name.clone())
    }

    /// The node ID of stream `id` and the serial of output `name`, for
    /// moving the one to the other; no serial for the default output.
    fn target(&self, id: u64, name: Option<&str>) -> Result<(u32, Option<String>), String> {
        let node = *self
            .streams
            .get(&id)
            .ok_or("the stream is not in the audio system yet")?;
        let Some(name) = name else {
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
                match global.type_ {
                    pw::types::ObjectType::Link => {
                        let node = |k| get(k).and_then(|v: &str| v.parse().ok());
                        if let (Some(out), Some(input)) =
                            (node("link.output.node"), node("link.input.node"))
                        {
                            graph.lock().unwrap().links.insert(id, (out, input));
                        }
                    }
                    pw::types::ObjectType::Node
                        if get(*pw::keys::MEDIA_CLASS) == Some("Audio/Sink") =>
                    {
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
                                    let sink = Sink {
                                        name: get(*pw::keys::NODE_NAME),
                                        description: get(*pw::keys::NODE_DESCRIPTION),
                                        kind: output_type(|k| props.get(k)),
                                        serial: get("object.serial"),
                                    };
                                    graph.lock().unwrap().sinks.insert(id, sink);
                                }
                            })
                            .register();
                        graph.lock().unwrap().sinks.insert(id, Sink::default());
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
                                    if subject == 0 && key.is_none_or(|k| k == "default.audio.sink")
                                    {
                                        graph.lock().unwrap().default =
                                            value.and_then(default_name);
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
                g.links.remove(&id);
            }
        })
        .register();
    Ok(Box::new((registry, listener, proxies)))
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
fn format_pod(format: AudioFormat) -> Result<Vec<u8>, String> {
    let channels = format.channels as usize;
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(format!("unsupported channel count {channels}"));
    }
    let mut info = AudioInfoRaw::new();
    info.set_format(SAMPLE_FORMAT);
    info.set_rate(format.rate);
    info.set_channels(format.channels.into());
    info.set_position(channel_positions(channels));

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

/// Mono, stereo, or numbered AUX channels for anything else.
fn channel_positions(channels: usize) -> [u32; MAX_CHANNELS] {
    let mut pos = [0; MAX_CHANNELS];
    match channels {
        1 => pos[0] = spa_sys::SPA_AUDIO_CHANNEL_MONO,
        2 => {
            pos[0] = spa_sys::SPA_AUDIO_CHANNEL_FL;
            pos[1] = spa_sys::SPA_AUDIO_CHANNEL_FR;
        }
        n => {
            for (i, p) in pos.iter_mut().take(n).enumerate() {
                *p = spa_sys::SPA_AUDIO_CHANNEL_AUX0 + i as u32;
            }
        }
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
