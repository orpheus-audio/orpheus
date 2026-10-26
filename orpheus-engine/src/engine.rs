//! The engine: owns the backend and the audio routes built on top of it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orpheus_clock::Clock;
use orpheus_core::{AudioFormat, Layout, OutputType};

use crate::backend::{
    AppStream, AudioBackend, BackendError, Input, Link, Output, OutputSpec, Placement,
    StreamHandle, StreamSpec, Volume,
};
use crate::port::{PortStats, StatsSnapshot, port_pair};
use crate::receiver::{Filter, Receiver, ReceiverConfig, ReceiverStatus};
use crate::sender::{Route, Sender, SenderConfig, SenderStatus};

/// A local test route: a virtual sink whose audio is played straight back on
/// the local output. Exercises the backend, its realtime callbacks and the
/// ports without any networking.
#[derive(Debug, Clone)]
pub struct LoopbackConfig {
    pub name: String,
    pub description: String,
    /// Of the stream that plays what the sink takes.
    pub playback_description: String,
    pub format: AudioFormat,
    pub latency_ms: u32,
    /// Frames per cycle to ask the audio system for.
    pub block_frames: Option<u32>,
}

struct Loopback {
    // Dropped in declaration order: the sink drives the playback stream, so
    // it goes first.
    _sink: StreamHandle,
    _playback: StreamHandle,
    stats: Arc<PortStats>,
}

/// How long a sender's new sink may take to show in the audio system.
const SINK_TIMEOUT: Duration = Duration::from_secs(1);

/// Senders are named by the route they serve, which outlives them.
pub type SenderId = orpheus_core::RouteId;
pub type ReceiverId = u32;

/// A snapshot of everything the engine runs.
#[derive(Debug, Clone, Default)]
pub struct EngineStatus {
    pub loopback: Option<StatsSnapshot>,
    pub senders: Vec<(SenderId, SenderStatus)>,
    pub receivers: Vec<(ReceiverId, ReceiverStatus)>,
}

/// A sender whose sink is gone for now: what it needs to start again.
struct Hidden {
    cfg: SenderConfig,
    route: Route,
    paused: bool,
    label: Option<String>,
}

pub struct Engine {
    // Routes go before the backend they live in.
    senders: BTreeMap<SenderId, Sender>,
    hidden: BTreeMap<SenderId, Hidden>,
    receivers: BTreeMap<ReceiverId, Receiver>,
    loopback: Option<Loopback>,
    /// The outputs created in the audio system, by machine name.
    created: BTreeMap<String, StreamHandle>,
    clock: Arc<dyn Clock>,
    backend: Box<dyn AudioBackend>,
}

impl Engine {
    pub fn new(backend: Box<dyn AudioBackend>, clock: Arc<dyn Clock>) -> Self {
        Self {
            senders: BTreeMap::new(),
            hidden: BTreeMap::new(),
            receivers: BTreeMap::new(),
            loopback: None,
            created: BTreeMap::new(),
            clock,
            backend,
        }
    }

    /// Starts the loopback route, replacing a running one.
    pub fn start_loopback(&mut self, cfg: &LoopbackConfig) -> Result<(), BackendError> {
        self.loopback = None;
        let target = cfg.format.frames_in_ms(cfg.latency_ms);
        let (capture, playback, stats) = port_pair(cfg.format.channels, target);

        let playback = self.backend.create_playback(
            &StreamSpec {
                name: format!("{}.playback", cfg.name),
                description: cfg.playback_description.clone(),
                format: cfg.format,
                layout: Layout::default_for(cfg.format.channels.into()),
                group: Some(cfg.name.clone()),
                remote: None,
                block_frames: cfg.block_frames,
                key: None,
                placement: Placement::Default,
                volume: None,
                process_latency: 0,
            },
            playback,
        )?;
        let sink = self.backend.create_sink(
            &StreamSpec {
                name: cfg.name.clone(),
                description: cfg.description.clone(),
                format: cfg.format,
                layout: Layout::default_for(cfg.format.channels.into()),
                group: Some(cfg.name.clone()),
                remote: None,
                block_frames: cfg.block_frames,
                key: None,
                placement: Placement::Default,
                volume: None,
                process_latency: target as u32,
            },
            capture,
        )?;
        self.loopback = Some(Loopback {
            _sink: sink,
            _playback: playback,
            stats,
        });
        Ok(())
    }

    /// Starts receiver `id`, replacing a running one with that ID.
    pub fn start_receiver(
        &mut self,
        id: ReceiverId,
        cfg: &ReceiverConfig,
    ) -> Result<(), BackendError> {
        self.receivers.remove(&id);
        let receiver = Receiver::start(self.backend.as_mut(), self.clock.clone(), cfg)?;
        self.receivers.insert(id, receiver);
        Ok(())
    }

    /// Stops a receiver; returns whether it existed.
    pub fn remove_receiver(&mut self, id: ReceiverId) -> bool {
        self.receivers.remove(&id).is_some()
    }

    /// Starts a sender, replacing one with the same ID.
    pub fn insert_sender(&mut self, id: SenderId, cfg: &SenderConfig) -> Result<(), BackendError> {
        self.senders.remove(&id);
        self.hidden.remove(&id);
        let sender = Sender::start(self.backend.as_mut(), self.clock.clone(), cfg)?;
        self.senders.insert(id, sender);
        Ok(())
    }

    /// Redirects a sender; `Ok(false)` if there is no such sender.
    pub fn set_sender_route(&mut self, id: SenderId, route: Route) -> std::io::Result<bool> {
        if let Some(h) = self.hidden.get_mut(&id) {
            h.route = route;
            return Ok(true);
        }
        match self.senders.get(&id) {
            Some(s) => s.set_route(route).map(|()| true),
            None => Ok(false),
        }
    }

    /// Removes a sender's sink until [`show_sender`](Self::show_sender),
    /// keeping its name, route and volume; nothing is sent meanwhile.
    /// Returns whether the sender runs.
    pub fn hide_sender(&mut self, id: SenderId) -> bool {
        let Some(sender) = self.senders.remove(&id) else {
            return self.hidden.contains_key(&id);
        };
        let hidden = Hidden {
            cfg: sender.config(),
            route: sender.route(),
            paused: sender.paused(),
            label: sender.label().map(str::to_owned),
        };
        drop(sender);
        self.hidden.insert(id, hidden);
        true
    }

    /// Brings a hidden sender's sink back; `Ok(false)` if it isn't hidden.
    pub fn show_sender(&mut self, id: SenderId) -> Result<bool, BackendError> {
        let Some(h) = self.hidden.remove(&id) else {
            return Ok(false);
        };
        let mut sender = match Sender::start(self.backend.as_mut(), self.clock.clone(), &h.cfg) {
            Ok(s) => s,
            Err(e) => {
                self.hidden.insert(id, h);
                return Err(e);
            }
        };
        sender.pause(h.paused);
        sender.set_label(h.label.as_deref());
        let routed = sender.set_route(h.route);
        self.senders.insert(id, sender);
        routed.map(|()| true).map_err(Into::into)
    }

    /// Whether sender `id`'s sink is hidden.
    pub fn sender_hidden(&self, id: SenderId) -> bool {
        self.hidden.contains_key(&id)
    }

    /// What the channels of sender `id`'s sink carry, also while it is
    /// hidden.
    pub fn sender_layout(&self, id: SenderId) -> Option<&Layout> {
        self.senders
            .get(&id)
            .map(Sender::layout)
            .or_else(|| self.hidden.get(&id).map(|h| &h.cfg.layout))
    }

    /// Gives sender `id`'s sink other channels: starts a new sink with
    /// `layout`, of the same name, route, volume and label, and removes
    /// the old one once the audio system shows the new one, so that what
    /// played into the old one goes on in the new one. A hidden sender
    /// keeps the layout for when it shows. If the new sink can't start,
    /// the old one stays. `Ok(false)` if there is no such sender.
    pub fn set_sender_layout(
        &mut self,
        id: SenderId,
        layout: Layout,
    ) -> Result<bool, BackendError> {
        if let Some(h) = self.hidden.get_mut(&id) {
            h.cfg.format.channels = layout.len() as u16;
            h.cfg.layout = layout;
            return Ok(true);
        }
        let Some(old) = self.senders.get(&id) else {
            return Ok(false);
        };
        let mut cfg = old.config();
        cfg.format.channels = layout.len() as u16;
        cfg.layout = layout;
        let (paused, label, route) = (old.paused(), old.label().map(str::to_owned), old.route());
        let mut new = Sender::start(self.backend.as_mut(), self.clock.clone(), &cfg)?;
        new.pause(paused);
        new.set_label(label.as_deref());
        // Both the same name: wait until the audio system shows the new
        // sink, else what plays into the old one has nowhere to go.
        let channels = cfg.layout.len();
        let before = self.sinks_named(&cfg.name, channels);
        let until = Instant::now() + SINK_TIMEOUT;
        while self.sinks_named(&cfg.name, channels) <= before && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        let routed = new.set_route(route);
        self.senders.insert(id, new);
        routed.map(|()| true).map_err(Into::into)
    }

    /// How many of the audio system's outputs are named `name` and have
    /// `channels` channels.
    fn sinks_named(&self, name: &str, channels: usize) -> usize {
        self.backend
            .outputs()
            .iter()
            .filter(|o| o.name == name && o.channels.len() == channels)
            .count()
    }

    /// Changes how long the senders fade in, for those that run and those
    /// started later with it.
    pub fn set_sender_fade_in(&mut self, ms: u32) {
        for s in self.senders.values() {
            s.set_fade_in(ms);
        }
        for h in self.hidden.values_mut() {
            h.cfg.fade_in_ms = ms;
        }
    }

    /// Moves a receiver to another UDP port; `Ok(false)` if there is no
    /// such receiver.
    pub fn set_receiver_port(&mut self, id: ReceiverId, port: u16) -> std::io::Result<bool> {
        match self.receivers.get(&id) {
            Some(r) => r.set_port(port).map(|()| true),
            None => Ok(false),
        }
    }

    /// Sets which packets a receiver plays; `Ok(false)` if there is no such
    /// receiver.
    pub fn set_receiver_filter(&mut self, id: ReceiverId, filter: Filter) -> std::io::Result<bool> {
        match self.receivers.get(&id) {
            Some(r) => r.set_filter(filter).map(|()| true),
            None => Ok(false),
        }
    }

    /// Shows a receiver's playback stream under `description`, or under
    /// its configured one for `None`.
    pub fn set_receiver_description(&mut self, id: ReceiverId, description: Option<&str>) {
        if let Some(r) = self.receivers.get(&id) {
            r.describe(description);
        }
    }

    /// Whether receiver `id` runs, and under which key.
    pub fn receiver_key(&self, id: ReceiverId) -> Option<Option<&str>> {
        self.receivers.get(&id).map(Receiver::key)
    }

    /// What the channels of receiver `id`'s stream carry, if it runs.
    pub fn receiver_layout(&self, id: ReceiverId) -> Option<&Layout> {
        self.receivers.get(&id).map(Receiver::layout)
    }

    /// The link offset receiver `id` runs with, if it runs.
    pub fn receiver_link_offset(&self, id: ReceiverId) -> Option<u32> {
        self.receivers.get(&id).map(Receiver::link_offset_ms)
    }

    /// Fades a receiver to `gain` (0 to 1); returns whether it exists.
    pub fn set_receiver_gain(&mut self, id: ReceiverId, gain: f32) -> bool {
        self.receivers.get(&id).map(|r| r.set_gain(gain)).is_some()
    }

    /// How long each running receiver's stream has been silent, in
    /// milliseconds (see [`ReceiverStatus::silent_ms`]).
    pub fn receiver_silence(&self) -> Vec<(ReceiverId, Option<u64>)> {
        self.receivers
            .iter()
            .map(|(id, r)| (*id, r.silent_ms()))
            .collect()
    }

    /// The audio system's outputs.
    pub fn outputs(&self) -> Vec<Output> {
        self.backend.outputs()
    }

    /// See [`AudioBackend::chosen_default`].
    pub fn chosen_default(&self) -> Option<String> {
        self.backend.chosen_default()
    }

    /// The streams applications play now.
    pub fn app_streams(&self) -> Vec<AppStream> {
        self.backend.app_streams()
    }

    /// The audio system's inputs.
    pub fn inputs(&self) -> Vec<Input> {
        self.backend.inputs()
    }

    /// Creates an output on chosen channels of another, replacing one of
    /// the same name; see [`AudioBackend::create_output`].
    pub fn add_output(&mut self, spec: &OutputSpec) -> Result<(), BackendError> {
        self.created.remove(&spec.name);
        let handle = self.backend.create_output(spec)?;
        self.created.insert(spec.name.clone(), handle);
        Ok(())
    }

    /// Removes a created output; `false` if there is none of that name.
    pub fn remove_output(&mut self, name: &str) -> bool {
        self.created.remove(name).is_some()
    }

    /// Keeps exactly these links made; see [`AudioBackend::set_links`].
    pub fn set_links(&self, links: &[Link]) -> Result<(), BackendError> {
        self.backend.set_links(links)
    }

    /// The links asked for that are made now.
    pub fn links(&self) -> Vec<Link> {
        self.backend.links()
    }

    /// How loud the links and their outputs play; see
    /// [`AudioBackend::levels`].
    pub fn levels(&self) -> crate::Levels {
        self.backend.levels()
    }

    /// Makes output `name` the default one; see
    /// [`AudioBackend::set_default_output`].
    pub fn set_default_output(&self, name: &str) -> Result<(), BackendError> {
        self.backend.set_default_output(name)
    }

    /// The output a receiver plays on, if its stream is linked to one.
    pub fn receiver_output(&self, id: ReceiverId) -> Option<String> {
        self.receivers.get(&id).and_then(Receiver::output)
    }

    /// Moves a receiver's stream to output `name`, or with `None` back to
    /// the default output; `Ok(false)` if the receiver is not running.
    pub fn set_receiver_output(
        &self,
        id: ReceiverId,
        name: Option<&str>,
    ) -> Result<bool, BackendError> {
        match self.receivers.get(&id) {
            Some(r) => r.set_output(name).map(|()| true),
            None => Ok(false),
        }
    }

    /// The output the user last moved receiver `id`'s stream to; see
    /// [`StreamControl::chosen_output`](crate::StreamControl::chosen_output).
    pub fn receiver_chosen_output(&self, id: ReceiverId) -> Option<Option<String>> {
        self.receivers.get(&id)?.playback().chosen_output()
    }

    /// Plays receiver `id`'s stream where `placement` says; `Ok(false)` if
    /// the receiver is not running.
    pub fn place_receiver(
        &self,
        id: ReceiverId,
        placement: &Placement,
    ) -> Result<bool, BackendError> {
        match self.receivers.get(&id) {
            Some(r) => r.playback().place(placement).map(|()| true),
            None => Ok(false),
        }
    }

    /// The volume of receiver `id`'s stream, once the audio system has said.
    pub fn receiver_volume(&self, id: ReceiverId) -> Option<Volume> {
        self.receivers.get(&id)?.playback().volume()
    }

    /// Sets the volume of receiver `id`'s stream; returns whether it runs.
    pub fn set_receiver_volume(&self, id: ReceiverId, volume: &Volume) -> bool {
        self.receivers
            .get(&id)
            .map(|r| r.playback().set_volume(volume))
            .is_some()
    }

    /// Changes the kind of output a sender's sink shows; returns whether
    /// the sender exists.
    pub fn set_sender_remote(&mut self, id: SenderId, remote: OutputType) -> bool {
        if let Some(h) = self.hidden.get_mut(&id) {
            h.cfg.remote = remote;
            return true;
        }
        self.senders
            .get_mut(&id)
            .map(|s| s.set_remote(remote))
            .is_some()
    }

    /// The volume of a sender's sink, once the audio system has said.
    pub fn sender_volume(&self, id: SenderId) -> Option<Volume> {
        if let Some(h) = self.hidden.get(&id) {
            return h.cfg.volume.clone();
        }
        self.senders.get(&id)?.sink().volume()
    }

    /// Shows a sender's sink under another name; returns whether the
    /// sender exists.
    pub fn rename_sender(&mut self, id: SenderId, name: &str) -> bool {
        if let Some(h) = self.hidden.get_mut(&id) {
            h.cfg.description = name.to_owned();
            return true;
        }
        self.senders.get_mut(&id).map(|s| s.rename(name)).is_some()
    }

    /// Shows a sender's sink under `label` instead of its name, or under
    /// its name again; the name itself stays. Returns whether the sender
    /// exists.
    pub fn set_sender_label(&mut self, id: SenderId, label: Option<&str>) -> bool {
        if let Some(h) = self.hidden.get_mut(&id) {
            h.label = label.map(str::to_owned);
            return true;
        }
        self.senders
            .get_mut(&id)
            .map(|s| s.set_label(label))
            .is_some()
    }

    /// The name of a sender's sink, without a label.
    pub fn sender_name(&self, id: SenderId) -> Option<String> {
        match self.hidden.get(&id) {
            Some(h) => Some(h.cfg.description.clone()),
            None => Some(self.senders.get(&id)?.description().to_owned()),
        }
    }

    /// A sender's label, if it has one.
    pub fn sender_label(&self, id: SenderId) -> Option<&str> {
        match self.hidden.get(&id) {
            Some(h) => h.label.as_deref(),
            None => self.senders.get(&id)?.label(),
        }
    }

    /// The machine name of a sender's sink, also while it is hidden.
    pub fn sender_sink(&self, id: SenderId) -> Option<&str> {
        match self.hidden.get(&id) {
            Some(h) => Some(&h.cfg.name),
            None => Some(self.senders.get(&id)?.sink_name()),
        }
    }

    /// Whether anything plays into a sender's sink: `None` if it is hidden
    /// or the audio system can't tell.
    pub fn sender_fed(&self, id: SenderId) -> Option<bool> {
        self.senders.get(&id)?.sink().fed()
    }

    /// Holds a sender's packets back, or lets them go again, whatever its
    /// route says; returns whether the sender exists.
    pub fn pause_sender(&mut self, id: SenderId, paused: bool) -> bool {
        if let Some(h) = self.hidden.get_mut(&id) {
            h.paused = paused;
            return true;
        }
        self.senders.get(&id).map(|s| s.pause(paused)).is_some()
    }

    /// Stops a sender; returns whether it existed.
    pub fn remove_sender(&mut self, id: SenderId) -> bool {
        let hidden = self.hidden.remove(&id).is_some();
        self.senders.remove(&id).is_some() || hidden
    }

    pub fn status(&self) -> EngineStatus {
        EngineStatus {
            loopback: self.loopback.as_ref().map(|l| l.stats.snapshot()),
            senders: {
                let mut senders: Vec<_> = self
                    .senders
                    .iter()
                    .map(|(id, s)| (*id, s.status()))
                    .chain(
                        self.hidden
                            .iter()
                            .map(|(id, h)| (*id, SenderStatus::hidden(&h.cfg, h.route))),
                    )
                    .collect();
                senders.sort_by_key(|(id, _)| *id);
                senders
            },
            receivers: self
                .receivers
                .iter()
                .map(|(id, r)| (*id, r.status()))
                .collect(),
        }
    }
}
