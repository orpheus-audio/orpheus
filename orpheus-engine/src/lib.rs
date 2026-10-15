//! Platform-independent media engine for Orpheus.
//!
//! The engine moves audio between a platform [`AudioBackend`] and the
//! network. Realtime threads and the engine only share lock-free
//! [ports](port).

mod asrc;
pub mod backend;
pub mod engine;
mod grid;
pub mod port;
pub mod receiver;
pub mod sender;

pub use backend::{
    AudioBackend, BackendError, Output, Placement, StreamControl, StreamHandle, StreamSpec, Volume,
};
pub use engine::{Engine, EngineStatus, LoopbackConfig, ReceiverId, SenderId};
pub use port::{CapturePort, PlaybackPort, PortStats, StatsSnapshot, port_pair};
pub use receiver::{Filter, Group, RAMP_MS, ReceiverConfig, ReceiverStatus, SOUND_THRESHOLD};
pub use sender::{Route, SenderConfig, SenderStatus};
