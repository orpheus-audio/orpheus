//! Sender: a virtual sink whose audio goes out as an RTP stream to one peer.
//!
//! Applications play into the sink; its realtime callback pushes the audio
//! into a port. A TX thread drains the port one packet (1 ms) at a time on
//! the media-clock grid and stamps each packet with the media-clock time of
//! its first frame. The port's target covers one audio-system cycle, since
//! the sink delivers a whole cycle at once while the TX thread takes 1 ms.
//! Until there is ASRC, drift between the sound card and the media clock is
//! absorbed by the port dropping or re-priming audio (see its stats).

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::thread::{self, JoinHandle};

use orpheus_clock::Clock;
use orpheus_core::AudioFormat;
use orpheus_core::rtp::{Encoding, HEADER_LEN, Header};
use orpheus_core::timebase::MediaClock;

use crate::backend::{AudioBackend, BackendError, StreamHandle, StreamSpec};
use crate::grid::{Grid, Tick};
use crate::port::{PlaybackPort, PortStats, StatsSnapshot, port_pair};

/// Dynamic payload type; AES67 leaves the number to the SDP.
pub const PAYLOAD_TYPE: u8 = 97;

/// Cycle length assumed when no quantum is configured (PipeWire's default).
const DEFAULT_QUANTUM: u32 = 1024;

/// Extra buffering for scheduling jitter between the sink and the TX thread.
const JITTER_MARGIN_MS: u32 = 2;

#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// Machine name of the sink, e.g. `orpheus.send.0`.
    pub name: String,
    /// Name of the sink shown to the user.
    pub description: String,
    pub format: AudioFormat,
    pub encoding: Encoding,
    pub peer: SocketAddr,
    /// Frames per audio-system cycle, as configured.
    pub quantum: Option<u32>,
}

#[derive(Debug, Default)]
struct Counters {
    packets: AtomicU64,
    send_errors: AtomicU64,
    realigns: AtomicU64,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderStatus {
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "192.168.1.20:5004"))]
    pub peer: SocketAddr,
    pub packets: u64,
    pub send_errors: u64,
    /// The TX thread was stalled or the clock jumped.
    pub realigns: u64,
    /// The buffer between the sink and the TX thread.
    pub buffer: StatsSnapshot,
}

pub(crate) struct Sender {
    peer: SocketAddr,
    counters: Arc<Counters>,
    buffer: Arc<PortStats>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _sink: StreamHandle,
}

impl Sender {
    pub fn start(
        backend: &mut dyn AudioBackend,
        clock: Arc<dyn Clock>,
        cfg: &SenderConfig,
    ) -> Result<Self, BackendError> {
        let format = cfg.format;
        let quantum = cfg.quantum.unwrap_or(DEFAULT_QUANTUM) as usize;
        let target = quantum + format.frames_in_ms(JITTER_MARGIN_MS);
        let (capture, playback, buffer) = port_pair(format.channels, target);

        let bind: SocketAddr = match cfg.peer {
            SocketAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
            SocketAddr::V6(_) => ([0u16; 8], 0).into(),
        };
        let socket = UdpSocket::bind(bind)?;
        let sink = backend.create_sink(
            &StreamSpec {
                name: cfg.name.clone(),
                description: cfg.description.clone(),
                format,
                target: None,
                group: None,
                quantum: cfg.quantum,
                process_latency: target as u32,
            },
            capture,
        )?;

        let counters = Arc::new(Counters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new().name("orpheus-tx".into()).spawn({
            let tx = Tx {
                port: playback,
                socket,
                peer: cfg.peer,
                clock,
                format,
                encoding: cfg.encoding,
                counters: counters.clone(),
            };
            let stop = stop.clone();
            move || tx.run(&stop)
        })?;
        Ok(Self {
            peer: cfg.peer,
            counters,
            buffer,
            stop,
            thread: Some(thread),
            _sink: sink,
        })
    }

    pub fn status(&self) -> SenderStatus {
        SenderStatus {
            peer: self.peer,
            packets: self.counters.packets.load(Relaxed),
            send_errors: self.counters.send_errors.load(Relaxed),
            realigns: self.counters.realigns.load(Relaxed),
            buffer: self.buffer.snapshot(),
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
    socket: UdpSocket,
    peer: SocketAddr,
    clock: Arc<dyn Clock>,
    format: AudioFormat,
    encoding: Encoding,
    counters: Arc<Counters>,
}

impl Tx {
    fn run(mut self, stop: &AtomicBool) {
        let ptime = self.format.frames_in_ms(1);
        let mut samples = vec![0.0f32; ptime * self.format.channels as usize];
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

        while !stop.load(Relaxed) {
            // A packet goes out once its last frame is due.
            match grid.poll(self.clock.now_ns(), ptime as u64) {
                Tick::Wait(d) => {
                    thread::sleep(d);
                    continue;
                }
                Tick::Realigned => {
                    self.counters.realigns.fetch_add(1, Relaxed);
                    header.marker = true;
                    continue;
                }
                Tick::Due => {}
            }
            self.port.fill(&mut samples);
            header.timestamp = grid.next() as u32;
            header.write((&mut packet[..HEADER_LEN]).try_into().unwrap());
            self.encoding.encode(&samples, &mut packet[HEADER_LEN..]);
            match self.socket.send_to(&packet, self.peer) {
                Ok(_) => self.counters.packets.fetch_add(1, Relaxed),
                Err(_) => self.counters.send_errors.fetch_add(1, Relaxed),
            };
            header.sequence = header.sequence.wrapping_add(1);
            header.marker = false;
            grid.advance();
        }
    }
}

fn random() -> u32 {
    getrandom::u32().unwrap_or(0x6f72_7068)
}
