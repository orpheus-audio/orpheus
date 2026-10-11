//! Sender → UDP → receiver on localhost, with a fake audio backend standing
//! in for PipeWire.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use orpheus_clock::SystemClock;
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

#[test]
fn audio_arrives_intact_and_on_time() {
    const QUANTUM: usize = 240; // 5 ms
    const LINK_OFFSET_MS: u32 = 5;
    let format = AudioFormat::AES67;
    let fake = FakeBackend::default();
    let mut engine = Engine::new(Box::new(fake.clone()), Arc::new(SystemClock));
    engine
        .start_receiver(&ReceiverConfig {
            name: "rx".into(),
            description: "rx".into(),
            format,
            encoding: Encoding::L24,
            listen: "127.0.0.1:0".parse().unwrap(),
            link_offset_ms: LINK_OFFSET_MS,
            quantum: Some(QUANTUM as u32),
            target: None,
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

    // Play a ramp into the sink and record the playback side, one cycle at a
    // time like the audio system would. Each sample is its frame number,
    // which L24 carries exactly.
    let full = (1 << 23) as f32;
    let cycles = 300;
    let mut out = vec![0.0f32; QUANTUM * 2];
    let mut played = Vec::new();
    let start = Instant::now();
    for cycle in 0..cycles {
        let input: Vec<f32> = (0..QUANTUM)
            .flat_map(|i| [(cycle * QUANTUM + i + 1) as f32 / full; 2])
            .collect();
        sink.push(&input);
        playback.fill(&mut out);
        played.extend(out.iter().step_by(2).map(|s| (s * full) as i64));
        let due = start + Duration::from_millis(5 * (cycle as u64 + 1));
        thread::sleep(due.saturating_duration_since(Instant::now()));
    }

    let first = played
        .iter()
        .position(|&s| s != 0)
        .expect("nothing arrived");
    let tail = &played[played.len() - 4800..];
    assert!(
        tail.windows(2).all(|w| w[1] == w[0] + 1),
        "last 100 ms not continuous"
    );
    // Latency = sender buffer (~1 cycle) + link offset + receiver buffer
    // (~1 cycle + 4 ms); allow for scheduling.
    let latency = (played.len() - tail.len()) as i64 - (tail[0] - 1);
    let ms = latency as f64 / 48.0;
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
