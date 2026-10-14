//! Which of the streams a node receives are heard when several play at
//! once (`[receiver] policy`), and ducking under high-priority streams.
//!
//! A stream plays while it carries sound or did within the last
//! [`HOLD`], so a pause between two songs doesn't count as stopping.
//! Among the normal streams that play, the policy picks the ones heard and
//! mutes the others; high-priority streams are always heard, and while one
//! plays, the normal ones heard are turned down by `[receiver] duck_db`.
//! A silent stream gets the gain it would have if it started now, so that
//! it is heard (or not) from its first sound on.
//!
//! The policy only sets gains, which the engine fades over 30 ms; it never
//! refuses or drops a connection. The level is measured before the gain,
//! so a muted stream is still known to play.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orpheus_engine::ReceiverId;
use orpheus_nmos::sdp::Priority;
use serde::Serialize;
use utoipa::ToSchema;

use crate::api::Node;
use crate::config::Policy;

/// How long a stream counts as playing after its last sound.
pub const HOLD: Duration = Duration::from_secs(5);

/// How often the gains are set.
const TICK: Duration = Duration::from_millis(50);

/// What the policy does to a receiver's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// No sound for [`HOLD`].
    Silent,
    /// Playing and heard.
    Playing,
    /// Playing, but muted: another stream is heard instead.
    Muted,
    /// Playing, turned down under a high-priority stream.
    Ducked,
}

impl State {
    /// The value of the IS-04 state tag, for what other nodes should know.
    pub fn tag(self) -> Option<&'static str> {
        match self {
            State::Muted => Some("muted"),
            State::Ducked => Some("ducked"),
            State::Silent | State::Playing => None,
        }
    }
}

/// The gain a stream gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gain {
    Full,
    /// Turned down by `duck_db`.
    Down,
    Off,
}

/// The policy's memory: since when each stream plays, and what it decided.
#[derive(Default)]
pub struct Mixer {
    since: BTreeMap<ReceiverId, Instant>,
    states: BTreeMap<ReceiverId, State>,
}

impl Mixer {
    /// What the policy does to receiver `id`'s stream; `None` if it has
    /// none.
    pub fn state(&self, id: ReceiverId) -> Option<State> {
        self.states.get(&id).copied()
    }
}

/// A running stream as the policy sees it.
struct Input {
    id: ReceiverId,
    priority: Priority,
    /// Since when it plays; `None` while silent.
    since: Option<Instant>,
}

/// The state and gain of each stream.
fn decide(policy: Policy, inputs: &[Input]) -> Vec<(ReceiverId, State, Gain)> {
    let high_plays = inputs
        .iter()
        .any(|i| i.priority == Priority::High && i.since.is_some());
    let normal: Vec<(Instant, ReceiverId)> = inputs
        .iter()
        .filter(|i| i.priority == Priority::Normal)
        .filter_map(|i| Some((i.since?, i.id)))
        .collect();
    let chosen: Vec<ReceiverId> = match policy {
        Policy::Mix => normal.iter().map(|&(_, id)| id).collect(),
        Policy::Last => normal.iter().max().map(|&(_, id)| id).into_iter().collect(),
        Policy::First => normal.iter().min().map(|&(_, id)| id).into_iter().collect(),
    };
    let heard = if high_plays { Gain::Down } else { Gain::Full };
    inputs
        .iter()
        .map(|i| {
            let (state, gain) = match (i.priority, i.since) {
                (Priority::High, Some(_)) => (State::Playing, Gain::Full),
                (Priority::High, None) => (State::Silent, Gain::Full),
                (Priority::Normal, Some(_)) if !chosen.contains(&i.id) => (State::Muted, Gain::Off),
                (Priority::Normal, Some(_)) if high_plays => (State::Ducked, heard),
                (Priority::Normal, Some(_)) => (State::Playing, heard),
                // As it would be heard if it started now.
                (Priority::Normal, None) if policy == Policy::First && !normal.is_empty() => {
                    (State::Silent, Gain::Off)
                }
                (Priority::Normal, None) => (State::Silent, heard),
            };
            (i.id, state, gain)
        })
        .collect()
}

/// Sets the receivers' gains as the policy says, every [`TICK`].
pub async fn run(node: Arc<Node>) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        apply(&node);
    }
}

fn apply(node: &Node) {
    let Some(engine) = &node.engine else {
        return;
    };
    let settings = node.settings().receiver;
    let priorities = node.connections.lock().unwrap().receiver_priorities();
    let mut engine = engine.lock().unwrap();
    let mut mixer = node.mixer.lock().unwrap();
    let now = Instant::now();
    let hold = HOLD.as_millis() as u64;
    let inputs: Vec<Input> = engine
        .receiver_silence()
        .into_iter()
        .map(|(id, silent_ms)| {
            let since = if silent_ms.is_some_and(|ms| ms < hold) {
                Some(*mixer.since.entry(id).or_insert(now))
            } else {
                mixer.since.remove(&id);
                None
            };
            Input {
                id,
                priority: priorities.get(&id).copied().unwrap_or_default(),
                since,
            }
        })
        .collect();
    mixer
        .since
        .retain(|id, _| inputs.iter().any(|i| i.id == *id));

    let ducked = 10f32.powf(-settings.duck_db / 20.0);
    let mut states = BTreeMap::new();
    for (id, state, gain) in decide(settings.policy, &inputs) {
        engine.set_receiver_gain(
            id,
            match gain {
                Gain::Full => 1.0,
                Gain::Down => ducked,
                Gain::Off => 0.0,
            },
        );
        if mixer.state(id) != Some(state) {
            tracing::debug!(id, ?state, "receiver's stream");
        }
        states.insert(id, state);
    }
    mixer.states = states;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Streams by (priority, seconds since they started playing).
    fn run(policy: Policy, streams: &[(Priority, Option<u64>)]) -> Vec<(State, Gain)> {
        let now = Instant::now() + Duration::from_secs(100);
        let inputs: Vec<Input> = streams
            .iter()
            .enumerate()
            .map(|(id, &(priority, ago))| Input {
                id: id as ReceiverId,
                priority,
                since: ago.map(|s| now - Duration::from_secs(s)),
            })
            .collect();
        decide(policy, &inputs)
            .into_iter()
            .map(|(_, s, g)| (s, g))
            .collect()
    }

    use Gain::*;
    use Priority::{High, Normal};
    use State::*;

    #[test]
    fn last_hears_the_newest() {
        let two = [(Normal, Some(30)), (Normal, Some(2)), (Normal, None)];
        assert_eq!(
            run(Policy::Last, &two),
            [(Muted, Off), (Playing, Full), (Silent, Full)]
        );
        // Alone, either is heard.
        assert_eq!(run(Policy::Last, &two[..1]), [(Playing, Full)]);
    }

    #[test]
    fn first_keeps_the_oldest_and_holds_new_ones_back() {
        let two = [(Normal, Some(30)), (Normal, Some(2)), (Normal, None)];
        assert_eq!(
            run(Policy::First, &two),
            [(Playing, Full), (Muted, Off), (Silent, Off)]
        );
        assert_eq!(
            run(Policy::First, &[(Normal, None), (Normal, None)]),
            [(Silent, Full), (Silent, Full)]
        );
    }

    #[test]
    fn mix_hears_all() {
        assert_eq!(
            run(Policy::Mix, &[(Normal, Some(30)), (Normal, Some(2))]),
            [(Playing, Full), (Playing, Full)]
        );
    }

    #[test]
    fn high_priority_ducks_the_rest() {
        let streams = [
            (Normal, Some(30)),
            (Normal, Some(2)),
            (High, Some(1)),
            (Normal, None),
        ];
        assert_eq!(
            run(Policy::Last, &streams),
            [
                (Muted, Off),
                (Ducked, Down),
                (Playing, Full),
                (Silent, Down)
            ]
        );
        // A silent high-priority stream ducks nothing.
        assert_eq!(
            run(Policy::Last, &[(Normal, Some(30)), (High, None)]),
            [(Playing, Full), (Silent, Full)]
        );
        // High-priority streams mix with each other.
        assert_eq!(
            run(Policy::First, &[(High, Some(30)), (High, Some(2))]),
            [(Playing, Full), (Playing, Full)]
        );
    }

    #[test]
    fn tags_name_what_others_should_know() {
        assert_eq!(Muted.tag(), Some("muted"));
        assert_eq!(Ducked.tag(), Some("ducked"));
        assert_eq!(Playing.tag(), None);
        assert_eq!(Silent.tag(), None);
    }
}
