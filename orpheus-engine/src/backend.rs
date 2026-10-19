//! The interface between the engine and a platform audio system.

use std::error::Error;

use orpheus_core::{AudioFormat, Layout, OutputType};

use crate::port::{CapturePort, PlaybackPort};

pub type BackendError = Box<dyn Error + Send + Sync>;

/// What to create in the audio system.
#[derive(Debug, Clone)]
pub struct StreamSpec {
    /// Stable machine name, e.g. `orpheus.loopback`.
    pub name: String,
    /// Name shown to the user, e.g. "Kitchen".
    pub description: String,
    /// For streams to or from another node: the kind of output on the other
    /// end, which backends show, e.g. as an icon. `None` for local streams.
    pub remote: Option<OutputType>,
    pub format: AudioFormat,
    /// What each of the `format.channels` channels carries, which the
    /// audio system uses to place them.
    pub layout: Layout,
    /// Streams with the same group run on one clock in one graph cycle, so
    /// audio passed between them never drifts.
    pub group: Option<String>,
    /// Frames per processing cycle to ask the audio system for; `None` keeps
    /// the system default.
    pub block_frames: Option<u32>,
    /// For a playback stream: what it plays, the same across restarts,
    /// for the audio system's per-application rules; e.g. the source a
    /// stream comes from. The audio system itself restores nothing for it;
    /// see `placement` and `volume`.
    pub key: Option<String>,
    /// For a playback stream: where it plays.
    pub placement: Placement,
    /// For a playback stream: the volume to start with; `None` leaves the
    /// audio system's default.
    pub volume: Option<Volume>,
    /// Latency the stream adds on top of the audio system, in frames.
    /// Reported so that players can delay video to match.
    pub process_latency: u32,
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

    /// The audio system's outputs.
    fn outputs(&self) -> Vec<Output> {
        Vec::new()
    }

    /// The output the user made the default, by machine name, also while
    /// it is missing; unlike the default in [`outputs`](Self::outputs),
    /// which the audio system changes by itself when that one goes.
    fn chosen_default(&self) -> Option<String> {
        None
    }

    /// The audio system's inputs: microphones, line-ins.
    fn inputs(&self) -> Vec<Input> {
        Vec::new()
    }

    /// Creates an output that plays on chosen channels of another one.
    /// It stays while the handle lives, also while the other output is
    /// missing, and plays there again once it is back.
    fn create_output(&mut self, _spec: &OutputSpec) -> Result<StreamHandle, BackendError> {
        Err("this audio system cannot create outputs".into())
    }

    /// Makes output `name` the default one, where streams go unless
    /// moved, as the user would in the desktop's sound settings. The
    /// audio system keeps it across restarts.
    fn set_default_output(&self, _name: &str) -> Result<(), BackendError> {
        Err("this audio system cannot change the default output".into())
    }

    /// Keeps exactly these links made, besides those the audio system
    /// makes itself, as far as their nodes and channels are there; made
    /// again once they are back. Each plays at its gain, which changes
    /// without a click; what several links bring into one channel is
    /// summed.
    fn set_links(&self, _links: &[Link]) -> Result<(), BackendError> {
        Err("this audio system cannot link channels".into())
    }

    /// Those of the links asked for with [`set_links`](Self::set_links)
    /// that are made now.
    fn links(&self) -> Vec<Link> {
        Vec::new()
    }

    /// The streams applications play now, besides Orpheus's own.
    fn app_streams(&self) -> Vec<AppStream> {
        Vec::new()
    }
}

/// A stream an application plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppStream {
    /// The ID of the process that plays it, where the audio system can
    /// tell for sure.
    pub process: Option<u32>,
    /// The outputs it plays on, by machine name.
    pub outputs: Vec<String>,
}

/// An output of the audio system, such as a sound card or another node's
/// sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Stable machine name.
    pub name: String,
    /// Name shown to the user.
    pub description: String,
    /// What the audio system says it is.
    pub kind: OutputType,
    /// Whether streams go here unless moved.
    pub default: bool,
    /// What its channels carry, in order.
    pub channels: Layout,
}

/// An input of the audio system, such as a microphone or a line-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// Stable machine name.
    pub name: String,
    /// Name shown to the user.
    pub description: String,
    /// What its channels carry, in order.
    pub channels: Layout,
}

/// An output made of chosen channels of another, such as channels 3 and 4
/// of an audio interface: what is played into it plays there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSpec {
    /// Stable machine name.
    pub name: String,
    /// Name shown to the user.
    pub description: String,
    /// What its channels carry.
    pub layout: Layout,
    /// The output it plays on, by machine name.
    pub device: String,
    /// For each of its channels, the channel of `device` it plays on,
    /// counted from 0.
    pub channels: Vec<usize>,
}

/// A link from a channel of an input, or of a receiver's stream, to a
/// channel of an output, at a gain. Nodes go by machine name, channels are
/// counted from 0.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub from: String,
    pub from_channel: usize,
    pub to: String,
    pub to_channel: usize,
    /// The factor the channel is played at: 1 as it is, 0 silent (see
    /// [`gain`](crate::gain)).
    pub gain: f32,
}


/// Where a playback stream plays.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Placement {
    /// Wherever the audio system plays streams that name no output.
    #[default]
    Default,
    /// On this output, by its machine name. While it is missing the
    /// stream plays nowhere, also after it went; placing it again links it
    /// to the output when back.
    Output(String),
    /// Nowhere: the stream is not linked to any output.
    Held,
}

/// The volume of a stream, as the audio system's mixers show it.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Volume {
    /// Linear gain per channel, 1.0 for unchanged.
    pub channels: Vec<f32>,
    pub mute: bool,
}

impl Volume {
    /// Whether `other` is the same volume to the 4th decimal, as mixers
    /// round.
    pub fn same(&self, other: &Self) -> bool {
        self.mute == other.mute
            && self.channels.len() == other.channels.len()
            && self
                .channels
                .iter()
                .zip(&other.channels)
                .all(|(a, b)| (a - b).abs() < 1e-4)
    }
}

/// What a backend can do with a running stream besides removing it.
pub trait StreamControl: Send {
    /// Changes the name shown to the user.
    fn describe(&self, _description: &str) {}
    /// Changes the kind of output at the other end (for sinks to other
    /// nodes).
    fn set_remote(&self, _remote: OutputType) {}
    /// For a playback stream: the name of the output it plays on, if it
    /// is linked to one.
    fn output(&self) -> Option<String> {
        None
    }
    /// For a playback stream: plays on output `name`, or with `None` on
    /// the default output again, as if the user moved it; see
    /// [`chosen_output`](Self::chosen_output).
    fn set_output(&self, _name: Option<&str>) -> Result<(), BackendError> {
        Err("this audio system cannot move streams".into())
    }
    /// For a playback stream: the output the user last moved it to
    /// (`Some(None)` for back to the default), in a mixer or with
    /// [`set_output`](Self::set_output); `None` if nobody has since it
    /// started or was last [placed](Self::place).
    fn chosen_output(&self) -> Option<Option<String>> {
        None
    }
    /// For a playback stream: plays it where `placement` says. Not a
    /// choice of the user's: it clears [`chosen_output`](Self::chosen_output).
    fn place(&self, _placement: &Placement) -> Result<(), BackendError> {
        Err("this audio system cannot move streams".into())
    }
    /// For a playback stream: its volume, once the audio system has said.
    fn volume(&self) -> Option<Volume> {
        None
    }
    /// For a playback stream: sets its volume.
    fn set_volume(&self, _volume: &Volume) {}
    /// For a sink: whether anything plays into it; `None` if the backend
    /// can't tell.
    fn fed(&self) -> Option<bool> {
        None
    }
}

/// Keeps a backend stream alive; dropping it removes the stream.
pub struct StreamHandle {
    close: Option<Box<dyn FnOnce() + Send>>,
    control: Option<Box<dyn StreamControl>>,
}

impl StreamHandle {
    pub fn new(close: impl FnOnce() + Send + 'static) -> Self {
        Self {
            close: Some(Box::new(close)),
            control: None,
        }
    }

    /// Lets the running stream be changed through `control`.
    pub fn with_control(mut self, control: impl StreamControl + 'static) -> Self {
        self.control = Some(Box::new(control));
        self
    }

    /// Changes the name shown to the user, where the backend can.
    pub fn describe(&self, description: &str) {
        if let Some(c) = &self.control {
            c.describe(description);
        }
    }

    /// Changes the kind of output at the other end, where the backend can.
    pub fn set_remote(&self, remote: OutputType) {
        if let Some(c) = &self.control {
            c.set_remote(remote);
        }
    }

    /// Whether anything plays into a sink, where the backend can tell.
    pub fn fed(&self) -> Option<bool> {
        self.control.as_ref().and_then(|c| c.fed())
    }

    /// The output a playback stream plays on, where the backend knows.
    pub fn output(&self) -> Option<String> {
        self.control.as_ref().and_then(|c| c.output())
    }

    /// Moves a playback stream to output `name`, or with `None` back to
    /// the default output, as the user would.
    pub fn set_output(&self, name: Option<&str>) -> Result<(), BackendError> {
        match &self.control {
            Some(c) => c.set_output(name),
            None => Err("this audio system cannot move streams".into()),
        }
    }

    /// See [`StreamControl::chosen_output`].
    pub fn chosen_output(&self) -> Option<Option<String>> {
        self.control.as_ref().and_then(|c| c.chosen_output())
    }

    /// See [`StreamControl::place`].
    pub fn place(&self, placement: &Placement) -> Result<(), BackendError> {
        match &self.control {
            Some(c) => c.place(placement),
            None => Err("this audio system cannot move streams".into()),
        }
    }

    /// See [`StreamControl::volume`].
    pub fn volume(&self) -> Option<Volume> {
        self.control.as_ref().and_then(|c| c.volume())
    }

    /// See [`StreamControl::set_volume`].
    pub fn set_volume(&self, volume: &Volume) {
        if let Some(c) = &self.control {
            c.set_volume(volume);
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
