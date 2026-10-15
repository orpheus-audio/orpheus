//! Core types shared by all Orpheus nodes (Linux daemon, ESP firmware, UI backend).

#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod format;
pub mod id;
#[cfg(feature = "alloc")]
pub mod jitter;
pub mod output;
pub mod rtp;
pub mod timebase;

pub use format::AudioFormat;
pub use id::{DeviceId, RouteId, SourceId};
pub use output::OutputType;
