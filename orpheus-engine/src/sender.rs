//! Sender: a virtual sink whose audio goes out as an RTP stream to one peer.
//!
//! Applications play into the sink; its realtime callback pushes the audio
//! into a port. A TX thread drains the port one packet (1 ms) at a time on
//! the media-clock grid and stamps each packet with the media-clock time of
//! its first frame. The port's target covers one audio-system cycle, since
//! the sink delivers a whole cycle at once while the TX thread takes 1 ms.
//! The TX thread resamples from the sound card's clock to the media clock
//! ([`asrc`](crate::asrc)), taking a little more or less than 1 ms from the
//! port so that its level stays put.
//!
//! The TX thread also notes whether the sink plays (applications feed it)
//! and when it last carried sound, so that the node can claim a receiver
//! when audio starts and release it when the sink has been silent for a
//! while. When it starts sending again after a pause, it fades in.

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use orpheus_clock::Clock;
use orpheus_core::rtp::{Encoding, HEADER_LEN, Header};
use orpheus_core::timebase::MediaClock;
use orpheus_core::{AudioFormat, Layout, OutputType};

use crate::asrc::{Asrc, Fixed};
use crate::backend::{AudioBackend, BackendError, Placement, StreamHandle, StreamSpec, Volume};
use crate::grid::{Grid, Tick};
use crate::port::{PlaybackPort, PortStats, StatsSnapshot, port_pair};
use crate::receiver::SOUND_THRESHOLD;

/// Dynamic payload type; AES67 leaves the number to the SDP.
pub const PAYLOAD_TYPE: u8 = 97;

/// Block size the sink asks for when none is configured (PipeWire's default
/// quantum). The sink always asks for the block its buffer is sized for:
/// otherwise a client asking for more latency raises the cycle beyond it
/// and the buffer runs dry between cycles.
const DEFAULT_BLOCK_FRAMES: u32 = 1024;

/// Extra buffering for scheduling jitter between the sink and the TX thread.
const JITTER_MARGIN_MS: u32 = 2;

#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// Machine name of the sink, e.g. `orpheus.send.K7QM3XRA`.
    pub name: String,
    /// Name of the sink shown to the user.
    pub description: String,
    pub format: AudioFormat,
    pub encoding: Encoding,
    pub peer: SocketAddr,
    /// The kind of output at the other end, which the sink shows.
    pub remote: OutputType,
    /// Frames per audio-system cycle, as configured.
    pub block_frames: Option<u32>,
    /// The sink's volume to start with; `None` leaves the audio system's
    /// default.
    pub volume: Option<Volume>,
    /// How long sending fades in when it starts or resumes, so that a
    /// stream cut in mid-sound doesn't click.
    pub fade_in_ms: u32,
}

/// How long after audio last arrived from the sink it counts as playing;
/// longer than the audio system's longest cycles.
const FED_NS: u64 = 300_000_000;

/// Hops a multicast stream may take; AES67 devices commonly use 32.
const MULTICAST_TTL: u32 = 32;

/// Where a sender's packets go, and from where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    /// A unicast address or a multicast group.
    pub peer: SocketAddr,
    /// Address and port to send from; unspecified parts are chosen by the
    /// system.
    pub source: SocketAddr,
    /// Off: the sink keeps taking audio, but nothing is sent.
    pub enabled: bool,
}

impl Route {
    /// To `peer` from any address, enabled.
    pub fn to(peer: SocketAddr) -> Self {
        let source = match peer {
            SocketAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
            SocketAddr::V6(_) => ([0u16; 8], 0).into(),
        };
        Self {
            peer,
            source,
            enabled: true,
        }
    }
}

/// The socket and the route it serves, shared with the TX thread.
struct Link {
    /// `None` only while a failed rebind could not get the old one back.
    socket: Option<UdpSocket>,
    route: Route,
    /// Held back regardless of the route, e.g. while the peer's receiver
    /// plays another sender.
    paused: bool,
}

impl Link {
    fn open(route: Route) -> io::Result<Self> {
        Ok(Self {
            socket: Some(bind(route.source)?),
            route,
            paused: false,
        })
    }

    /// Moves to a new source. The old socket goes first, since the new one
    /// may want its port; if the new one can't bind, the old comes back.
    fn rebind(&mut self, route: Route) -> io::Result<()> {
        let old = self.socket.take().and_then(|s| s.local_addr().ok());
        match bind(route.source) {
            Ok(socket) => {
                self.socket = Some(socket);
                self.route = route;
                Ok(())
            }
            Err(e) => {
                self.socket = old.and_then(|a| bind(a).ok());
                Err(e)
            }
        }
    }
}

/// A socket sending from `source`, multicast through the interface of
/// `source` if it names one. It doesn't block: when the link stalls (Wi-Fi)
/// and the send queue is full, a packet is dropped and counted as a send
/// error instead of stalling the TX thread, which would fall behind the
/// media clock and hold the link lock meanwhile.
fn bind(source: SocketAddr) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(source)?;
    socket.set_nonblocking(true)?;
    match source.ip() {
        IpAddr::V4(ip) => {
            socket.set_multicast_ttl_v4(MULTICAST_TTL)?;
            if !ip.is_unspecified() {
                socket2::SockRef::from(&socket).set_multicast_if_v4(&ip)?;
            }
        }
        IpAddr::V6(_) => socket2::SockRef::from(&socket).set_multicast_hops_v6(MULTICAST_TTL)?,
    }
    Ok(socket)
}

#[derive(Debug, Default)]
struct Counters {
    packets: AtomicU64,
    send_errors: AtomicU64,
    realigns: AtomicU64,
    /// `f64` bits.
    asrc_ppm: AtomicU64,
    /// Media-clock time the sink last carried sound; 0 if it never did.
    sound_ns: AtomicU64,
    /// Media-clock time audio last arrived from the sink; 0 if it never
    /// did.
    fed_ns: AtomicU64,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq)]
pub struct SenderStatus {
    /// Name of the sink, as shown to the user.
    pub name: String,
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "192.168.1.20:5004"))]
    pub peer: SocketAddr,
    /// Whether packets go out.
    pub enabled: bool,
    /// The port packets are sent from.
    pub source_port: u16,
    pub packets: u64,
    pub send_errors: u64,
    /// The TX thread was stalled or the clock jumped.
    pub realigns: u64,
    /// Rate correction from the sound card's clock to the media clock.
    pub asrc_ppm: f64,
    /// The buffer between the sink and the TX thread.
    pub buffer: StatsSnapshot,
    /// Applications play into the sink: audio arrives, if only silence.
    pub playing: bool,
    /// Time since the sink last carried sound (above −60 dBFS), in
    /// milliseconds; `None` if it never did.
    pub silent_ms: Option<u64>,
    /// The sink is gone for now, while the peer is offline.
    pub hidden: bool,
}

pub(crate) struct Sender {
    /// What it was started with, kept up to date with renames and the
    /// like, so that it can be started again the same.
    cfg: SenderConfig,
    link: Arc<Mutex<Link>>,
    counters: Arc<Counters>,
    buffer: Arc<PortStats>,
    /// Fade-in length in frames.
    fade_frames: Arc<AtomicU32>,
    clock: Arc<dyn Clock>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    sink: StreamHandle,
}

impl Sender {
    pub fn sink(&self) -> &StreamHandle {
        &self.sink
    }

    /// Changes the kind of output the sink shows.
    pub fn set_remote(&mut self, remote: OutputType) {
        self.sink.set_remote(remote);
        self.cfg.remote = remote;
    }

    /// Shows the sink under another name.
    pub fn rename(&mut self, name: &str) {
        self.sink.describe(name);
        self.cfg.description = name.to_owned();
    }

    /// Stops or resumes sending, whatever the route says.
    pub fn pause(&self, paused: bool) {
        self.link.lock().unwrap().paused = paused;
    }

    /// Whether sending is held back, whatever the route says.
    pub fn paused(&self) -> bool {
        self.link.lock().unwrap().paused
    }

    pub fn route(&self) -> Route {
        self.link.lock().unwrap().route
    }

    /// Changes how long sending fades in.
    pub fn set_fade_in(&self, ms: u32) {
        let frames = self.cfg.format.frames_in_ms(ms) as u32;
        self.fade_frames.store(frames, Relaxed);
    }

    /// What it would start with again: the configuration with the name,
    /// output type, route and volume it has now.
    pub fn config(&self) -> SenderConfig {
        SenderConfig {
            peer: self.route().peer,
            volume: self.sink.volume().or_else(|| self.cfg.volume.clone()),
            ..self.cfg.clone()
        }
    }

    pub fn start(
        backend: &mut dyn AudioBackend,
        clock: Arc<dyn Clock>,
        cfg: &SenderConfig,
    ) -> Result<Self, BackendError> {
        let format = cfg.format;
        let block_frames = cfg.block_frames.unwrap_or(DEFAULT_BLOCK_FRAMES);
        let target = block_frames as usize + format.frames_in_ms(JITTER_MARGIN_MS);
        let (capture, playback, buffer) = port_pair(format.channels, target);

        let link = Arc::new(Mutex::new(Link::open(Route::to(cfg.peer))?));
        let asrc = Asrc::new(
            format.rate,
            format.channels as usize,
            format.frames_in_ms(1),
            Fixed::Output,
            0.001,
        );
        let sink = backend.create_sink(
            &StreamSpec {
                name: cfg.name.clone(),
                description: cfg.description.clone(),
                format,
                layout: Layout::default_for(format.channels.into()),
                group: None,
                remote: Some(cfg.remote),
                block_frames: Some(block_frames),
                key: None,
                placement: Placement::Default,
                volume: cfg.volume.clone(),
                process_latency: (target + asrc.delay()) as u32,
            },
            capture,
        )?;

        let counters = Arc::new(Counters::default());
        let fade_frames = Arc::new(AtomicU32::new(format.frames_in_ms(cfg.fade_in_ms) as u32));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new().name("orpheus-tx".into()).spawn({
            let tx = Tx {
                port: playback,
                link: link.clone(),
                clock: clock.clone(),
                format,
                encoding: cfg.encoding,
                asrc,
                buffer: buffer.clone(),
                counters: counters.clone(),
                fade_frames: fade_frames.clone(),
            };
            let stop = stop.clone();
            move || tx.run(&stop)
        })?;
        Ok(Self {
            cfg: cfg.clone(),
            link,
            counters,
            buffer,
            fade_frames,
            clock,
            stop,
            thread: Some(thread),
            sink,
        })
    }

    /// Redirects the stream; opens a new socket if the source changes.
    pub fn set_route(&self, route: Route) -> io::Result<()> {
        let mut link = self.link.lock().unwrap();
        if link.route.source != route.source || link.socket.is_none() {
            link.rebind(route)?;
        } else {
            link.route = route;
        }
        Ok(())
    }

    /// Time since the sink last carried sound, in milliseconds; `None` if
    /// it never did.
    pub fn silent_ms(&self) -> Option<u64> {
        let sound = self.counters.sound_ns.load(Relaxed);
        (sound != 0).then(|| self.clock.now_ns().saturating_sub(sound) / 1_000_000)
    }

    /// Whether audio arrived from the sink lately.
    fn playing(&self) -> bool {
        let fed = self.counters.fed_ns.load(Relaxed);
        fed != 0 && self.clock.now_ns().saturating_sub(fed) < FED_NS
    }

    pub fn status(&self) -> SenderStatus {
        let link = self.link.lock().unwrap();
        SenderStatus {
            name: self.cfg.description.clone(),
            peer: link.route.peer,
            enabled: link.route.enabled && !link.paused,
            source_port: link
                .socket
                .as_ref()
                .and_then(|s| s.local_addr().ok())
                .map_or(0, |a| a.port()),
            packets: self.counters.packets.load(Relaxed),
            send_errors: self.counters.send_errors.load(Relaxed),
            realigns: self.counters.realigns.load(Relaxed),
            asrc_ppm: f64::from_bits(self.counters.asrc_ppm.load(Relaxed)),
            buffer: self.buffer.snapshot(),
            playing: self.playing(),
            silent_ms: self.silent_ms(),
            hidden: false,
        }
    }
}

impl SenderStatus {
    /// The status of a sender whose sink is hidden, started with `cfg`.
    pub(crate) fn hidden(cfg: &SenderConfig, route: Route) -> Self {
        Self {
            name: cfg.description.clone(),
            peer: route.peer,
            enabled: false,
            source_port: 0,
            packets: 0,
            send_errors: 0,
            realigns: 0,
            asrc_ppm: 0.0,
            buffer: StatsSnapshot::default(),
            playing: false,
            silent_ms: None,
            hidden: true,
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Tx {
    port: PlaybackPort,
    link: Arc<Mutex<Link>>,
    clock: Arc<dyn Clock>,
    format: AudioFormat,
    encoding: Encoding,
    asrc: Asrc,
    buffer: Arc<PortStats>,
    counters: Arc<Counters>,
    fade_frames: Arc<AtomicU32>,
}

impl Tx {
    fn run(mut self, stop: &AtomicBool) {
        let ptime = self.format.frames_in_ms(1);
        let ch = self.format.channels as usize;
        let mut input = vec![0.0f32; self.asrc.input_frames_max() * ch];
        let mut samples = vec![0.0f32; ptime * ch];
        let mut packet = vec![0u8; HEADER_LEN + samples.len() * self.encoding.bytes_per_sample()];
        let mut header = Header {
            marker: true,
            payload_type: PAYLOAD_TYPE,
            sequence: random() as u16,
            timestamp: 0,
            ssrc: random(),
        };
        let media = MediaClock::new(self.format.rate);
        let mut grid = Grid::new(media, ptime as u64, self.clock.now_ns());
        // Frames sent since sending (re)started, for the fade-in.
        let mut sent = 0u32;
        let mut frames_in = 0;

        while !stop.load(Relaxed) {
            // A packet goes out once its last frame is due.
            match grid.poll(self.clock.now_ns(), ptime as u64) {
                Tick::Wait(d) => {
                    thread::sleep(d);
                    continue;
                }
                Tick::Realigned => {
                    self.counters.realigns.fetch_add(1, Relaxed);
                    self.asrc.restart();
                    header.marker = true;
                    continue;
                }
                Tick::Due => {}
            }
            let need = self.asrc.input_frames_next() * ch;
            self.port.fill(&mut input[..need]);
            if self.buffer.frames_in() != frames_in {
                frames_in = self.buffer.frames_in();
                self.counters
                    .fed_ns
                    .store(self.clock.now_ns().max(1), Relaxed);
            }
            self.meter(&input[..need]);
            self.asrc.process(&input[..need], &mut samples);
            self.asrc.control(
                self.port.level(),
                self.buffer.generation(),
                self.buffer.playing(),
            );
            self.counters
                .asrc_ppm
                .store(self.asrc.correction_ppm().to_bits(), Relaxed);
            header.timestamp = grid.next() as u32;
            header.write((&mut packet[..HEADER_LEN]).try_into().unwrap());
            let link = self.link.lock().unwrap();
            if link.route.enabled
                && !link.paused
                && let Some(socket) = &link.socket
            {
                let length = self.fade_frames.load(Relaxed);
                sent = fade(&mut samples, self.format.channels as usize, length, sent);
                self.encoding.encode(&samples, &mut packet[HEADER_LEN..]);
                match socket.send_to(&packet, link.route.peer) {
                    Ok(_) => self.counters.packets.fetch_add(1, Relaxed),
                    Err(_) => self.counters.send_errors.fetch_add(1, Relaxed),
                };
            } else {
                sent = 0;
            }
            drop(link);
            header.sequence = header.sequence.wrapping_add(1);
            header.marker = false;
            grid.advance();
        }
    }
}

impl Tx {
    /// Notes whether the block carries sound.
    fn meter(&self, block: &[f32]) {
        if block.iter().any(|s| s.abs() > SOUND_THRESHOLD) {
            self.counters
                .sound_ns
                .store(self.clock.now_ns().max(1), Relaxed);
        }
    }
}

/// Fades interleaved `samples` in over `length` frames, of which `sent`
/// have gone out before; returns the frames sent with these.
fn fade(samples: &mut [f32], channels: usize, length: u32, sent: u32) -> u32 {
    if sent < length {
        for (i, frame) in samples.chunks_exact_mut(channels).enumerate() {
            let gain = ((sent + i as u32) as f32 / length as f32).min(1.0);
            frame.iter_mut().for_each(|s| *s *= gain);
        }
    }
    sent.saturating_add((samples.len() / channels) as u32)
}

fn random() -> u32 {
    getrandom::u32().unwrap_or(0x6f72_7068)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fades_in_across_packets() {
        let mut a = [1.0f32; 8]; // 4 stereo frames
        assert_eq!(fade(&mut a, 2, 8, 0), 4);
        assert_eq!(a, [0.0, 0.0, 0.125, 0.125, 0.25, 0.25, 0.375, 0.375]);
        let mut b = [1.0f32; 12];
        assert_eq!(fade(&mut b, 2, 8, 4), 10);
        assert_eq!(b[..2], [0.5, 0.5]);
        assert_eq!(b[8..], [1.0; 4], "full gain once the fade is over");
        let mut c = [1.0f32; 4];
        fade(&mut c, 2, 0, 0);
        assert_eq!(c, [1.0; 4], "no fade");
    }
}
