//! Core types shared by all Orpheus nodes (Linux daemon, ESP firmware, UI backend).

#![no_std]

pub mod format;
pub mod id;

pub use format::AudioFormat;
pub use id::DeviceId;
