//! PTP (IEEE 1588-2019) on top of statime.
//!
//! Every node runs a PTP instance with one port on its LAN interface. The
//! best master clock algorithm elects one grandmaster: a dedicated AES67 or
//! Dante grandmaster if there is one, otherwise one of the nodes (they
//! default to a low priority so that real hardware wins). The others follow
//! it.
//!
//! The node does not steer the system clock, which stays with NTP. It keeps
//! a virtual clock instead: an [`OverlayClock`] on top of the system clock
//! (on the TAI timescale) that statime's Kalman filter adjusts. Offsets
//! above the step threshold are stepped, which happens on a cold start;
//! smaller ones are slewed by running the virtual clock slightly fast or
//! slow. After a step the filter starts afresh (see [`Servo`]). The clock
//! counts as settled once, for a few announce intervals in a row, it has
//! been within the settle threshold of the grandmaster, or when this node is
//! the grandmaster.
//!
//! The clock exists whether or not PTP runs. Until PTP starts it is the
//! system clock. If PTP can't start, or its interface goes away, the port
//! tries again every few seconds and whenever the interfaces change, while
//! the clock runs on with its last frequency correction (holdover). It
//! never falls back to the bare system clock, which would make it jump.
//! With no interface configured, PTP follows the default route.
//!
//! PTP runs over UDP on IPv4 (as AES67 and Dante expect) or IPv6; nodes on
//! different transports do not see each other. By default a node uses IPv4
//! if its interface has an IPv4 address, IPv6 otherwise.
//!
//! Software timestamps only for now. Binding the PTP ports 319 and 320 needs
//! `CAP_NET_BIND_SERVICE`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use rand::SeedableRng;
use rand::rngs::StdRng;
use statime::config::{
    AcceptAnyMaster, ClockIdentity, DelayMechanism, InstanceConfig, LeapIndicator, PortConfig,
    PtpMinorVersion, SdoId, TimePropertiesDS, TimeSource,
};
use statime::filters::{Filter, FilterEstimate, FilterUpdate, KalmanConfiguration, KalmanFilter};
use statime::observability::port::PortState;
use statime::port::{
    InBmca, MAX_DATA_LEN, Measurement, NoForwardedTLVs, Port, PortAction, PortActionIterator,
    TimestampContext, is_message_buffer_compatible,
};
use statime::time::{Duration, Interval, Time};
use statime::{OverlayClock, PtpInstance, PtpInstanceState};
use timestamped_socket::interface::{ChangeDetector, InterfaceName, interfaces};
use timestamped_socket::networkaddress::MulticastJoinable;
use timestamped_socket::socket::{
    InterfaceTimestampMode, Open, Socket, Timestamp, open_interface_udp4, open_interface_udp6,
};
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep, sleep_until};

use crate::{Clock, Timescale};

const EVENT_PORT: u16 = 319;
const GENERAL_PORT: u16 = 320;
const PRIMARY_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
const PDELAY_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 107);
/// `ff0e::181`: global scope, as ptp4l uses by default.
const PRIMARY_V6: Ipv6Addr = Ipv6Addr::new(0xff0e, 0, 0, 0, 0, 0, 0, 0x181);
/// `ff02::6b`: link-local, as IEEE 1588 requires for peer delay.
const PDELAY_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x6b);

/// Announce intervals in a row within the settle threshold before the clock
/// counts as settled.
const SETTLE_ROUNDS: u32 = 3;

/// Measurements this soon after a step are dropped: they may have been
/// taken before it.
const STEP_HOLDOFF_NS: i64 = 500_000_000;

/// Delay samples the servo's own path delay estimate is the median of.
const DELAY_SAMPLES: usize = 15;
/// A sync and a delay measurement further apart than this are not paired:
/// the clock's frequency error between them would bias the delay.
const DELAY_PAIR_NS: f64 = 250_000_000.0;

/// The gate passes a measurement that arrived at most this much later than
/// the fastest of its direction in the window. A wired path stays well
/// within it, so the gate only acts on Wi-Fi.
const GATE_MARGIN_S: f64 = 500e-6;
/// The windows the gate takes the fastest measurement from: 8 s of syncs,
/// 16 s of delay requests. A real change of offset or path passes once the
/// faster measurements from before it have left the window.
const GATE_SYNC_SAMPLES: usize = 64;
const GATE_DELAY_SAMPLES: usize = 16;
/// Measurements the filter takes after a start or a step before the gate
/// trusts its prediction.
const GATE_WARMUP: u32 = 8;

/// How long to wait before trying again to start PTP, unless the
/// interfaces change first.
const RETRY: std::time::Duration = std::time::Duration::from_secs(5);
/// How long to let the interfaces settle after a change before PTP tries
/// again.
const SETTLE_CHANGES: std::time::Duration = std::time::Duration::from_secs(1);

/// TAI − UTC announced when this node is the grandmaster.
const UTC_OFFSET_S: i16 = 37;

/// The network protocol PTP messages travel over.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PtpTransport {
    /// IPv4 if the interface has an IPv4 address, IPv6 otherwise.
    #[default]
    Auto,
    Ipv4,
    Ipv6,
}

#[derive(Debug, Clone)]
pub struct PtpConfig {
    /// Network interface; `None` picks the one with the default route.
    pub interface: Option<String>,
    pub transport: PtpTransport,
    pub domain: u8,
    /// Lower wins the grandmaster election. Dedicated grandmasters usually
    /// use 128 or less.
    pub priority1: u8,
    pub priority2: u8,
    /// Never become grandmaster.
    pub slave_only: bool,
    /// Offsets above this are stepped, smaller ones slewed.
    pub step_threshold_us: u32,
    /// Offset under which the clock counts as settled.
    pub settle_threshold_us: u32,
}

impl Default for PtpConfig {
    fn default() -> Self {
        Self {
            interface: None,
            transport: PtpTransport::Auto,
            domain: 0,
            priority1: 250,
            priority2: 128,
            slave_only: false,
            step_threshold_us: 1000,
            settle_threshold_us: 100,
        }
    }
}

/// Port state as defined by IEEE 1588.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtpState {
    Initializing,
    Faulty,
    Disabled,
    Listening,
    PreMaster,
    /// This node is the grandmaster.
    Master,
    Passive,
    /// Following a master, not yet synchronized.
    Uncalibrated,
    /// Following a master.
    Slave,
}

impl From<PortState> for PtpState {
    fn from(s: PortState) -> Self {
        match s {
            PortState::Initializing => Self::Initializing,
            PortState::Faulty => Self::Faulty,
            PortState::Disabled => Self::Disabled,
            PortState::Listening => Self::Listening,
            PortState::PreMaster => Self::PreMaster,
            PortState::Master => Self::Master,
            PortState::Passive => Self::Passive,
            PortState::Uncalibrated => Self::Uncalibrated,
            PortState::Slave => Self::Slave,
        }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq)]
pub struct PtpStatus {
    /// Whether the port runs. If not, `error` says why, and the clock runs
    /// on by itself.
    pub running: bool,
    /// Why PTP is not running.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub error: Option<String>,
    /// PTP stopped after the clock had followed a grandmaster: it keeps
    /// that grandmaster's time and rate, and drifts away slowly.
    pub holdover: bool,
    /// Empty if none could be found.
    pub interface: String,
    /// `ipv4` or `ipv6`, never `auto`.
    pub transport: PtpTransport,
    pub domain: u8,
    /// This node's clock identity.
    #[cfg_attr(feature = "openapi", schema(example = "4cb04a.fffe.50a582"))]
    pub identity: String,
    pub state: PtpState,
    /// Clock identity of the grandmaster.
    pub grandmaster: String,
    /// Estimated offset from the master, while following one.
    pub offset_ns: Option<i64>,
    /// Estimated one-way network delay to the master.
    pub mean_delay_ns: Option<i64>,
    /// How much faster than the system clock the virtual clock runs.
    pub frequency_ppm: f64,
    /// Times the virtual clock was stepped.
    pub steps: u64,
    /// Within the settle threshold of the grandmaster, or the grandmaster.
    pub settled: bool,
}

/// The system clock on the TAI timescale, read-only underneath the overlay.
#[derive(Debug, Clone, Copy)]
struct Base(Timescale);

impl statime::Clock for Base {
    type Error = io::Error;

    fn now(&self) -> Time {
        Time::from_nanos(self.0.now())
    }

    fn step_clock(&mut self, _: Duration) -> Result<Time, io::Error> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn set_frequency(&mut self, _: f64) -> Result<Time, io::Error> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn set_properties(&mut self, _: &TimePropertiesDS) -> Result<(), io::Error> {
        Ok(())
    }
}

struct Shared {
    clock: Mutex<OverlayClock<Base>>,
    status: Mutex<PtpStatus>,
    steps: AtomicU64,
    frequency_ppm: AtomicU64,
    /// The clock has followed a grandmaster since the start.
    followed: AtomicBool,
}

impl Shared {
    /// The system clock on `timescale`, with PTP not running yet.
    fn new(timescale: Timescale, domain: u8) -> Self {
        Self {
            clock: Mutex::new(OverlayClock::new(Base(timescale))),
            status: Mutex::new(PtpStatus {
                running: false,
                error: None,
                holdover: false,
                interface: String::new(),
                transport: PtpTransport::Ipv4,
                domain,
                identity: String::new(),
                state: PtpState::Initializing,
                grandmaster: String::new(),
                offset_ns: None,
                mean_delay_ns: None,
                frequency_ppm: 0.0,
                steps: 0,
                settled: false,
            }),
            steps: AtomicU64::new(0),
            frequency_ppm: AtomicU64::new(0f64.to_bits()),
            followed: AtomicBool::new(false),
        }
    }

    fn now(&self) -> Time {
        statime::Clock::now(&*self.clock.lock().unwrap())
    }
}

/// The virtual clock as statime's port sees it.
#[derive(Clone)]
struct PortClock(Arc<Shared>);

impl PortClock {
    /// Converts a socket timestamp (`CLOCK_REALTIME`) to virtual time.
    fn time_of(&self, ts: Timestamp) -> Time {
        let realtime = ts.seconds as u64 * 1_000_000_000 + ts.nanos as u64;
        let clock = self.0.clock.lock().unwrap();
        let base = Time::from_nanos(clock.underlying().0.from_realtime(realtime));
        clock.time_from_underlying(base)
    }
}

impl statime::Clock for PortClock {
    type Error = io::Error;

    fn now(&self) -> Time {
        self.0.now()
    }

    fn step_clock(&mut self, offset: Duration) -> Result<Time, io::Error> {
        let mut clock = self.0.clock.lock().unwrap();
        self.0.steps.fetch_add(1, Relaxed);
        tracing::info!(offset_us = offset.nanos_lossy() / 1e3, "PTP clock stepped");
        clock.step_clock(offset)
    }

    fn set_frequency(&mut self, ppm: f64) -> Result<Time, io::Error> {
        self.0.frequency_ppm.store(ppm.to_bits(), Relaxed);
        self.0.clock.lock().unwrap().set_frequency(ppm)
    }

    fn set_properties(&mut self, _: &TimePropertiesDS) -> Result<(), io::Error> {
        Ok(())
    }
}

/// A media clock synchronized over PTP. Stops when dropped.
pub struct PtpClock {
    shared: Arc<Shared>,
    port: Mutex<PortThread>,
}

/// The thread running the PTP port.
struct PortThread {
    cfg: PtpConfig,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl PortThread {
    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl PtpClock {
    /// Starts following (or being) the grandmaster in a background thread.
    /// The virtual clock runs on top of `timescale`. If PTP can't start, the
    /// thread keeps trying and the status says why; this fails only if the
    /// thread can't be started.
    pub fn start(cfg: PtpConfig, timescale: Timescale) -> io::Result<Self> {
        let shared = Arc::new(Shared::new(timescale, cfg.domain));
        let port = spawn(cfg, &shared, true)?;
        Ok(Self {
            shared,
            port: Mutex::new(port),
        })
    }

    /// Restarts PTP with other settings. The clock keeps running and its
    /// frequency correction, so it does not jump. If PTP can't start with
    /// the new settings, it goes on with the old ones (or keeps trying to)
    /// and returns the error.
    pub fn reconfigure(&self, cfg: PtpConfig) -> io::Result<()> {
        let mut port = self.port.lock().unwrap();
        port.stop();
        match spawn(cfg, &self.shared, false) {
            Ok(new) => {
                *port = new;
                Ok(())
            }
            Err(e) => {
                let old = port.cfg.clone();
                *port = spawn(old, &self.shared, true)?;
                Err(e)
            }
        }
    }

    pub fn status(&self) -> PtpStatus {
        self.shared.status.lock().unwrap().clone()
    }
}

/// Runs PTP for `cfg` in a background thread, on the virtual clock in
/// `shared`, and waits for the first attempt to start it. If that fails,
/// the thread keeps trying with `keep_trying`; otherwise it ends and the
/// error is returned.
fn spawn(cfg: PtpConfig, shared: &Arc<Shared>, keep_trying: bool) -> io::Result<PortThread> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let thread = thread::Builder::new().name("orpheus-ptp".into()).spawn({
        let shared = shared.clone();
        let cfg = cfg.clone();
        move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match rt {
                Ok(rt) => rt.block_on(supervise(Supervise {
                    cfg,
                    shared,
                    ready: Some(ready_tx),
                    keep_trying,
                    stop: stop_rx,
                })),
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        }
    })?;
    let ready = ready_rx
        .recv()
        .unwrap_or_else(|_| Err(io::Error::other("PTP thread exited")));
    if let Err(e) = ready
        && !keep_trying
    {
        let _ = thread.join();
        return Err(e);
    }
    Ok(PortThread {
        cfg,
        stop: Some(stop_tx),
        thread: Some(thread),
    })
}

impl Clock for PtpClock {
    fn now_ns(&self) -> u64 {
        self.shared.now().nanos().to_num()
    }
}

impl Drop for PtpClock {
    fn drop(&mut self) {
        self.port.lock().unwrap().stop();
    }
}

type BmcaPort<'a> =
    Port<'a, InBmca, AcceptAnyMaster, StdRng, PortClock, Servo, RefCell<PtpInstanceState>>;

/// statime's Kalman filter, started afresh after every step, fed with
/// measurements from which the path delay is taken out.
///
/// statime keeps its estimate of the measurement noise across a step, and on
/// a cold start that estimate takes in a pair of measurements from both
/// sides of the step, off by the whole step. The filter then distrusts every
/// measurement until the pair ages out about 30 s later, and overshoots.
///
/// statime also resets its filter when a raw measurement (offset + delay
/// from a sync, offset − delay from a delay request) is further than the
/// step threshold from its offset alone, without the delay. With a path
/// delay above half the threshold it then resets and steps over and over.
/// So the servo estimates the delay itself, from sync and delay
/// measurements close together (the offset cancels), and hands the filter
/// only what is left of it. The estimate survives steps, which don't change
/// the path.
///
/// A [`Gate`] in front of both keeps out measurements that waited on the
/// way. Waiting only ever adds time, so the fastest measurements are the
/// truthful ones. On Wi-Fi the access point holds multicast for up to a
/// beacon interval (about 100 ms) while any client dozes; a single held
/// sync or delay request would otherwise make the filter reset and step,
/// and the next one step back.
struct Servo {
    config: KalmanConfiguration,
    filter: KalmanFilter,
    /// Time of the last step; measurements shortly after it are dropped.
    stepped_at: Option<Time>,
    delay: DelayEstimate,
    sync_gate: Gate,
    delay_gate: Gate,
    /// Measurements the filter took since it started afresh.
    taken: u32,
}

/// Passes the measurements of one direction that arrived about as early as
/// the earliest in a window. How late a measurement is, its excess, is
/// taken against the filter's prediction, which follows the clock's drift,
/// so the window may be seconds long.
struct Gate {
    excess: VecDeque<f64>,
    len: usize,
}

impl Gate {
    fn new(len: usize) -> Self {
        Self {
            excess: VecDeque::with_capacity(len + 1),
            len,
        }
    }

    /// Whether a measurement `excess` seconds later than predicted passes.
    fn pass(&mut self, excess: f64) -> bool {
        self.excess.push_back(excess);
        if self.excess.len() > self.len {
            self.excess.pop_front();
        }
        let fastest = self.excess.iter().copied().fold(f64::INFINITY, f64::min);
        excess <= fastest + GATE_MARGIN_S
    }

    fn clear(&mut self) {
        self.excess.clear();
    }
}

/// The path delay, as the median of recent samples.
#[derive(Default)]
struct DelayEstimate {
    /// The last sync measurement: when, and offset + delay in seconds.
    last_sync: Option<(Time, f64)>,
    samples: VecDeque<f64>,
    /// Median of `samples`, in seconds; `None` before the first pair.
    value: Option<f64>,
}

impl DelayEstimate {
    fn sync(&mut self, at: Time, raw: f64) {
        self.last_sync = Some((at, raw));
    }

    fn delay(&mut self, at: Time, raw: f64) {
        let Some((sync_at, sync)) = self.last_sync else {
            return;
        };
        if (at - sync_at).nanos_lossy().abs() > DELAY_PAIR_NS {
            return;
        }
        self.samples.push_back((sync - raw) / 2.0);
        if self.samples.len() > DELAY_SAMPLES {
            self.samples.pop_front();
        }
        let mut sorted: Vec<f64> = self.samples.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        self.value = Some(sorted[sorted.len() / 2]);
    }

    /// Forgets the last sync, which a step made stale.
    fn stepped(&mut self) {
        self.last_sync = None;
    }
}

impl Filter for Servo {
    type Config = KalmanConfiguration;

    fn new(config: KalmanConfiguration) -> Self {
        Self {
            filter: KalmanFilter::new(config),
            config,
            stepped_at: None,
            delay: DelayEstimate::default(),
            sync_gate: Gate::new(GATE_SYNC_SAMPLES),
            delay_gate: Gate::new(GATE_DELAY_SAMPLES),
            taken: 0,
        }
    }

    fn measurement<C: statime::Clock>(
        &mut self,
        mut m: Measurement,
        clock: &mut C,
    ) -> FilterUpdate {
        if let Some(at) = self.stepped_at {
            if (clock.now() - at).nanos_lossy() < STEP_HOLDOFF_NS as f64 {
                return FilterUpdate::default();
            }
            self.stepped_at = None;
            self.filter = KalmanFilter::new(self.config);
            self.taken = 0;
            self.sync_gate.clear();
            self.delay_gate.clear();
        }
        if !self.gate(&mut m) {
            return FilterUpdate::default();
        }
        if let Some(sync) = m.raw_sync_offset {
            self.delay.sync(m.event_time, sync.seconds());
        }
        if let Some(delay) = m.raw_delay_offset {
            self.delay.delay(m.event_time, delay.seconds());
        }
        // Until the first pair, the filter would take the delay for an
        // offset and step by it.
        let Some(delay) = self.delay.value else {
            return FilterUpdate::default();
        };
        let delay = Duration::from_seconds(delay);
        m.raw_sync_offset = m.raw_sync_offset.map(|o| o - delay);
        m.raw_delay_offset = m.raw_delay_offset.map(|o| o + delay);
        let mut watch = StepWatch {
            clock,
            stepped_at: None,
        };
        let update = self.filter.measurement(m, &mut watch);
        self.taken = self.taken.saturating_add(1);
        self.stepped_at = watch.stepped_at;
        if self.stepped_at.is_some() {
            self.delay.stepped();
        }
        self.with_delay(update)
    }

    fn update<C: statime::Clock>(&mut self, clock: &mut C) -> FilterUpdate {
        let update = self.filter.update(clock);
        self.with_delay(update)
    }

    fn demobilize<C: statime::Clock>(self, clock: &mut C) {
        self.filter.demobilize(clock);
    }

    fn current_estimates(&self) -> FilterEstimate {
        let mut estimate = self.filter.current_estimates();
        // Zero still means "no measurements yet".
        if estimate.mean_delay != Duration::ZERO {
            estimate.mean_delay += self.bulk_delay();
        }
        estimate
    }
}

impl Servo {
    /// Takes out of `m` what waited on the way; whether anything is left.
    /// Until the filter has taken a few measurements it predicts nothing,
    /// and everything passes.
    fn gate(&mut self, m: &mut Measurement) -> bool {
        let Some(bulk) = self.delay.value.filter(|_| self.taken >= GATE_WARMUP) else {
            return true;
        };
        let e = self.filter.current_estimates();
        let (offset, delay) = (e.offset_from_master.seconds(), e.mean_delay.seconds());
        // A sync sees offset + delay, a delay request offset - delay;
        // waiting raises the one and lowers the other.
        if let Some(s) = m.raw_sync_offset
            && !self.sync_gate.pass(s.seconds() - bulk - (offset + delay))
        {
            m.raw_sync_offset = None;
        }
        if let Some(d) = m.raw_delay_offset
            && !self
                .delay_gate
                .pass((offset - delay) - (d.seconds() + bulk))
        {
            m.raw_delay_offset = None;
        }
        m.raw_sync_offset.is_some() || m.raw_delay_offset.is_some()
    }

    /// The part of the delay taken out before the filter.
    fn bulk_delay(&self) -> Duration {
        Duration::from_seconds(self.delay.value.unwrap_or(0.0))
    }

    fn with_delay(&self, mut update: FilterUpdate) -> FilterUpdate {
        update.mean_delay = update.mean_delay.map(|d| d + self.bulk_delay());
        update
    }
}

/// Passes clock calls through and notes when the clock was stepped.
struct StepWatch<'a, C> {
    clock: &'a mut C,
    stepped_at: Option<Time>,
}

impl<C: statime::Clock> statime::Clock for StepWatch<'_, C> {
    type Error = C::Error;

    fn now(&self) -> Time {
        self.clock.now()
    }

    fn step_clock(&mut self, offset: Duration) -> Result<Time, C::Error> {
        let time = self.clock.step_clock(offset)?;
        self.stepped_at = Some(time);
        Ok(time)
    }

    fn set_frequency(&mut self, ppm: f64) -> Result<Time, C::Error> {
        self.clock.set_frequency(ppm)
    }

    fn set_properties(&mut self, properties: &TimePropertiesDS) -> Result<(), C::Error> {
        self.clock.set_properties(properties)
    }
}

/// Deadlines of the port's timers; `None` when not armed.
#[derive(Default)]
struct Timers {
    announce: Option<Instant>,
    sync: Option<Instant>,
    delay_request: Option<Instant>,
    announce_receipt: Option<Instant>,
    filter_update: Option<Instant>,
}

async fn at(deadline: Option<Instant>) {
    match deadline {
        Some(d) => sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// A UDP transport for PTP: IPv4 or IPv6.
trait Transport: MulticastJoinable {
    /// The primary or the peer-delay multicast group.
    fn group(link_local: bool, port: u16) -> Self;

    fn open(
        interface: InterfaceName,
        port: u16,
        timestamping: InterfaceTimestampMode,
    ) -> io::Result<Socket<Self, Open>>;
}

impl Transport for SocketAddrV4 {
    fn group(link_local: bool, port: u16) -> Self {
        Self::new(if link_local { PDELAY_V4 } else { PRIMARY_V4 }, port)
    }

    fn open(
        interface: InterfaceName,
        port: u16,
        timestamping: InterfaceTimestampMode,
    ) -> io::Result<Socket<Self, Open>> {
        open_interface_udp4(interface, port, timestamping, None)
    }
}

impl Transport for SocketAddrV6 {
    fn group(link_local: bool, port: u16) -> Self {
        Self::new(if link_local { PDELAY_V6 } else { PRIMARY_V6 }, port, 0, 0)
    }

    fn open(
        interface: InterfaceName,
        port: u16,
        timestamping: InterfaceTimestampMode,
    ) -> io::Result<Socket<Self, Open>> {
        open_interface_udp6(interface, port, timestamping, None)
    }
}

struct Sockets<A: Transport> {
    interface: InterfaceName,
    event: Socket<A, Open>,
    general: Socket<A, Open>,
    /// Send timestamps missing in a row.
    missing: u32,
    /// The interface gives no send timestamps: event messages are stamped
    /// just before sending instead (see [`handle_actions`]).
    send_time: bool,
}

/// Opens a socket and joins both PTP groups on it.
fn open_socket<A: Transport>(
    interface: InterfaceName,
    port: u16,
    timestamping: InterfaceTimestampMode,
) -> io::Result<Socket<A, Open>> {
    let socket = A::open(interface, port, timestamping)?;
    for link_local in [false, true] {
        socket.join_multicast(A::group(link_local, 0), interface)?;
    }
    Ok(socket)
}

fn open_sockets<A: Transport>(interface: InterfaceName) -> io::Result<Sockets<A>> {
    let context = |what: &str, e: io::Error| {
        let hint = if e.kind() == io::ErrorKind::PermissionDenied {
            " (needs CAP_NET_BIND_SERVICE: sudo setcap cap_net_bind_service=ep orpheusd)"
        } else {
            ""
        };
        io::Error::new(
            e.kind(),
            format!("PTP {what} socket on {interface}: {e}{hint}"),
        )
    };
    // `ORPHEUS_DEBUG_NO_SEND_TIMESTAMPS` acts as if the interface gave none.
    let send_time = std::env::var_os("ORPHEUS_DEBUG_NO_SEND_TIMESTAMPS").is_some();
    let timestamping = if send_time {
        InterfaceTimestampMode::SoftwareRecv
    } else {
        InterfaceTimestampMode::SoftwareAll
    };
    let event =
        open_socket(interface, EVENT_PORT, timestamping).map_err(|e| context("event", e))?;
    let general = open_socket(interface, GENERAL_PORT, InterfaceTimestampMode::None)
        .map_err(|e| context("general", e))?;
    Ok(Sockets {
        interface,
        event,
        general,
        missing: 0,
        send_time,
    })
}

/// What the PTP thread starts with.
struct Supervise {
    cfg: PtpConfig,
    shared: Arc<Shared>,
    /// Takes the outcome of the first attempt.
    ready: Option<mpsc::Sender<io::Result<()>>>,
    keep_trying: bool,
    stop: oneshot::Receiver<()>,
}

/// Why the port stopped.
enum Ended {
    /// Asked to.
    Stopped,
    /// It could not start.
    Failed(io::Error),
    /// Its interface went away or changed.
    Lost(String),
}

/// Runs the port, starts it again when its interface changes, and keeps
/// trying while it can't start.
async fn supervise(mut sv: Supervise) {
    let mut changes = ChangeDetector::new()
        .inspect_err(|e| {
            tracing::warn!(error = %e, "not watching the interfaces; PTP retries every 5 s only")
        })
        .ok();
    let mut logged: Option<String> = None;
    loop {
        let ended = match resolve(&sv.cfg) {
            Ok(link) => match link.transport {
                PtpTransport::Ipv6 => run::<SocketAddrV6>(&mut sv, &link, &mut changes).await,
                _ => run::<SocketAddrV4>(&mut sv, &link, &mut changes).await,
            },
            Err(e) => Ended::Failed(e),
        };
        let error = match ended {
            Ended::Stopped => return,
            Ended::Lost(reason) => {
                tracing::warn!(%reason, "PTP stopped; the clock runs on by itself");
                down(&sv.shared, &reason);
                logged = Some(reason);
                // Straight on to the next attempt, which may find a new
                // interface.
                continue;
            }
            Ended::Failed(e) => e,
        };
        let message = error.to_string();
        if let Some(ready) = sv.ready.take() {
            let _ = ready.send(Err(error));
            if !sv.keep_trying {
                return;
            }
        }
        if logged.as_ref() != Some(&message) {
            tracing::warn!(error = %message, "PTP can't start; trying again");
        }
        down(&sv.shared, &message);
        logged = Some(message);
        tokio::select! {
            _ = &mut sv.stop => return,
            () = sleep(RETRY) => {}
            () = async {
                changed(&mut changes).await;
                sleep(SETTLE_CHANGES).await;
            } => {}
        }
    }
}

/// Waits for a change to the interfaces, or forever without a detector.
async fn changed(changes: &mut Option<ChangeDetector>) {
    match changes {
        Some(c) => c.wait_for_change().await,
        None => std::future::pending().await,
    }
}

/// Marks PTP as not running, for `error`.
fn down(shared: &Shared, error: &str) {
    let mut status = shared.status.lock().unwrap();
    status.running = false;
    status.error = Some(error.to_owned());
    status.holdover = shared.followed.load(Relaxed);
    status.state = PtpState::Faulty;
    status.offset_ns = None;
    status.mean_delay_ns = None;
    status.settled = false;
}

/// The interface PTP runs on, and what it needs to know about it.
#[derive(Debug, PartialEq)]
struct Link {
    interface: InterfaceName,
    transport: PtpTransport,
    identity: ClockIdentity,
    /// Its IPv4 addresses, sorted; a new one restarts the port.
    ipv4: Vec<Ipv4Addr>,
}

/// The interface for `cfg`, if it can carry PTP.
fn resolve(cfg: &PtpConfig) -> io::Result<Link> {
    let interface = match &cfg.interface {
        Some(name) => name.parse::<InterfaceName>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad interface {name:?}"),
            )
        })?,
        None => default_interface()?,
    };
    let data = interfaces()?.remove(&interface).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("no interface {interface}"))
    })?;
    let mac = data.mac().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface {interface} has no MAC address"),
        )
    })?;
    if is_down(interface) {
        return Err(io::Error::new(
            io::ErrorKind::NetworkDown,
            format!("interface {interface} is down"),
        ));
    }
    let ips: Vec<IpAddr> = data.ips().collect();
    let transport = match cfg.transport {
        PtpTransport::Auto => auto_transport(interface, &ips)?,
        t => t,
    };
    let mut ipv4: Vec<Ipv4Addr> = ips
        .iter()
        .filter_map(|ip| match ip {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        })
        .collect();
    ipv4.sort();
    Ok(Link {
        interface,
        transport,
        identity: eui64(mac),
        ipv4,
    })
}

impl Link {
    /// Why the port has to start again on another link, if it does.
    fn lost(&self, cfg: &PtpConfig) -> Option<String> {
        match resolve(cfg) {
            Err(e) => Some(e.to_string()),
            Ok(now) if now.interface != self.interface => {
                Some(format!("the default route moved to {}", now.interface))
            }
            Ok(now) if now.transport != self.transport => Some(format!(
                "interface {} changed its addresses",
                self.interface
            )),
            Ok(now) if self.transport == PtpTransport::Ipv4 && now.ipv4 != self.ipv4 => {
                Some(format!("interface {} has a new address", self.interface))
            }
            Ok(_) => None,
        }
    }
}

/// Whether the kernel reports `interface` as down. Unknown counts as up,
/// as for tunnels and some virtual interfaces.
fn is_down(interface: InterfaceName) -> bool {
    let path = format!("/sys/class/net/{interface}/operstate");
    fs::read_to_string(path)
        .is_ok_and(|s| matches!(s.trim(), "down" | "lowerlayerdown" | "notpresent"))
}

/// Opens the sockets for `link` and runs the port until it is stopped or
/// the link goes away.
async fn run<A: Transport>(
    sv: &mut Supervise,
    link: &Link,
    changes: &mut Option<ChangeDetector>,
) -> Ended {
    let (cfg, shared) = (&sv.cfg, &sv.shared);
    let (interface, identity) = (link.interface, link.identity);
    let mut sockets = match open_sockets::<A>(interface) {
        Ok(s) => s,
        Err(e) => return Ended::Failed(e),
    };
    {
        let mut status = shared.status.lock().unwrap();
        status.running = true;
        status.error = None;
        status.holdover = false;
        status.interface = interface.to_string();
        status.transport = link.transport;
        status.domain = cfg.domain;
        status.identity = format_identity(identity);
        status.state = PtpState::Initializing;
        status.grandmaster = format_identity(identity);
        status.offset_ns = None;
        status.mean_delay_ns = None;
        status.settled = false;
    }
    if let Some(ready) = sv.ready.take() {
        let _ = ready.send(Ok(()));
    }
    let instance = PtpInstance::<Servo>::new(
        InstanceConfig {
            clock_identity: identity,
            priority_1: cfg.priority1,
            priority_2: cfg.priority2,
            domain_number: cfg.domain,
            slave_only: cfg.slave_only,
            sdo_id: SdoId::try_from(0).expect("0 is a valid SdoId"),
            path_trace: false,
        },
        TimePropertiesDS::new_ptp_time(
            Some(UTC_OFFSET_S),
            LeapIndicator::NoLeap,
            false,
            false,
            TimeSource::InternalOscillator,
        ),
    );
    let clock = PortClock(shared.clone());
    let filter = KalmanConfiguration {
        step_threshold: Duration::from_nanos(cfg.step_threshold_us as i64 * 1000),
        // Correct offsets within about a second instead of two.
        steer_time: Duration::from_seconds(1.0),
        // Estimate the measurement noise from two measurement pairs instead
        // of four. Until then statime assumes a noise of `steer_time` and
        // learns almost nothing, while a clock that is 200 ppm off drifts by
        // up to a millisecond. Both together halve the time to settle on a
        // cold start; see docs/clock.md.
        difference_estimation_boundary: 2,
        ..Default::default()
    };
    // AES67 media profile defaults.
    let port_config = PortConfig {
        acceptable_master_list: AcceptAnyMaster,
        delay_mechanism: DelayMechanism::E2E {
            interval: Interval::from_log_2(0),
        },
        announce_interval: Interval::from_log_2(1),
        announce_receipt_timeout: 3,
        sync_interval: Interval::from_log_2(-3),
        master_only: false,
        delay_asymmetry: Duration::ZERO,
        minor_ptp_version: PtpMinorVersion::One,
    };
    let mut port = instance.add_port(port_config, filter, clock.clone(), StdRng::from_entropy());
    tracing::info!(
        %interface,
        transport = ?link.transport,
        identity = %format_identity(identity),
        "PTP running"
    );

    let mut timers = Timers::default();
    let mut settle = Settle::default();
    let mut event_buf = [0u8; MAX_DATA_LEN];
    let mut general_buf = [0u8; 2048];
    let mut bmca_at = Instant::now();
    loop {
        instance.bmca(&mut [&mut port]);
        publish(&instance, &port, shared, cfg, &mut settle);
        bmca_at += instance.bmca_interval();

        let (mut running, actions) = port.end_bmca();
        let mut pending = handle_actions(actions, &mut sockets, &mut timers, &clock).await;
        while let Some((context, time)) = pending {
            let actions = running.handle_send_timestamp(context, time);
            pending = handle_actions(actions, &mut sockets, &mut timers, &clock).await;
        }

        loop {
            let mut actions = tokio::select! {
                _ = &mut sv.stop => return Ended::Stopped,
                () = sleep_until(bmca_at) => break,
                () = changed(changes) => match link.lost(cfg) {
                    Some(reason) => return Ended::Lost(reason),
                    None => PortActionIterator::empty(),
                },
                result = sockets.event.recv(&mut event_buf) => match result {
                    Ok(packet) => {
                        let data = &event_buf[..packet.bytes_read];
                        match packet.timestamp_data.selected_timestamp() {
                            Some(ts) if is_message_buffer_compatible(data) => {
                                running.handle_event_receive(data, clock.time_of(ts))
                            }
                            _ => PortActionIterator::empty(),
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "PTP receive failed");
                        PortActionIterator::empty()
                    }
                },
                result = sockets.general.recv(&mut general_buf) => match result {
                    Ok(packet) => running.handle_general_receive(&general_buf[..packet.bytes_read]),
                    Err(e) => {
                        tracing::warn!(error = %e, "PTP receive failed");
                        PortActionIterator::empty()
                    }
                },
                () = at(timers.announce) => {
                    timers.announce = None;
                    running.handle_announce_timer(&mut NoForwardedTLVs)
                },
                () = at(timers.sync) => {
                    timers.sync = None;
                    running.handle_sync_timer()
                },
                () = at(timers.delay_request) => {
                    timers.delay_request = None;
                    running.handle_delay_request_timer()
                },
                () = at(timers.announce_receipt) => {
                    timers.announce_receipt = None;
                    running.handle_announce_receipt_timer()
                },
                () = at(timers.filter_update) => {
                    timers.filter_update = None;
                    running.handle_filter_update_timer()
                },
            };
            loop {
                let pending = handle_actions(actions, &mut sockets, &mut timers, &clock).await;
                actions = match pending {
                    Some((context, time)) => running.handle_send_timestamp(context, time),
                    None => break,
                };
            }
        }
        port = running.start_bmca();
    }
}

/// Send timestamps missing in a row before the interface counts as giving
/// none. Each one costs timestamped-socket a 200 ms wait.
const MAX_MISSING_SEND_TIMESTAMPS: u32 = 3;

/// Carries out the port's actions; returns the send timestamp the port
/// asked for, if any.
///
/// Some interfaces (many Wi-Fi drivers, bridges over them) give no software
/// send timestamps. Then the time just before sending stands in. It misses
/// the time the packet spends in the kernel and driver, a few tens of µs,
/// which ends up in the delay estimate. After a few misses in a row the
/// event socket is reopened without send timestamps so that sending no
/// longer waits for them.
async fn handle_actions<A: Transport>(
    actions: PortActionIterator<'_>,
    sockets: &mut Sockets<A>,
    timers: &mut Timers,
    clock: &PortClock,
) -> Option<(TimestampContext, Time)> {
    let mut pending = None;
    for action in actions {
        match action {
            PortAction::SendEvent {
                context,
                data,
                link_local,
            } => {
                let before = statime::Clock::now(clock);
                match sockets
                    .event
                    .send_to(data, A::group(link_local, EVENT_PORT))
                    .await
                {
                    Ok(ts) => match ts.selected_timestamp() {
                        Some(ts) => {
                            sockets.missing = 0;
                            pending = Some((context, clock.time_of(ts)));
                        }
                        None => {
                            pending = Some((context, before));
                            if !sockets.send_time {
                                missing_send_timestamp(sockets);
                            }
                        }
                    },
                    Err(e) => tracing::warn!(error = %e, "PTP send failed"),
                }
            }
            PortAction::SendGeneral { data, link_local } => {
                let to = A::group(link_local, GENERAL_PORT);
                if let Err(e) = sockets.general.send_to(data, to).await {
                    tracing::warn!(error = %e, "PTP send failed");
                }
            }
            PortAction::ResetAnnounceTimer { duration } => {
                timers.announce = Some(Instant::now() + duration);
            }
            PortAction::ResetSyncTimer { duration } => {
                timers.sync = Some(Instant::now() + duration);
            }
            PortAction::ResetDelayRequestTimer { duration } => {
                timers.delay_request = Some(Instant::now() + duration);
            }
            PortAction::ResetAnnounceReceiptTimer { duration } => {
                timers.announce_receipt = Some(Instant::now() + duration);
            }
            PortAction::ResetFilterUpdateTimer { duration } => {
                timers.filter_update = Some(Instant::now() + duration);
            }
            PortAction::ForwardTLV { .. } => {}
        }
    }
    pending
}

/// Counts a missing send timestamp; after too many in a row switches the
/// event socket to stamping before sending.
fn missing_send_timestamp<A: Transport>(sockets: &mut Sockets<A>) {
    sockets.missing += 1;
    if sockets.missing < MAX_MISSING_SEND_TIMESTAMPS {
        return;
    }
    let interface = sockets.interface;
    // The sockets reuse the address, so the new one binds next to the old.
    match open_socket(interface, EVENT_PORT, InterfaceTimestampMode::SoftwareRecv) {
        Ok(event) => {
            sockets.event = event;
            sockets.send_time = true;
            tracing::warn!(
                %interface,
                "no PTP send timestamps from the interface; using the time before sending"
            );
        }
        Err(e) => {
            sockets.missing = 0;
            tracing::warn!(error = %e, "PTP event socket could not be reopened");
        }
    }
}

/// Tracks how long the clock has looked settled.
#[derive(Default)]
struct Settle {
    rounds: u32,
}

/// Updates the status after a BMCA round.
fn publish(
    instance: &PtpInstance<Servo>,
    port: &BmcaPort<'_>,
    shared: &Shared,
    cfg: &PtpConfig,
    settle: &mut Settle,
) {
    let state = PtpState::from(port.port_ds().port_state);
    // A delay of exactly zero means the filter has no measurements yet.
    let estimate = port
        .port_current_ds_contribution()
        .filter(|e| e.mean_delay != Duration::ZERO);
    let offset_ns = estimate
        .as_ref()
        .map(|e| e.offset_from_master.nanos_lossy() as i64);
    let frequency_ppm = f64::from_bits(shared.frequency_ppm.load(Relaxed));
    let within =
        offset_ns.is_some_and(|o| o.unsigned_abs() < cfg.settle_threshold_us as u64 * 1000);
    settle.rounds = if state == PtpState::Slave && within {
        settle.rounds + 1
    } else {
        0
    };
    let settled = state == PtpState::Master || settle.rounds >= SETTLE_ROUNDS;
    if state == PtpState::Slave {
        shared.followed.store(true, Relaxed);
    }
    let grandmaster = format_identity(instance.parent_ds().grandmaster_identity);

    let mut status = shared.status.lock().unwrap();
    if status.state != state || status.settled != settled || status.grandmaster != grandmaster {
        tracing::info!(?state, settled, %grandmaster, "PTP");
    }
    *status = PtpStatus {
        state,
        grandmaster,
        offset_ns,
        mean_delay_ns: estimate.map(|e| e.mean_delay.nanos_lossy() as i64),
        frequency_ppm,
        steps: shared.steps.load(Relaxed),
        settled,
        ..status.clone()
    };
}

/// The clock identity for a MAC address: EUI-64 with `ff:fe` in the middle,
/// as ptp4l derives it.
fn eui64(mac: [u8; 6]) -> ClockIdentity {
    let m = mac;
    ClockIdentity([m[0], m[1], m[2], 0xff, 0xfe, m[3], m[4], m[5]])
}

/// `xxxxxx.xxxx.xxxxxx`, as ptp4l prints it.
fn format_identity(id: ClockIdentity) -> String {
    let b = id.0;
    format!(
        "{:02x}{:02x}{:02x}.{:02x}{:02x}.{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
    )
}

/// The interface of the default route, IPv4 preferred.
fn default_interface() -> io::Result<InterfaceName> {
    let v4 = fs::read_to_string("/proc/net/route").unwrap_or_default();
    let v6 = fs::read_to_string("/proc/net/ipv6_route").unwrap_or_default();
    default_route_v4(&v4)
        .or_else(|| default_route_v6(&v6))
        .and_then(|iface| iface.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no default route; set the PTP interface",
            )
        })
}

/// In `/proc/net/route`: interface, destination, gateway, … in hex.
fn default_route_v4(routes: &str) -> Option<&str> {
    routes.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let iface = fields.next()?;
        (fields.next()? == "00000000").then_some(iface)
    })
}

/// In `/proc/net/ipv6_route`: destination, prefix length, source, …,
/// flags, interface. Unreachable routes also have prefix length 0 but point
/// at `lo`.
fn default_route_v6(routes: &str) -> Option<&str> {
    routes.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [dest, prefix, .., iface] = fields[..] else {
            return None;
        };
        (fields.len() == 10 && prefix == "00" && dest.bytes().all(|b| b == b'0') && iface != "lo")
            .then_some(iface)
    })
}

/// IPv4 if the interface has an IPv4 address, otherwise IPv6 if it has one.
fn auto_transport(interface: InterfaceName, ips: &[IpAddr]) -> io::Result<PtpTransport> {
    if ips.iter().any(IpAddr::is_ipv4) {
        Ok(PtpTransport::Ipv4)
    } else if ips.iter().any(IpAddr::is_ipv6) {
        Ok(PtpTransport::Ipv6)
    } else {
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("interface {interface} has no IP address"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_estimate_pairs_sync_and_delay() {
        let ms = Time::from_millis;
        let mut d = DelayEstimate::default();
        // A delay measurement without a sync before it says nothing.
        d.delay(ms(1000), 0.002 - 0.004);
        assert_eq!(d.value, None);
        // Offset 2 ms, delay 4 ms: sync sees +6 ms, the delay request -2 ms.
        d.sync(ms(1000), 0.006);
        d.delay(ms(1100), -0.002);
        assert!((d.value.unwrap() - 0.004).abs() < 1e-12);
        // Too far apart to pair.
        d.sync(ms(2000), 0.016);
        d.delay(ms(2500), -0.002);
        assert_eq!(d.samples.len(), 1);
        // A step makes the last sync stale.
        d.sync(ms(3000), 0.006);
        d.stepped();
        d.delay(ms(3100), -0.010);
        assert_eq!(d.samples.len(), 1);
        // The median ignores outliers.
        for (i, delay) in [0.004, 0.030, 0.004, 0.004].into_iter().enumerate() {
            let at = 4000 + 1000 * i as u64;
            d.sync(ms(at), 0.001 + delay);
            d.delay(ms(at + 50), 0.001 - delay);
        }
        assert!((d.value.unwrap() - 0.004).abs() < 1e-12);
    }

    #[test]
    fn default_routes() {
        let v4 = "Iface\tDestination\tGateway\tFlags\n\
                  wlan0\t0018A8C0\t00000000\t0001\n\
                  eth0\t00000000\t0118A8C0\t0003\n";
        assert_eq!(default_route_v4(v4), Some("eth0"));
        assert_eq!(default_route_v4("Iface\tDestination\n"), None);
        let zero = "00000000000000000000000000000000";
        let v6 = format!(
            "{zero} 00 {zero} 00 {zero} ffffffff 00000001 00000000 00200200 lo\n\
             fd000000000000000000000000000000 40 {zero} 00 {zero} 00000100 00000001 00000000 00000001 eth0\n\
             {zero} 00 {zero} 00 fe800000000000000000000000000001 00000400 00000001 00000000 00450003 eth0\n"
        );
        assert_eq!(default_route_v6(&v6), Some("eth0"));
    }

    #[test]
    fn transport_groups() {
        assert_eq!(
            SocketAddrV4::group(false, EVENT_PORT).to_string(),
            "224.0.1.129:319"
        );
        assert_eq!(
            SocketAddrV6::group(false, EVENT_PORT).to_string(),
            "[ff0e::181]:319"
        );
        assert_eq!(
            SocketAddrV6::group(true, GENERAL_PORT).to_string(),
            "[ff02::6b]:320"
        );
    }

    #[test]
    fn identity_format() {
        let id = eui64([0x4c, 0xb0, 0x4a, 0x50, 0xa5, 0x82]);
        assert_eq!(format_identity(id), "4cb04a.fffe.50a582");
    }

    #[test]
    fn overlay_follows_steps_and_frequency() {
        let shared = Arc::new(Shared::new(Timescale::tai(), 0));
        let mut clock = PortClock(shared.clone());
        let ptp = PtpClock {
            shared: shared.clone(),
            port: Mutex::new(PortThread {
                cfg: PtpConfig::default(),
                stop: None,
                thread: None,
            }),
        };
        let system = crate::SystemClock::new();
        let diff = |ptp: &PtpClock| ptp.now_ns() as i64 - system.now_ns() as i64;
        assert!(diff(&ptp).abs() < 1_000_000);
        statime::Clock::step_clock(&mut clock, Duration::from_nanos(500_000_000)).unwrap();
        assert!((diff(&ptp) - 500_000_000).abs() < 1_000_000);
        statime::Clock::set_frequency(&mut clock, 100.0).unwrap();
        assert_eq!(shared.steps.load(Relaxed), 1);
        assert_eq!(f64::from_bits(shared.frequency_ppm.load(Relaxed)), 100.0);
        // A socket timestamp of "now" maps to about the virtual time now.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let ts = Timestamp {
            seconds: now.as_secs() as i64,
            nanos: now.subsec_nanos(),
        };
        let mapped: u64 = clock.time_of(ts).nanos().to_num();
        assert!((mapped as i64 - ptp.now_ns() as i64).abs() < 1_000_000);
    }

    /// A clock that only moves when told to.
    struct TestClock {
        now: Time,
        steps: u32,
    }

    impl statime::Clock for TestClock {
        type Error = io::Error;

        fn now(&self) -> Time {
            self.now
        }

        fn step_clock(&mut self, offset: Duration) -> Result<Time, io::Error> {
            self.now += offset;
            self.steps += 1;
            Ok(self.now)
        }

        fn set_frequency(&mut self, _: f64) -> Result<Time, io::Error> {
            Ok(self.now)
        }

        fn set_properties(&mut self, _: &TimePropertiesDS) -> Result<(), io::Error> {
            Ok(())
        }
    }

    #[test]
    fn servo_ignores_measurements_from_before_a_step() {
        let mut clock = TestClock {
            now: Time::from_secs(1000),
            steps: 0,
        };
        let mut servo = Servo::new(KalmanConfiguration::default());
        let sync = |clock: &TestClock, offset: Duration| Measurement {
            event_time: clock.now,
            raw_sync_offset: Some(offset),
            ..Default::default()
        };
        // Cold start, 200 ms off: nothing until a delay request gives the
        // delay (here none), then one step.
        servo.measurement(sync(&clock, Duration::from_millis(200)), &mut clock);
        assert_eq!(clock.steps, 0);
        clock.now += Duration::from_millis(10);
        let delay = Measurement {
            event_time: clock.now,
            raw_delay_offset: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        servo.measurement(delay, &mut clock);
        assert_eq!(clock.steps, 1);
        // A measurement taken before the step arrives after it. statime's
        // filter alone would step back.
        clock.now += Duration::from_millis(100);
        servo.measurement(sync(&clock, Duration::from_millis(200)), &mut clock);
        assert_eq!(clock.steps, 1);
        // Later measurements go to a fresh filter.
        clock.now += Duration::from_secs(1);
        servo.measurement(sync(&clock, Duration::from_micros(2)), &mut clock);
        assert_eq!(clock.steps, 1);
        let offset = servo.current_estimates().offset_from_master;
        assert!(offset.abs() < Duration::from_micros(10), "{offset:?}");
    }

    #[test]
    fn servo_settles_behind_a_slow_path() {
        // 5 ms each way, five times the step threshold, and 300 µs off.
        // statime's filter alone resets and steps on every measurement.
        let mut clock = TestClock {
            now: Time::from_secs(1000),
            steps: 0,
        };
        let mut servo = Servo::new(KalmanConfiguration::default());
        let (delay, offset) = (Duration::from_millis(5), Duration::from_micros(300));
        for i in 0..40 {
            clock.now += Duration::from_millis(125);
            let mut m = Measurement {
                event_time: clock.now,
                ..Default::default()
            };
            if i % 2 == 0 {
                m.raw_sync_offset = Some(offset + delay);
            } else {
                m.raw_delay_offset = Some(offset - delay);
            }
            servo.measurement(m, &mut clock);
        }
        assert_eq!(clock.steps, 0);
        // The test clock ignores frequency steering, so offset and delay only
        // roughly matches.
        let e = servo.current_estimates();
        let off = e.offset_from_master - offset;
        assert!(off.abs() < Duration::from_micros(200), "{off:?}");
        let d = e.mean_delay - delay;
        assert!(d.abs() < Duration::from_micros(50), "{d:?}");
    }

    /// A follower's clock with a drifting crystal, whose error from the
    /// grandmaster the servo's steps and frequency changes act on.
    struct SimClock {
        now: Time,
        /// This clock minus the grandmaster's, in seconds.
        error: f64,
        crystal_ppm: f64,
        steering_ppm: f64,
        steps: u32,
    }

    impl SimClock {
        fn advance(&mut self, seconds: f64) {
            self.now += Duration::from_seconds(seconds);
            self.error += seconds * (self.crystal_ppm + self.steering_ppm) * 1e-6;
        }
    }

    impl statime::Clock for SimClock {
        type Error = io::Error;

        fn now(&self) -> Time {
            self.now
        }

        fn step_clock(&mut self, offset: Duration) -> Result<Time, io::Error> {
            self.now += offset;
            self.error += offset.seconds();
            self.steps += 1;
            Ok(self.now)
        }

        fn set_frequency(&mut self, ppm: f64) -> Result<Time, io::Error> {
            self.steering_ppm = ppm;
            Ok(self.now)
        }

        fn set_properties(&mut self, _: &TimePropertiesDS) -> Result<(), io::Error> {
            Ok(())
        }
    }

    /// What a run of [`simulate`] saw after its first minute.
    #[derive(Debug)]
    struct Run {
        steps: u32,
        /// The largest error from the grandmaster, in seconds.
        worst: f64,
        /// The largest change of the steering between two syncs, in ppm.
        worst_jump_ppm: f64,
        /// The error at the end, in seconds.
        last: f64,
    }

    /// `seconds` of AES67 rates (8 syncs, 1 delay request a second) over a
    /// 3 ms path, starting 50 ms off with a 30 ppm crystal. `wait` gives
    /// how long a message waited on the way at a time, in seconds; `jump`
    /// when and by how much the grandmaster's time jumps.
    fn simulate(seconds: u32, mut wait: impl FnMut(f64) -> f64, jump: Option<(f64, f64)>) -> Run {
        let mut clock = SimClock {
            now: Time::from_secs(1000),
            error: 0.05,
            crystal_ppm: 30.0,
            steering_ppm: 0.0,
            steps: 0,
        };
        let mut servo = Servo::new(KalmanConfiguration {
            step_threshold: Duration::from_millis(20),
            ..KalmanConfiguration::default()
        });
        let path = 0.003;
        let mut run = Run {
            steps: 0,
            worst: 0.0,
            worst_jump_ppm: 0.0,
            last: 0.0,
        };
        let mut update_at: Option<f64> = None;
        let mut last_ppm = 0.0;
        for i in 0..seconds * 8 {
            let t = i as f64 / 8.0;
            clock.advance(0.125);
            if let Some((at, by)) = jump
                && t == at
            {
                clock.error -= by;
            }
            if update_at.is_some_and(|u| u <= t) {
                update_at = None;
                let u = servo.update(&mut clock);
                update_at = u.next_update.map(|d| t + d.as_secs_f64()).or(update_at);
            }
            let mut m = Measurement {
                event_time: clock.now,
                raw_sync_offset: Some(Duration::from_seconds(clock.error + path + wait(t))),
                ..Default::default()
            };
            let u = servo.measurement(m, &mut clock);
            update_at = u.next_update.map(|d| t + d.as_secs_f64()).or(update_at);
            if i % 8 == 4 {
                m = Measurement {
                    event_time: clock.now,
                    raw_delay_offset: Some(Duration::from_seconds(clock.error - path - wait(t))),
                    ..Default::default()
                };
                let u = servo.measurement(m, &mut clock);
                update_at = u.next_update.map(|d| t + d.as_secs_f64()).or(update_at);
            }
            if t >= 60.0 {
                if run.steps == 0 && t < 60.2 {
                    run.steps = clock.steps;
                }
                run.worst = run.worst.max(clock.error.abs());
                run.worst_jump_ppm = run
                    .worst_jump_ppm
                    .max((clock.steering_ppm - last_ppm).abs());
            }
            last_ppm = clock.steering_ppm;
        }
        run.steps = clock.steps - run.steps;
        run.last = clock.error;
        run
    }

    /// Wi-Fi with a client dozing now and then: for a burst of 0.5 to 5 s
    /// every 5 to 20 s, the access point holds every message until the next
    /// beacon (0 to 102.4 ms); otherwise up to 300 µs of jitter.
    fn dtim_bursts(seed: u64) -> impl FnMut(f64) -> f64 {
        use rand::Rng;
        let mut rng = StdRng::seed_from_u64(seed);
        let mut burst = (0.0, 0.0);
        move |t| {
            if t >= burst.1 {
                let start = t + rng.gen_range(5.0..20.0);
                burst = (start, start + rng.gen_range(0.5..5.0));
            }
            if t >= burst.0 {
                rng.gen_range(0.0..0.1024)
            } else {
                rng.gen_range(0.0..300e-6)
            }
        }
    }

    #[test]
    fn servo_rides_out_wifi_bursts() {
        for seed in 1..=5 {
            let run = simulate(600, dtim_bursts(seed), None);
            assert_eq!(run.steps, 0, "seed {seed}: {run:?}");
            assert!(run.worst < 0.001, "seed {seed}: {run:?}");
        }
    }

    #[test]
    fn servo_follows_a_real_jump() {
        // After 2 minutes the grandmaster's time jumps by 30 ms; the
        // follower steps once, after the gate's window, and settles again.
        let run = simulate(240, |_| 0.0, Some((120.0, 0.03)));
        assert_eq!(run.steps, 1, "{run:?}");
        assert!(run.last.abs() < 100e-6, "{run:?}");
    }
}
