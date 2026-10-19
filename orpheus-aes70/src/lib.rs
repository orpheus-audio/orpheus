//! The device side of AES70 (Open Control Architecture), for controllers
//! such as AES70 Explorer to see and set a node's gains and meters.
//!
//! [`ocp1`] reads and writes OCP.1, AES70's binary protocol; [`device`]
//! holds the objects a node shows and answers controllers' commands. How
//! the bytes travel (TCP, WebSocket) is the host's business, as is what
//! the objects stand for.

#![no_std]

extern crate alloc;

pub mod device;
pub mod ocp1;

pub use device::{Change, Device, Handled, Identity, Kind, Law, Object, ROOT, Session, Set, Tree};
pub use ocp1::{Command, Incoming, Outgoing, Status};
