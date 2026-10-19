//! Changing a sender's channels: the new sink starts before the old one
//! goes, with everything the old one had, against a fake audio system that
//! shows sinks a while after they are created, as PipeWire does.

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use orpheus_clock::{Clock, SystemClock};
use orpheus_core::rtp::Encoding;
use orpheus_core::{AudioFormat, Layout, OutputType};
use orpheus_engine::*;

#[derive(Debug, Clone, PartialEq)]
enum Event {
    /// Sink `n` was created with this many channels and this volume.
    Created(usize, usize, Option<Volume>),
    /// Sink `n` showed in the outputs.
    Shown(usize),
    Removed(usize),
}

struct Sink {
    name: String,
    layout: Layout,
    created: Instant,
    removed: bool,
}

#[derive(Default)]
struct State {
    sinks: Vec<Sink>,
    events: Vec<Event>,
    /// How long a sink takes to show; `None`: never.
    delay: Option<Duration>,
    fail: bool,
}

#[derive(Clone)]
struct Fake(Arc<Mutex<State>>);

impl Fake {
    fn new(delay: Option<Duration>) -> Self {
        Self(Arc::new(Mutex::new(State {
            delay,
            ..State::default()
        })))
    }

    fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().events.clone()
    }

    /// The channel counts of the sinks not removed.
    fn live(&self) -> Vec<usize> {
        let s = self.0.lock().unwrap();
        s.sinks
            .iter()
            .filter(|k| !k.removed)
            .map(|k| k.layout.len())
            .collect()
    }
}

impl AudioBackend for Fake {
    fn create_sink(
        &mut self,
        spec: &StreamSpec,
        _: CapturePort,
    ) -> Result<StreamHandle, BackendError> {
        let mut s = self.0.lock().unwrap();
        if s.fail {
            return Err("no more sinks".into());
        }
        let n = s.sinks.len();
        s.sinks.push(Sink {
            name: spec.name.clone(),
            layout: spec.layout.clone(),
            created: Instant::now(),
            removed: false,
        });
        s.events
            .push(Event::Created(n, spec.layout.len(), spec.volume.clone()));
        let state = self.0.clone();
        Ok(StreamHandle::new(move || {
            let mut s = state.lock().unwrap();
            s.sinks[n].removed = true;
            s.events.push(Event::Removed(n));
        }))
    }

    fn create_playback(
        &mut self,
        _: &StreamSpec,
        _: PlaybackPort,
    ) -> Result<StreamHandle, BackendError> {
        Err("no playback here".into())
    }

    fn outputs(&self) -> Vec<Output> {
        let mut s = self.0.lock().unwrap();
        let Some(delay) = s.delay else {
            return Vec::new();
        };
        let mut shown = Vec::new();
        for (n, k) in s.sinks.iter().enumerate() {
            if !k.removed && k.created.elapsed() >= delay {
                shown.push((n, k.name.clone(), k.layout.clone()));
            }
        }
        for &(n, _, _) in &shown {
            if !s.events.contains(&Event::Shown(n)) {
                s.events.push(Event::Shown(n));
            }
        }
        shown
            .into_iter()
            .map(|(_, name, channels)| Output {
                description: name.clone(),
                name,
                kind: OutputType::Generic,
                default: false,
                channels,
            })
            .collect()
    }
}

const ID: SenderId = SenderId::from_bits(1);

/// An engine running one stereo sender to a socket that takes its packets.
fn engine(fake: &Fake) -> (Engine, UdpSocket) {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let mut engine = Engine::new(Box::new(fake.clone()), clock);
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    engine
        .insert_sender(
            ID,
            &SenderConfig {
                name: "orpheus.send.TEST".into(),
                description: "Kitchen".into(),
                format: AudioFormat::AES67,
                layout: Layout::default_for(2),
                encoding: Encoding::L24,
                peer: peer.local_addr().unwrap(),
                remote: OutputType::Speaker,
                block_frames: Some(256),
                volume: Some(Volume {
                    channels: vec![0.5, 0.5],
                    mute: false,
                }),
                fade_in_ms: 0,
            },
        )
        .unwrap();
    (engine, peer)
}

fn layout(text: &str) -> Layout {
    Layout::parse(text).unwrap()
}

#[test]
fn the_new_sink_shows_before_the_old_one_goes() {
    let fake = Fake::new(Some(Duration::from_millis(50)));
    let (mut engine, _peer) = engine(&fake);
    let elsewhere: SocketAddr = "127.0.0.1:9".parse().unwrap();
    assert!(engine.set_sender_route(ID, Route::to(elsewhere)).unwrap());
    assert!(engine.pause_sender(ID, true));
    assert!(engine.set_sender_label(ID, Some("Kitchen (unavailable)")));

    assert!(engine.set_sender_layout(ID, layout("51")).unwrap());

    // Sink 0 is the old one; the fake hasn't been asked for outputs before.
    let events = fake.events();
    let volume = Some(Volume {
        channels: vec![0.5, 0.5],
        mute: false,
    });
    assert_eq!(events[0], Event::Created(0, 2, volume.clone()));
    assert_eq!(events[1], Event::Created(1, 6, volume));
    let at = |e: Event| events.iter().position(|x| *x == e).unwrap();
    assert!(
        at(Event::Shown(1)) < at(Event::Removed(0)),
        "the old sink went before the new one showed: {events:?}"
    );
    assert_eq!(fake.live(), [6]);

    assert_eq!(engine.sender_layout(ID), Some(&layout("51")));
    assert_eq!(engine.sender_label(ID), Some("Kitchen (unavailable)"));
    assert_eq!(engine.sender_name(ID).as_deref(), Some("Kitchen"));
    let status = &engine.status().senders[0].1;
    assert_eq!(status.peer, elsewhere);
    assert!(!status.enabled, "no longer paused");
}

#[test]
fn as_many_channels_as_before_wait_for_the_second_sink() {
    let fake = Fake::new(Some(Duration::from_millis(50)));
    let (mut engine, _peer) = engine(&fake);
    // Let the old sink show, so that it counts.
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(engine.outputs().len(), 1);

    assert!(engine.set_sender_layout(ID, layout("DM")).unwrap());
    let events = fake.events();
    let at = |e: Event| events.iter().position(|x| *x == e).unwrap();
    assert!(
        at(Event::Shown(1)) < at(Event::Removed(0)),
        "the old sink went before the new one showed: {events:?}"
    );
    assert_eq!(fake.live(), [2]);
    assert_eq!(engine.sender_layout(ID), Some(&layout("DM")));
}

#[test]
fn a_sink_that_never_shows_replaces_the_old_one_after_a_second() {
    let fake = Fake::new(None);
    let (mut engine, _peer) = engine(&fake);
    let started = Instant::now();
    assert!(engine.set_sender_layout(ID, layout("51")).unwrap());
    let took = started.elapsed();
    assert!(
        (Duration::from_secs(1)..Duration::from_secs(2)).contains(&took),
        "took {took:?}"
    );
    assert_eq!(fake.live(), [6]);
}

#[test]
fn a_sink_that_fails_leaves_the_old_one() {
    let fake = Fake::new(Some(Duration::ZERO));
    let (mut engine, _peer) = engine(&fake);
    fake.0.lock().unwrap().fail = true;
    assert!(engine.set_sender_layout(ID, layout("51")).is_err());
    assert_eq!(fake.live(), [2]);
    assert_eq!(engine.sender_layout(ID), Some(&layout("ST")));
    assert_eq!(engine.status().senders.len(), 1);
}

#[test]
fn a_hidden_sender_keeps_the_layout_for_when_it_shows() {
    let fake = Fake::new(Some(Duration::ZERO));
    let (mut engine, _peer) = engine(&fake);
    assert!(engine.hide_sender(ID));
    assert_eq!(fake.live(), Vec::<usize>::new());

    assert!(engine.set_sender_layout(ID, layout("U16")).unwrap());
    assert_eq!(fake.live(), Vec::<usize>::new(), "it stays hidden");
    assert_eq!(engine.sender_layout(ID), Some(&layout("U16")));

    assert!(engine.show_sender(ID).unwrap());
    assert_eq!(fake.live(), [16]);
}

#[test]
fn no_such_sender() {
    let fake = Fake::new(Some(Duration::ZERO));
    let (mut engine, _peer) = engine(&fake);
    let other = SenderId::from_bits(2);
    assert!(!engine.set_sender_layout(other, layout("51")).unwrap());
    assert_eq!(fake.live(), [2]);
}
