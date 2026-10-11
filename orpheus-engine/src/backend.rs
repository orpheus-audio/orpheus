//! The interface between the engine and a platform audio system.

use std::error::Error;

use orpheus_core::AudioFormat;

use crate::port::{CapturePort, PlaybackPort};

pub type BackendError = Box<dyn Error + Send + Sync>;

/// What to create in the audio system.
#[derive(Debug, Clone)]
pub struct StreamSpec {
    /// Stable machine name, e.g. `orpheus.loopback`.
    pub name: String,
    /// Name shown to the user, e.g. "Orpheus → Kitchen".
    pub description: String,
    pub format: AudioFormat,
    /// Node to connect to instead of the system default (playback only).
    pub target: Option<String>,
}

/// A platform audio system (PipeWire, later WASAPI, Core Audio).
///
/// Implementations run their realtime callbacks on their own threads and only
/// move samples between the audio system and the given port.
pub trait AudioBackend: Send {
    /// Creates a virtual output device that applications can play into. Its
    /// audio is pushed into `port`.
    fn create_sink(
        &mut self,
        spec: &StreamSpec,
        port: CapturePort,
    ) -> Result<StreamHandle, BackendError>;

    /// Creates a playback stream that plays the audio drained from `port`.
    fn create_playback(
        &mut self,
        spec: &StreamSpec,
        port: PlaybackPort,
    ) -> Result<StreamHandle, BackendError>;
}

/// Keeps a backend stream alive; dropping it removes the stream.
pub struct StreamHandle {
    close: Option<Box<dyn FnOnce() + Send>>,
}

impl StreamHandle {
    pub fn new(close: impl FnOnce() + Send + 'static) -> Self {
        Self {
            close: Some(Box::new(close)),
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        if let Some(close) = self.close.take() {
            close();
        }
    }
}
