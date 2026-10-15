//! The engine: owns the backend and the audio routes built on top of it.

use std::collections::BTreeMap;
use std::sync::Arc;

use orpheus_clock::Clock;
use orpheus_core::{AudioFormat, OutputType};

use crate::backend::{
    AudioBackend, BackendError, Output, Placement, StreamHandle, StreamSpec, Volume,
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
}

pub struct Engine {
    // Routes go before the backend they live in.
    senders: BTreeMap<SenderId, Sender>,
    hidden: BTreeMap<SenderId, Hidden>,
    receivers: BTreeMap<ReceiverId, Receiver>,
    loopback: Option<Loopback>,
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
                description: format!("{} (playback)", cfg.description),
                format: cfg.format,
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
        let sender = match Sender::start(self.backend.as_mut(), self.clock.clone(), &h.cfg) {
            Ok(s) => s,
            Err(e) => {
                self.hidden.insert(id, h);
                return Err(e);
            }
        };
        sender.pause(h.paused);
        let routed = sender.set_route(h.route);
        self.senders.insert(id, sender);
        routed.map(|()| true).map_err(Into::into)
    }

    /// Whether sender `id`'s sink is hidden.
    pub fn sender_hidden(&self, id: SenderId) -> bool {
        self.hidden.contains_key(&id)
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
