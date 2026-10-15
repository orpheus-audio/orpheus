//! Core types shared by all Orpheus nodes (Linux daemon, ESP firmware, UI backend).

#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod channels;
pub mod format;
pub mod id;
#[cfg(feature = "alloc")]
pub mod jitter;
pub mod layout;
pub mod output;
pub mod rtp;
pub mod timebase;

pub use channels::ChannelMap;
pub use format::AudioFormat;
pub use id::{DeviceId, RouteId, SourceId};
#[cfg(feature = "alloc")]
pub use layout::Layout;
pub use layout::Symbol;
pub use output::OutputType;
