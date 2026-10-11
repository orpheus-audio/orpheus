//! Measures the latency of a PipeWire audio path in graph samples.
//!
//! Plays short impulses into `<play-target>` and records the monitor of
//! `<monitor-sink>`. Both streams live in the same graph, so the graph clock
//! (`pw_time.ticks`) is a common time base: latency = ticks at detection -
//! ticks at emission. The result covers the software path up to the monitor
//! of the final sink, not the device's hardware buffer.
//!
//! ```text
//! cargo run -p orpheus-pipewire --example latency_probe -- orpheus.loopback my_sink 40
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use pipewire::{self as pw, properties::properties, spa};
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::pod::{Object, Pod, Value, serialize::PodSerializer};

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// Impulse spacing; must exceed the largest latency measured.
const PERIOD: u64 = RATE as u64 / 2;
const THRESHOLD: f32 = 0.1;

struct Shared {
    /// Graph time of the last emitted impulse, `u64::MAX` before the first.
    emitted_at: AtomicU64,
    /// Measured latencies in frames. A probe, so a try_lock in RT is fine.
    results: std::sync::Mutex<Vec<i64>>,
}

fn format_pod() -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(CHANNELS as u32);
    let mut pos = [0; spa::param::audio::MAX_CHANNELS];
    pos[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    pos[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(pos);
    let value = Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties: info.into(),
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .unwrap()
        .0
        .into_inner()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: latency_probe <play-target> <monitor-sink> [count]");
        std::process::exit(2);
    }
    let play_target = args[1].clone();
    let monitor = args[2].clone();
    let count: usize = args.get(3).map_or(40, |s| s.parse().unwrap());
    let node_latency = std::env::var("PROBE_LATENCY").unwrap_or_else(|_| "64/48000".into());

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).unwrap();
    let context = pw::context::ContextRc::new(&mainloop, None).unwrap();
    let core = context.connect_rc(None).unwrap();

    let shared = Arc::new(Shared {
        emitted_at: AtomicU64::new(u64::MAX),
        results: Default::default(),
    });

    // Generator: one impulse every PERIOD samples of graph time, placed at the
    // first frame of a cycle so its graph time is exactly `ticks`.
    let gen_stream = pw::stream::StreamRc::new(
        core.clone(),
        "orpheus-probe-gen",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::NODE_NAME => "orpheus-probe-gen",
            *pw::keys::TARGET_OBJECT => play_target.as_str(),
            *pw::keys::NODE_LATENCY => node_latency.as_str(),
        },
    )
    .unwrap();
    let next_emit = std::cell::Cell::new(0u64);
    let _gen_listener = gen_stream
        .add_local_listener_with_user_data(shared.clone())
        .process(move |stream, shared| {
            let ticks = stream.time().map(|t| t.ticks()).unwrap_or(0);
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let data = &mut buffer.datas_mut()[0];
            let stride = 4 * CHANNELS;
            let Some(bytes) = data.data() else { return };
            let frames = if requested > 0 {
                requested.min(bytes.len() / stride)
            } else {
                bytes.len() / stride
            };
            let samples: &mut [f32] = bytemuck::cast_slice_mut(&mut bytes[..frames * stride]);
            samples.fill(0.0);
            if ticks > 0 && ticks >= next_emit.get() {
                samples[0] = 0.5;
                samples[1] = 0.5;
                shared.emitted_at.store(ticks, Relaxed);
                next_emit.set(ticks + PERIOD);
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (frames * stride) as u32;
        })
        .register()
        .unwrap();

    // Recorder: captures the sink's monitor and timestamps the impulse.
    let rec_stream = pw::stream::StreamRc::new(
        core.clone(),
        "orpheus-probe-rec",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::NODE_NAME => "orpheus-probe-rec",
            *pw::keys::TARGET_OBJECT => monitor.as_str(),
            *pw::keys::STREAM_CAPTURE_SINK => "true",
            *pw::keys::NODE_LATENCY => node_latency.as_str(),
        },
    )
    .unwrap();
    let _rec_listener = rec_stream
        .add_local_listener_with_user_data(shared.clone())
        .process(|stream, shared| {
            let ticks = stream.time().map(|t| t.ticks()).unwrap_or(0);
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let data = &mut buffer.datas_mut()[0];
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else { return };
            let bytes = &bytes[offset..offset + size];
            let samples: &[f32] = bytemuck::cast_slice(bytes);
            let emitted = shared.emitted_at.load(Relaxed);
            if emitted == u64::MAX {
                return;
            }
            if let Some(i) = samples.iter().position(|s| s.abs() > THRESHOLD) {
                let at = ticks + (i / CHANNELS) as u64;
                if let Ok(mut r) = shared.results.try_lock() {
                    r.push(at as i64 - emitted as i64);
                }
            }
        })
        .register()
        .unwrap();

    let fmt = format_pod();
    let flags = pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS;
    rec_stream
        .connect(
            spa::utils::Direction::Input,
            None,
            flags,
            &mut [Pod::from_bytes(&fmt).unwrap()],
        )
        .unwrap();
    gen_stream
        .connect(
            spa::utils::Direction::Output,
            None,
            flags,
            &mut [Pod::from_bytes(&fmt).unwrap()],
        )
        .unwrap();

    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        let shared = shared.clone();
        let started = std::time::Instant::now();
        move |_| {
            let n = shared.results.lock().unwrap().len();
            if n >= count || started.elapsed() > Duration::from_secs(60) {
                mainloop.quit();
            }
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(100)),
        )
        .into_result()
        .unwrap();
    mainloop.run();

    let mut r = shared.results.lock().unwrap().clone();
    // Drop the first few: the path is still priming.
    let r: Vec<i64> = r.drain(..).skip(3).collect();
    if r.is_empty() {
        println!("no impulses detected");
        std::process::exit(1);
    }
    let mut s = r.clone();
    s.sort();
    let ms = |f: i64| f as f64 * 1000.0 / RATE as f64;
    println!(
        "n={} min={} ({:.2} ms) median={} ({:.2} ms) max={} ({:.2} ms)",
        s.len(),
        s[0],
        ms(s[0]),
        s[s.len() / 2],
        ms(s[s.len() / 2]),
        s[s.len() - 1],
        ms(s[s.len() - 1])
    );
}
