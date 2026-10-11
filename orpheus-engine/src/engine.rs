//! The engine: owns the backend and the audio routes built on top of it.

use std::sync::Arc;

use orpheus_core::AudioFormat;

use crate::backend::{AudioBackend, BackendError, StreamHandle, StreamSpec};
use crate::port::{PortStats, port_pair};

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
}

pub struct Engine {
    backend: Box<dyn AudioBackend>,
    loopback: Option<Loopback>,
}

impl Engine {
    pub fn new(backend: Box<dyn AudioBackend>) -> Self {
        Self {
            backend,
            loopback: None,
        }
    }

    /// Starts the loopback route, replacing a running one.
    pub fn start_loopback(&mut self, cfg: &LoopbackConfig) -> Result<Arc<PortStats>, BackendError> {
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
                quantum: cfg.quantum,
                process_latency: target as u32,
            },
            capture,
        )?;
        self.loopback = Some(Loopback {
            _sink: sink,
            _playback: playback,
        });
        Ok(stats)
    }
}
