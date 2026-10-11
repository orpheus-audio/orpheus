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
    /// Output node to play on; `None` follows the default output.
    pub target: Option<String>,
}

struct Loopback {
    // Dropped in declaration order: stop playback before removing the sink.
    _playback: StreamHandle,
    _sink: StreamHandle,
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

        let sink = self.backend.create_sink(
            &StreamSpec {
                name: cfg.name.clone(),
                description: cfg.description.clone(),
                format: cfg.format,
                target: None,
            },
            capture,
        )?;
        let playback = self.backend.create_playback(
            &StreamSpec {
                name: format!("{}.playback", cfg.name),
                description: format!("{} (playback)", cfg.description),
                format: cfg.format,
                target: cfg.target.clone(),
            },
            playback,
        )?;
        self.loopback = Some(Loopback {
            _playback: playback,
            _sink: sink,
        });
        Ok(stats)
    }
}
