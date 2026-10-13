//! Receiver: plays the RTP stream arriving on a UDP port.
//!
//! An RX thread stores incoming packets in a jitter buffer and, on the
//! media-clock grid, takes out the audio stamped `link_offset` earlier than
//! the current media-clock time. That audio is resampled from the media
//! clock to the sound card's clock ([`asrc`](crate::asrc)) and goes through
//! a port to a playback stream. Every node shares the media clock, so audio from one
//! sender plays at the same time everywhere; the link offset must cover
//! packetization, network delay and the error between the nodes' clocks.
//!
//! The receiver follows one stream (SSRC) at a time and switches to another
//! once the current one has been quiet for [`SWITCH_AFTER_MS`]. A [`Filter`]
//! limits it to one source and payload type, or switches it off. Listening
//! on `[::]`, it takes streams over IPv4 and IPv6.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use orpheus_clock::Clock;
use orpheus_core::jitter::{Insert, JitterBuffer};
use orpheus_core::rtp::{self, Encoding, SequenceStats};
use orpheus_core::timebase::{MediaClock, extend};
use orpheus_core::{AudioFormat, OutputType};
use socket2::{Domain, Protocol, Socket, Type};

use crate::asrc::{Asrc, Fixed};
use crate::backend::{AudioBackend, BackendError, StreamHandle, StreamSpec};
use crate::grid::{Grid, Tick};
use crate::port::{CapturePort, PortStats, StatsSnapshot, port_pair};

/// Buffer between the RX thread and the playback stream. The RX thread
/// delivers 1 ms at a time, so this only covers its scheduling jitter.
const BUFFER_MS: u32 = 4;

pub const SWITCH_AFTER_MS: u64 = 1000;

const NO_MARGIN: i64 = i64::MAX;
const NO_SSRC: u64 = u64::MAX;

#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    /// Machine name of the playback stream.
    pub name: String,
    pub description: String,
    pub format: AudioFormat,
    pub encoding: Encoding,
    /// `[::]:port` listens on IPv4 and IPv6 (only IPv4 where IPv6 is
    /// disabled).
    pub listen: SocketAddr,
    /// Playout delay after the media-clock time in the RTP timestamp.
    pub link_offset_ms: u32,
    pub quantum: Option<u32>,
}

/// Which packets the receiver plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filter {
    /// Off: everything is ignored and the receiver plays silence.
    pub enabled: bool,
    /// Only packets from this address; `None` takes any.
    pub source: Option<IpAddr>,
    /// Only this RTP payload type; `None` takes any.
    pub payload_type: Option<u8>,
    /// A multicast group to join, else the stream comes by unicast.
    pub group: Option<Group>,
}

/// A multicast group and, for IPv4, the address of the interface to join
/// it on (`None`: the system's choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group {
    pub address: IpAddr,
    pub interface: Option<IpAddr>,
}

impl Group {
    fn join(&self, socket: &UdpSocket) -> io::Result<()> {
        let s = socket2::SockRef::from(socket);
        match self.address {
            IpAddr::V4(g) => s.join_multicast_v4(&g, &self.interface_v4()),
            IpAddr::V6(g) => s.join_multicast_v6(&g, 0),
        }
    }

    fn leave(&self, socket: &UdpSocket) -> io::Result<()> {
        let s = socket2::SockRef::from(socket);
        match self.address {
            IpAddr::V4(g) => s.leave_multicast_v4(&g, &self.interface_v4()),
            IpAddr::V6(g) => s.leave_multicast_v6(&g, 0),
        }
    }

    fn interface_v4(&self) -> Ipv4Addr {
        match self.interface {
            Some(IpAddr::V4(ip)) => ip,
            _ => Ipv4Addr::UNSPECIFIED,
        }
    }
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            enabled: true,
            source: None,
            payload_type: None,
            group: None,
        }
    }
}

/// The filter, shared with the RX thread, which picks up a new one by its
/// generation.
#[derive(Default)]
struct SharedFilter {
    filter: Mutex<Filter>,
    /// A socket on a new port, for the RX thread to take over.
    socket: Mutex<Option<UdpSocket>>,
    generation: AtomicU64,
}

#[derive(Debug)]
struct Counters {
    packets: AtomicU64,
    malformed: AtomicU64,
    ignored: AtomicU64,
    late: AtomicU64,
    early: AtomicU64,
    lost: AtomicU64,
    realigns: AtomicU64,
    ssrc: AtomicU64,
    margin_us: AtomicI64,
    /// `f64` bits.
    asrc_ppm: AtomicU64,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReceiverStatus {
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "[::]:5004"))]
    pub listen: SocketAddr,
    pub link_offset_ms: u32,
    /// Whether the filter lets packets through.
    pub enabled: bool,
    /// The stream being played, if any.
    pub ssrc: Option<u32>,
    pub packets: u64,
    /// Not RTP or not the expected format.
    pub malformed: u64,
    /// Turned away by the filter: another source or payload type, or the
    /// receiver is off.
    pub ignored: u64,
    /// Arrived after their playout time.
    pub late: u64,
    /// Arrived too far ahead of their playout time to buffer.
    pub early: u64,
    /// Never arrived (by sequence number) in the current stream.
    pub lost: u64,
    /// The RX thread was stalled or the clock jumped.
    pub realigns: u64,
    /// Smallest time between arrival and playout over the last second, in
    /// microseconds. Negative means late; it should stay well above zero.
    pub margin_us: Option<i64>,
    /// Rate correction from the media clock to the sound card's clock.
    pub asrc_ppm: f64,
    /// The buffer between the RX thread and the playback stream.
    pub buffer: StatsSnapshot,
}

/// Binds the receive socket. `[::]` also accepts IPv4 (as IPv4-mapped
/// addresses); where IPv6 is disabled it falls back to `0.0.0.0`.
fn bind(listen: SocketAddr) -> io::Result<UdpSocket> {
    if !(listen.is_ipv6() && listen.ip().is_unspecified()) {
        return UdpSocket::bind(listen);
    }
    let dual = || -> io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_only_v6(false)?;
        socket.bind(&listen.into())?;
        Ok(socket.into())
    };
    dual().or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, listen.port())))
}

pub(crate) struct Receiver {
    /// The configured description of the playback stream.
    description: String,
    /// The address to listen on, with whatever port.
    bind_ip: IpAddr,
    link_offset_ms: u32,
    filter: Arc<SharedFilter>,
    /// The RX thread's socket, for joining and leaving groups.
    socket: Mutex<UdpSocket>,
    counters: Arc<Counters>,
    buffer: Arc<PortStats>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    playback: StreamHandle,
}

impl Receiver {
    pub fn start(
        backend: &mut dyn AudioBackend,
        clock: Arc<dyn Clock>,
        cfg: &ReceiverConfig,
    ) -> Result<Self, BackendError> {
        let format = cfg.format;
        let socket = bind(cfg.listen)?;
        let membership = socket.try_clone()?;
        let (capture, playback, buffer) =
            port_pair(format.channels, format.frames_in_ms(BUFFER_MS));
        let playback = backend.create_playback(
            &StreamSpec {
                name: cfg.name.clone(),
                description: cfg.description.clone(),
                format,
                group: None,
                remote: Some(OutputType::Generic),
                quantum: cfg.quantum,
                process_latency: 0,
            },
            playback,
        )?;

        let counters = Arc::new(Counters {
            packets: AtomicU64::new(0),
            malformed: AtomicU64::new(0),
            ignored: AtomicU64::new(0),
            late: AtomicU64::new(0),
            early: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            realigns: AtomicU64::new(0),
            ssrc: AtomicU64::new(NO_SSRC),
            margin_us: AtomicI64::new(NO_MARGIN),
            asrc_ppm: AtomicU64::new(0f64.to_bits()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let filter = Arc::new(SharedFilter::default());
        let offset = format.frames_in_ms(cfg.link_offset_ms) as u64;
        let rx = Rx {
            port: capture,
            socket,
            clock,
            format,
            encoding: cfg.encoding,
            media: MediaClock::new(format.rate),
            offset,
            // Room for packets up to the link offset ahead, and then some.
            jitter: JitterBuffer::new(
                format.channels as usize,
                2 * offset as usize + format.frames_in_ms(100),
            ),
            seqs: SequenceStats::default(),
            ssrc: None,
            shared_filter: filter.clone(),
            filter: Filter::default(),
            filter_generation: 0,
            last_heard: 0,
            window: MarginWindow::default(),
            asrc: Asrc::new(
                format.rate,
                format.channels as usize,
                format.frames_in_ms(1),
                Fixed::Input,
                0.001,
            ),
            buffer: buffer.clone(),
            counters: counters.clone(),
        };
        let thread = thread::Builder::new().name("orpheus-rx".into()).spawn({
            let stop = stop.clone();
            move || rx.run(&stop)
        })?;
        Ok(Self {
            description: cfg.description.clone(),
            bind_ip: cfg.listen.ip(),
            link_offset_ms: cfg.link_offset_ms,
            filter,
            socket: Mutex::new(membership),
            counters,
            buffer,
            stop,
            thread: Some(thread),
            playback,
        })
    }

    /// Sets the filter and joins its group, leaving the previous one.
    pub fn set_filter(&self, filter: Filter) -> io::Result<()> {
        let mut current = self.filter.filter.lock().unwrap();
        if current.group != filter.group {
            let socket = self.socket.lock().unwrap();
            if let Some(g) = &filter.group {
                g.join(&socket)?;
            }
            if let Some(g) = &current.group {
                let _ = g.leave(&socket);
            }
        }
        *current = filter;
        self.filter.generation.fetch_add(1, Relaxed);
        Ok(())
    }

    /// Renames the playback stream; `None` gives it its configured name.
    pub fn describe(&self, description: Option<&str>) {
        self.playback
            .describe(description.unwrap_or(&self.description));
    }

    /// The output the playback stream plays on, if known.
    pub fn output(&self) -> Option<String> {
        self.playback.output()
    }

    /// Moves to another UDP port, keeping the group joined.
    pub fn set_port(&self, port: u16) -> io::Result<()> {
        let current = self.filter.filter.lock().unwrap();
        let mut socket = self.socket.lock().unwrap();
        if socket.local_addr()?.port() == port {
            return Ok(());
        }
        let new = bind(SocketAddr::new(self.bind_ip, port))?;
        if let Some(g) = &current.group {
            g.join(&new)?;
        }
        *self.filter.socket.lock().unwrap() = Some(new.try_clone()?);
        *socket = new;
        self.filter.generation.fetch_add(1, Relaxed);
        Ok(())
    }

    fn listen(&self) -> SocketAddr {
        let socket = self.socket.lock().unwrap();
        socket
            .local_addr()
            .unwrap_or(SocketAddr::new(self.bind_ip, 0))
    }

    pub fn status(&self) -> ReceiverStatus {
        let c = &self.counters;
        let ssrc = c.ssrc.load(Relaxed);
        let margin = c.margin_us.load(Relaxed);
        ReceiverStatus {
            listen: self.listen(),
            link_offset_ms: self.link_offset_ms,
            enabled: self.filter.filter.lock().unwrap().enabled,
            ssrc: (ssrc != NO_SSRC).then_some(ssrc as u32),
            packets: c.packets.load(Relaxed),
            malformed: c.malformed.load(Relaxed),
            ignored: c.ignored.load(Relaxed),
            late: c.late.load(Relaxed),
            early: c.early.load(Relaxed),
            lost: c.lost.load(Relaxed),
            realigns: c.realigns.load(Relaxed),
            margin_us: (margin != NO_MARGIN).then_some(margin),
            asrc_ppm: f64::from_bits(c.asrc_ppm.load(Relaxed)),
            buffer: self.buffer.snapshot(),
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Minimum arrival margin over one-second windows.
#[derive(Default)]
struct MarginWindow {
    start_ns: u64,
    min_ns: Option<i64>,
}

struct Rx {
    port: CapturePort,
    socket: UdpSocket,
    clock: Arc<dyn Clock>,
    format: AudioFormat,
    encoding: Encoding,
    media: MediaClock,
    /// Link offset in frames.
    offset: u64,
    jitter: JitterBuffer,
    seqs: SequenceStats,
    ssrc: Option<u32>,
    shared_filter: Arc<SharedFilter>,
    filter: Filter,
    filter_generation: u64,
    last_heard: u64,
    window: MarginWindow,
    asrc: Asrc,
    buffer: Arc<PortStats>,
    counters: Arc<Counters>,
}

impl Rx {
    fn run(mut self, stop: &AtomicBool) {
        let ptime = self.format.frames_in_ms(1);
        let ch = self.format.channels as usize;
        let mut block = vec![0.0f32; ptime * ch];
        let mut resampled = vec![0.0f32; self.asrc.output_frames_max() * ch];
        let mut packet = vec![0u8; 65_536];
        let mut samples = vec![0.0f32; packet.len() / self.encoding.bytes_per_sample()];
        let mut grid = Grid::new(self.media, ptime as u64, self.clock.now_ns());

        while !stop.load(Relaxed) {
            // The block for media-clock frame `next` is handed on once that
            // frame is due, and holds the audio stamped `offset` earlier.
            let now = self.clock.now_ns();
            match grid.poll(now, 0) {
                Tick::Wait(d) => {
                    self.receive(&mut packet, &mut samples, grid.next(), d);
                    continue;
                }
                Tick::Realigned => {
                    self.counters.realigns.fetch_add(1, Relaxed);
                    self.asrc.restart();
                    continue;
                }
                Tick::Due => {}
            }
            let ts = grid.next().wrapping_sub(self.offset) as u32;
            self.jitter.read(ts, &mut block);
            let n = self.asrc.process(&block, &mut resampled);
            self.port.push(&resampled[..n * ch]);
            self.asrc.control(
                self.port.level(),
                self.buffer.generation(),
                self.buffer.playing(),
            );
            self.counters
                .asrc_ppm
                .store(self.asrc.correction_ppm().to_bits(), Relaxed);
            grid.advance();
            self.publish_margin(now);
        }
    }

    /// Waits up to `timeout` for one packet and stores it.
    fn receive(&mut self, packet: &mut [u8], samples: &mut [f32], next: u64, timeout: Duration) {
        // Before waiting, so that a new socket takes over at once.
        self.update_filter();
        // A zero timeout would mean "block forever".
        let timeout = timeout.max(Duration::from_micros(1));
        if self.socket.set_read_timeout(Some(timeout)).is_err() {
            return;
        }
        let (len, from) = match self.socket.recv_from(packet) {
            Ok(got) => got,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return;
            }
            Err(_) => {
                // E.g. ICMP errors surfacing; don't spin on them.
                thread::sleep(timeout);
                return;
            }
        };
        let now = self.clock.now_ns();
        let c = &self.counters;
        let Ok((header, payload)) = rtp::parse(&packet[..len]) else {
            c.malformed.fetch_add(1, Relaxed);
            return;
        };
        let f = self.filter;
        if !f.enabled
            || f.source.is_some_and(|s| s != from.ip().to_canonical())
            || f.payload_type.is_some_and(|pt| pt != header.payload_type)
        {
            c.ignored.fetch_add(1, Relaxed);
            return;
        }
        let ch = self.format.channels as usize;
        let n = self.encoding.decode(payload, samples);
        if n == 0 || !n.is_multiple_of(ch) || n * self.encoding.bytes_per_sample() != payload.len()
        {
            c.malformed.fetch_add(1, Relaxed);
            return;
        }

        if self.ssrc != Some(header.ssrc) {
            let quiet = now.saturating_sub(self.last_heard) > SWITCH_AFTER_MS * 1_000_000;
            if self.ssrc.is_some() && !quiet {
                return; // Someone else is playing; first come, first served.
            }
            self.ssrc = Some(header.ssrc);
            self.seqs = SequenceStats::default();
            self.jitter.reset();
            c.ssrc.store(header.ssrc as u64, Relaxed);
        }
        self.last_heard = now;
        self.seqs.record(header.sequence);
        c.packets.fetch_add(1, Relaxed);
        c.lost.store(self.seqs.lost(), Relaxed);

        let playout = extend(header.timestamp, next - self.offset) + self.offset;
        let margin = self.media.ns_at(playout) as i64 - now as i64;
        self.window.min_ns = Some(self.window.min_ns.map_or(margin, |m| m.min(margin)));

        match self.jitter.insert(header.timestamp, &samples[..n]) {
            Insert::Stored => {}
            Insert::Late => {
                c.late.fetch_add(1, Relaxed);
            }
            Insert::Early => {
                c.early.fetch_add(1, Relaxed);
            }
        }
    }

    /// Takes a new filter and drops the stream played so far, so that the
    /// next one starts at once.
    fn update_filter(&mut self) {
        let generation = self.shared_filter.generation.load(Relaxed);
        if generation == self.filter_generation {
            return;
        }
        self.filter_generation = generation;
        self.filter = *self.shared_filter.filter.lock().unwrap();
        if let Some(socket) = self.shared_filter.socket.lock().unwrap().take() {
            self.socket = socket;
        }
        self.ssrc = None;
        self.jitter.reset();
        self.counters.ssrc.store(NO_SSRC, Relaxed);
    }

    fn publish_margin(&mut self, now: u64) {
        if now.saturating_sub(self.window.start_ns) < 1_000_000_000 {
            return;
        }
        let margin = self.window.min_ns.take().map_or(NO_MARGIN, |m| m / 1000);
        self.counters.margin_us.store(margin, Relaxed);
        self.window.start_ns = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_ipv6_also_receives_ipv4() {
        let socket = bind("[::]:0".parse().unwrap()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        let mut targets = vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))];
        // Without IPv6 the socket fell back to IPv4.
        if socket.local_addr().unwrap().is_ipv6() {
            targets.push(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)));
        }
        for to in targets {
            let from = UdpSocket::bind(SocketAddr::new(to.ip(), 0)).unwrap();
            from.send_to(b"rtp", to).unwrap();
            let mut buf = [0; 8];
            assert_eq!(socket.recv(&mut buf).unwrap(), 3, "via {to}");
        }
    }
}
