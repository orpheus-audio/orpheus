//! The engine: owns the backend and the audio routes built on top of it.

use std::collections::BTreeMap;
use std::sync::Arc;

use orpheus_clock::Clock;
use orpheus_core::AudioFormat;

use crate::backend::{AudioBackend, BackendError, StreamHandle, StreamSpec};
use crate::port::{PortStats, StatsSnapshot, port_pair};
use crate::receiver::{Receiver, ReceiverConfig, ReceiverStatus};
use crate::sender::{Sender, SenderConfig, SenderStatus};

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
    /// Output node to play on; `None` follows the default output.
    pub target: Option<String>,
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
                target: cfg.target.clone(),
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
                target: None,
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
        let sender = Sender::start(self.backend.as_mut(), self.clock.clone(), cfg)?;
        let id = self.next_sender;
        self.next_sender += 1;
        self.senders.insert(id, sender);
        Ok(id)
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
