//! Sender → UDP → receiver on localhost, with a fake audio backend standing
//! in for PipeWire.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use orpheus_clock::{Clock, SystemClock, Timescale};
use orpheus_core::AudioFormat;
use orpheus_core::rtp::Encoding;
use orpheus_engine::*;

#[derive(Default, Clone)]
struct FakeBackend {
    sinks: Arc<Mutex<Vec<CapturePort>>>,
    playbacks: Arc<Mutex<Vec<PlaybackPort>>>,
}

impl AudioBackend for FakeBackend {
    fn create_sink(
        &mut self,
        _: &StreamSpec,
        port: CapturePort,
    ) -> Result<StreamHandle, BackendError> {
        self.sinks.lock().unwrap().push(port);
        Ok(StreamHandle::new(|| {}))
    }

    fn create_playback(
        &mut self,
        _: &StreamSpec,
        port: PlaybackPort,
    ) -> Result<StreamHandle, BackendError> {
        self.playbacks.lock().unwrap().push(port);
        Ok(StreamHandle::new(|| {}))
    }
}

const QUANTUM: usize = 240; // 5 ms
const LINK_OFFSET_MS: u32 = 5;
const FULL: f32 = (1 << 23) as f32;

/// Builds a sender → receiver route on `clock` and plays a ramp through it
/// for `cycles` audio cycles. The fake sound card runs on `Instant`. Each
/// sample is its frame number, which L24 carries exactly and the resamplers
/// (at a ratio of almost exactly 1) to within a fraction of a frame. Returns
/// the played samples (left channel).
fn play_ramp(clock: Arc<dyn Clock>, cycles: usize) -> (Engine, Vec<f64>) {
    let format = AudioFormat::AES67;
    let fake = FakeBackend::default();
    let mut engine = Engine::new(Box::new(fake.clone()), clock);
    engine
        .start_receiver(&ReceiverConfig {
            name: "rx".into(),
            description: "rx".into(),
            format,
            encoding: Encoding::L24,
            listen: "127.0.0.1:0".parse().unwrap(),
            link_offset_ms: LINK_OFFSET_MS,
            quantum: Some(QUANTUM as u32),
        })
        .unwrap();
    let listen = engine.status().receiver.unwrap().listen;
    engine
        .add_sender(&SenderConfig {
            name: "tx".into(),
            description: "tx".into(),
            format,
            encoding: Encoding::L24,
            peer: listen,
            quantum: Some(QUANTUM as u32),
        })
        .unwrap();
    let mut sink = fake.sinks.lock().unwrap().pop().unwrap();
    let mut playback = fake.playbacks.lock().unwrap().pop().unwrap();

    let mut out = vec![0.0f32; QUANTUM * 2];
    let mut played = Vec::new();
    let start = Instant::now();
    for cycle in 0..cycles {
        let input: Vec<f32> = (0..QUANTUM)
            .flat_map(|i| [(cycle * QUANTUM + i + 1) as f32 / FULL; 2])
            .collect();
        sink.push(&input);
        playback.fill(&mut out);
        played.extend(out.iter().step_by(2).map(|s| (s * FULL) as f64));
        let due = start + Duration::from_millis(5 * (cycle as u64 + 1));
        thread::sleep(due.saturating_duration_since(Instant::now()));
    }
    (engine, played)
}

/// Latency in frames of the audio at `at`, from the ramp value there.
fn latency_at(played: &[f64], at: usize) -> f64 {
    at as f64 - (played[at] - 1.0)
}

#[test]
fn audio_arrives_intact_and_on_time() {
    let (engine, played) = play_ramp(Arc::new(SystemClock::new()), 300);

    let first = played
        .iter()
        .position(|&s| s.abs() > 0.5)
        .expect("nothing arrived");
    let tail = &played[played.len() - 4800..];
    assert!(
        tail.windows(2).all(|w| (w[1] - w[0] - 1.0).abs() < 0.1),
        "last 100 ms not continuous"
    );
    // Latency = sender buffer (~1 cycle) + link offset + receiver buffer
    // (~1 cycle + 4 ms) + two resamplers (~0.7 ms each); allow for
    // scheduling.
    let ms = latency_at(&played, played.len() - tail.len()) / 48.0;
    assert!((10.0..40.0).contains(&ms), "latency {ms} ms");
    assert!(first > 0);

    let status = engine.status();
    let rx = status.receiver.unwrap();
    assert_eq!(rx.lost, 0);
    assert_eq!(rx.late, 0);
    assert!(rx.packets > 1000);
    assert_eq!(status.senders[0].1.send_errors, 0);
    eprintln!("latency {ms:.1} ms, {rx:?}");
}

/// The media clock runs 500 ppm fast against the sound card. Without ASRC
/// the sender's buffer would run dry about every 20 s and the receiver's
/// overflow; with it the latency holds. Takes two minutes.
#[test]
#[ignore]
fn drift_is_absorbed() {
    let skew = 500.0;
    let clock = SystemClock::with_timescale(Timescale::tai().skewed(skew, 0));
    let cycles = 24_000; // 120 s
    let (engine, played) = play_ramp(Arc::new(clock), cycles);

    let status = engine.status();
    let rx = status.receiver.unwrap();
    let tx = &status.senders[0].1;
    eprintln!("tx {tx:?}\nrx {rx:?}");
    // Both sides converge on the skew: the sender takes less from its port
    // than 1 ms per packet, the receiver puts more into its port.
    assert!((tx.asrc_ppm + skew).abs() < 50.0, "tx {}", tx.asrc_ppm);
    assert!((rx.asrc_ppm - skew).abs() < 50.0, "rx {}", rx.asrc_ppm);
    // No buffer had to re-prime or drop audio after the start.
    for buffer in [tx.buffer, rx.buffer] {
        assert_eq!(buffer.underruns, 0, "{buffer:?}");
        assert_eq!(buffer.resyncs, 0, "{buffer:?}");
    }
    // The latency 10 s in equals the latency at the end, within a few
    // frames (it would have grown by 60 ms without ASRC).
    let early = latency_at(&played, 48_000 * 10);
    let late = latency_at(&played, played.len() - 1);
    assert!(
        (early - late).abs() < 48.0,
        "latency {early} → {late} frames"
    );
    eprintln!("latency {early} → {late} frames");
}

/// Packets the receiver took and ignored over `ms`.
fn counts_over(engine: &Engine, ms: u64) -> (u64, u64) {
    let rx = |e: &Engine| e.status().receiver.unwrap();
    let before = rx(engine);
    thread::sleep(Duration::from_millis(ms));
    let after = rx(engine);
    (
        after.packets - before.packets,
        after.ignored - before.ignored,
    )
}

#[test]
fn routes_and_filters_change_at_runtime() {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let mut engine = Engine::new(Box::new(FakeBackend::default()), clock);
    engine
        .start_receiver(&ReceiverConfig {
            name: "rx".into(),
            description: "rx".into(),
            format: AudioFormat::AES67,
            encoding: Encoding::L24,
            listen: "127.0.0.1:0".parse().unwrap(),
            link_offset_ms: LINK_OFFSET_MS,
            quantum: Some(QUANTUM as u32),
        })
        .unwrap();
    let listen = engine.status().receiver.unwrap().listen;
    let id = engine
        .add_sender(&SenderConfig {
            name: "tx".into(),
            description: "tx".into(),
            format: AudioFormat::AES67,
            encoding: Encoding::L24,
            peer: listen,
            quantum: Some(QUANTUM as u32),
        })
        .unwrap();
    let (taken, ignored) = counts_over(&engine, 100);
    assert!(taken > 50 && ignored == 0, "{taken} {ignored}");

    // Another payload type or source: ignored.
    for filter in [
        Filter {
            payload_type: Some(96),
            ..Filter::default()
        },
        Filter {
            source: Some("127.0.0.2".parse().unwrap()),
            ..Filter::default()
        },
        Filter {
            enabled: false,
            ..Filter::default()
        },
    ] {
        engine.set_receiver_filter(filter).unwrap();
        let (taken, ignored) = counts_over(&engine, 100);
        assert!(taken <= 1 && ignored > 50, "{filter:?}: {taken} {ignored}");
    }
    let own = Filter {
        source: Some("127.0.0.1".parse().unwrap()),
        payload_type: Some(sender::PAYLOAD_TYPE),
        ..Filter::default()
    };
    engine.set_receiver_filter(own).unwrap();
    let (taken, _) = counts_over(&engine, 100);
    assert!(taken > 50, "{taken}");

    // A paused sender sends nothing.
    let mut route = Route::to(listen);
    route.enabled = false;
    assert!(engine.set_sender_route(id, route).unwrap());
    let (taken, ignored) = counts_over(&engine, 100);
    assert!(taken <= 1 && ignored == 0, "{taken} {ignored}");

    // Both move to another port, the sender also to another source port.
    engine.set_receiver_port(0).unwrap();
    let listen = engine.status().receiver.unwrap().listen;
    let mut route = Route::to(listen);
    route.source = "127.0.0.1:0".parse().unwrap();
    engine.set_sender_route(id, route).unwrap();
    thread::sleep(Duration::from_millis(20));
    let (taken, _) = counts_over(&engine, 100);
    assert!(taken > 50, "{taken}");
    assert!(!engine.set_sender_route(id + 1, route).unwrap());
}
