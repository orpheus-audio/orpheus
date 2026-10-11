//! Platform-independent media engine for Orpheus.
//!
//! The engine moves audio between a platform [`AudioBackend`] and (later) the
//! network. Realtime threads and the engine only share lock-free
//! [ports](port).

pub mod backend;
pub mod engine;
pub mod port;

pub use backend::{AudioBackend, BackendError, StreamHandle, StreamSpec};
pub use engine::{Engine, LoopbackConfig};
pub use port::{CapturePort, PlaybackPort, PortStats, StatsSnapshot, port_pair};
