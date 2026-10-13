//! The engine: owns the backend and the audio routes built on top of it.

use std::collections::BTreeMap;
use std::sync::Arc;

use orpheus_clock::Clock;
use orpheus_core::{AudioFormat, OutputType};

use crate::backend::{AudioBackend, BackendError, Output, StreamHandle, StreamSpec};
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
    pub quantum: Option<u32>,
}

struct Loopback {
    // Dropped in declaration order: the sink drives the playback stream, so
    // it goes first.
    _sink: StreamHandle,
    _playback: StreamHandle,
    stats: Arc<PortStats>,
}

pub type SenderId = u32;

/// A snapshot of everything the engine runs.
#[derive(Debug, Clone, Default)]
pub struct EngineStatus {
    pub loopback: Option<StatsSnapshot>,
    pub senders: Vec<(SenderId, SenderStatus)>,
    pub receiver: Option<ReceiverStatus>,
}

pub struct Engine {
    // Routes go before the backend they live in.
    senders: BTreeMap<SenderId, Sender>,
    next_sender: SenderId,
    receiver: Option<Receiver>,
    loopback: Option<Loopback>,
    clock: Arc<dyn Clock>,
    backend: Box<dyn AudioBackend>,
}

impl Engine {
    pub fn new(backend: Box<dyn AudioBackend>, clock: Arc<dyn Clock>) -> Self {
        Self {
            senders: BTreeMap::new(),
            next_sender: 0,
            receiver: None,
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
                quantum: cfg.quantum,
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
                quantum: cfg.quantum,
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

    /// Starts receiving, replacing a running receiver.
    pub fn start_receiver(&mut self, cfg: &ReceiverConfig) -> Result<(), BackendError> {
        self.receiver = None;
        self.receiver = Some(Receiver::start(
            self.backend.as_mut(),
            self.clock.clone(),
            cfg,
        )?);
        Ok(())
    }

    /// The ID the next sender will get, for naming it.
    pub fn next_sender_id(&self) -> SenderId {
        self.next_sender
    }

    /// Starts a sender and returns its ID.
    pub fn add_sender(&mut self, cfg: &SenderConfig) -> Result<SenderId, BackendError> {
        let id = self.next_sender;
        self.insert_sender(id, cfg)?;
        Ok(id)
    }

    /// Starts a sender under a given ID, e.g. one restored from before a
    /// restart, replacing a sender with that ID.
    pub fn insert_sender(&mut self, id: SenderId, cfg: &SenderConfig) -> Result<(), BackendError> {
        self.senders.remove(&id);
        let sender = Sender::start(self.backend.as_mut(), self.clock.clone(), cfg)?;
        self.next_sender = self.next_sender.max(id + 1);
        self.senders.insert(id, sender);
        Ok(())
    }

    /// Redirects a sender; `Ok(false)` if there is no such sender.
    pub fn set_sender_route(&mut self, id: SenderId, route: Route) -> std::io::Result<bool> {
        match self.senders.get(&id) {
            Some(s) => s.set_route(route).map(|()| true),
            None => Ok(false),
        }
    }

    /// Moves the receiver to another UDP port; `Ok(false)` if there is no
    /// receiver.
    pub fn set_receiver_port(&mut self, port: u16) -> std::io::Result<bool> {
        match &self.receiver {
            Some(r) => r.set_port(port).map(|()| true),
            None => Ok(false),
        }
    }

    /// Sets which packets the receiver plays; `Ok(false)` if there is no
    /// receiver.
    pub fn set_receiver_filter(&mut self, filter: Filter) -> std::io::Result<bool> {
        match &self.receiver {
            Some(r) => r.set_filter(filter).map(|()| true),
            None => Ok(false),
        }
    }

    /// Shows the receiver's playback stream under `description`, or under
    /// its configured one for `None`.
    pub fn set_receiver_description(&mut self, description: Option<&str>) {
        if let Some(r) = &self.receiver {
            r.describe(description);
        }
    }

    /// The audio system's outputs.
    pub fn outputs(&self) -> Vec<Output> {
        self.backend.outputs()
    }

    /// The output the receiver plays on, if its stream is linked to one.
    pub fn receiver_output(&self) -> Option<String> {
        self.receiver.as_ref().and_then(Receiver::output)
    }

    /// Changes the kind of output a sender's sink shows; returns whether
    /// the sender exists.
    pub fn set_sender_remote(&mut self, id: SenderId, remote: OutputType) -> bool {
        self.senders
            .get(&id)
            .map(|s| s.set_remote(remote))
            .is_some()
    }

    /// Shows a sender's sink under another name; returns whether the
    /// sender exists.
    pub fn rename_sender(&mut self, id: SenderId, name: &str) -> bool {
        self.senders.get_mut(&id).map(|s| s.rename(name)).is_some()
    }

    /// Holds a sender's packets back, or lets them go again, whatever its
    /// route says; returns whether the sender exists.
    pub fn pause_sender(&mut self, id: SenderId, paused: bool) -> bool {
        self.senders.get(&id).map(|s| s.pause(paused)).is_some()
    }

    /// Stops a sender; returns whether it existed.
    pub fn remove_sender(&mut self, id: SenderId) -> bool {
        self.senders.remove(&id).is_some()
    }

    pub fn status(&self) -> EngineStatus {
        EngineStatus {
            loopback: self.loopback.as_ref().map(|l| l.stats.snapshot()),
            senders: self
                .senders
                .iter()
                .map(|(id, s)| (*id, s.status()))
                .collect(),
            receiver: self.receiver.as_ref().map(Receiver::status),
        }
    }
}
