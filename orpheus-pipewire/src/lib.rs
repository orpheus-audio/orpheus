//! PipeWire audio backend for Orpheus.
//!
//! PipeWire objects are not `Send`, so everything PipeWire lives on one thread
//! that runs the PipeWire main loop. [`PipeWireBackend`] is a `Send` handle
//! that talks to that thread through a PipeWire channel. The process callbacks
//! run on PipeWire's realtime data thread (`RT_PROCESS`) and only copy samples
//! between PipeWire buffers and the engine's ports.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use orpheus_core::AudioFormat;
use orpheus_engine::{
    AudioBackend, BackendError, CapturePort, PlaybackPort, StreamHandle, StreamSpec,
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

enum Command {
    Create {
        id: u64,
        spec: StreamSpec,
        kind: Kind,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    Destroy(u64),
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
}

impl PipeWireBackend {
    /// Connects to the PipeWire daemon of the current user session.
    pub fn connect() -> Result<Self, BackendError> {
        let (tx, rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("orpheus-pipewire".into())
            .spawn(move || run(rx, ready_tx))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                thread: Some(thread),
                next_id: 0,
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
                spec: spec.clone(),
                kind,
                reply,
            })
            .map_err(|_| "PipeWire thread is gone")?;
        result
            .recv()
            .map_err(|_| "PipeWire thread is gone")?
            .map_err(|e| format!("creating PipeWire stream {}: {e}", spec.name))?;
        let tx = self.tx.clone();
        Ok(StreamHandle::new(move || {
            let _ = tx.send(Command::Destroy(id));
        }))
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
fn run(rx: pw::channel::Receiver<Command>, ready: mpsc::SyncSender<Result<(), String>>) {
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

    let _core_listener = core
        .add_listener_local()
        .error(|id, seq, res, msg| {
            tracing::error!(id, seq, res, msg, "PipeWire error");
        })
        .register();

    let streams: Rc<RefCell<HashMap<u64, Live>>> = Rc::default();
    let groups: Rc<RefCell<Groups>> = Rc::default();
    let _receiver = rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let streams = streams.clone();
        let groups = groups.clone();
        move |cmd| match cmd {
            Command::Create {
                id,
                spec,
                kind,
                reply,
            } => {
                let result = create_stream(&core, &spec, kind, &mut groups.borrow_mut());
                let _ = reply.send(result.map(|live| {
                    streams.borrow_mut().insert(id, live);
                }));
            }
            Command::Destroy(id) => {
                let live = streams.borrow_mut().remove(&id);
                if let Some(group) = live.as_ref().and_then(|l| l.group.as_ref()) {
                    groups.borrow_mut().remove(group);
                }
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
    kind: Kind,
    groups: &mut Groups,
) -> Result<Live, String> {
    let channels = spec.format.channels.to_string();
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::NODE_NAME => spec.name.as_str(),
        *pw::keys::NODE_DESCRIPTION => spec.description.as_str(),
        *pw::keys::AUDIO_CHANNELS => channels.as_str(),
    };
    if let Some(target) = &spec.target {
        props.insert(*pw::keys::TARGET_OBJECT, target.as_str());
    }
    if let Some(group) = &spec.group {
        props.insert(*pw::keys::NODE_GROUP, group.as_str());
        // Keeps the session manager from linking group members to each other.
        props.insert(*pw::keys::NODE_LINK_GROUP, group.as_str());
    }
    if let Some(quantum) = spec.quantum {
        let latency = format!("{quantum}/{}", spec.format.rate);
        props.insert(*pw::keys::NODE_LATENCY, latency.as_str());
    }

    let (direction, flags) = match &kind {
        Kind::Sink(_) => {
            props.insert(*pw::keys::MEDIA_CLASS, "Audio/Sink");
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
            props.insert(*pw::keys::MEDIA_CATEGORY, "Playback");
            props.insert(*pw::keys::MEDIA_ROLE, "Music");
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
                .state_changed(log_state(name))
                .process(process_sink)
                .register()
                .map_err(|e| e.to_string())?,
        ),
        Kind::Playback(port) => Box::new(
            stream
                .add_local_listener_with_user_data(port)
                .state_changed(log_state(name))
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
