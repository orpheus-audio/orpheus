//! Connect on play. With `[sender] claim = "on_play"`, a route holds a
//! receiver on its peer only while something plays into its sink: it
//! claims a free one when applications start playing into the sink, when
//! sound arrives or when the sink becomes the default output, and lets it
//! go after `release_after_s` of silence. Meanwhile the sender sends
//! nothing ([`PeerState::Idle`]). With `claim = "always"`, a route that
//! has no receiver claims one at once.
//!
//! Either way a route whose peer has no free receiver waits for one
//! ([`PeerState::Busy`]) and tries again every few seconds. One whose peer
//! has no receiver that can play the stream ([`PeerState::Incompatible`])
//! tries again when the peer's receivers change, or every minute if
//! discovery doesn't tell.

use std::collections::BTreeSet;

use super::*;

/// How often the sinks are looked at.
const TICK: Duration = Duration::from_millis(100);

/// Sound this recent counts as playing.
const SOUND_MS: u64 = 500;

/// How often a sender waiting for a free receiver tries again.
const BUSY_RETRY: Duration = Duration::from_secs(5);

/// How often a sender whose peer has no fitting receiver tries again, if
/// discovery doesn't say when the peer's receivers change.
const UNFIT_RETRY: Duration = Duration::from_secs(60);

/// What the last round saw.
#[derive(Default)]
struct Seen {
    /// The senders whose sinks played.
    playing: BTreeSet<SenderId>,
    /// The default output.
    default: Option<String>,
}

enum Action {
    /// Claim a receiver; `first` unless the route already waits for one.
    Claim {
        first: bool,
    },
    Release(PeerReceiver),
}

/// Claims and lets go of the peers' receivers as the sinks play, ten
/// times a second.
pub async fn run(node: Arc<Node>) {
    let mut seen = Seen::default();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        for (id, action) in decide(&node, &mut seen) {
            let node = node.clone();
            tokio::task::spawn_blocking(move || match action {
                Action::Claim { first } => claim(&node, id, first),
                Action::Release(to) => release(&node, id, &to),
            });
        }
    }
}

/// Picks the routes to claim a receiver for and those to let go of theirs.
/// Marks them as claiming or idle at once, so that the next round leaves
/// them alone.
fn decide(node: &Node, seen: &mut Seen) -> Vec<(SenderId, Action)> {
    let Some(engine) = &node.engine else {
        return vec![];
    };
    // Before the locks below: the peers' receiver change counters.
    let counters: BTreeMap<String, Option<u8>> = node
        .discovery
        .as_ref()
        .map(|d| d.nodes())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|n| Some((n.id?, n.receivers)))
        .collect();
    let settings = node.settings().sender;
    let on_play = settings.claim == Claim::OnPlay;
    let release_after = Duration::from_secs(settings.release_after_s.into());
    let mut conns = node.connections.lock().unwrap();
    let mut engine = engine.lock().unwrap();
    let status: BTreeMap<SenderId, _> = engine.status().senders.into_iter().collect();
    let default = engine
        .outputs()
        .into_iter()
        .find(|o| o.default)
        .map(|o| o.name);
    let now = Instant::now();
    let mut actions = vec![];
    for (&id, link) in &mut conns.links {
        let Some(to) = link.receiver.clone() else {
            continue;
        };
        let Some(s) = status.get(&id).filter(|s| !s.hidden) else {
            continue;
        };
        let sink = outputs::sender_sink(id);
        let started = s.playing && !seen.playing.contains(&id);
        let chosen = default.as_deref() == Some(&sink) && seen.default.as_deref() != Some(&sink);
        let sound = s.silent_ms.is_some_and(|ms| ms < SOUND_MS);
        let wanted = !on_play || started || chosen || sound;
        // Routes restored at start have no time: they may go at once.
        let retry = link.since.is_none_or(|t| t.elapsed() >= BUSY_RETRY);
        let held = link.since.is_some_and(|t| t.elapsed() < release_after);
        let counter = to
            .node
            .as_ref()
            .and_then(|n| counters.get(n))
            .copied()
            .flatten();
        let refit = match (counter, link.unfit_at) {
            (Some(now), Some(then)) => now != then,
            _ => link.since.is_none_or(|t| t.elapsed() >= UNFIT_RETRY),
        };
        let quiet = !held
            && s.silent_ms
                .is_none_or(|ms| ms >= release_after.as_millis() as u64);
        match link.state {
            PeerState::Idle if wanted => {
                tracing::info!(%id, "the sink plays; claiming a receiver of the peer");
                link.state = PeerState::Connecting;
                link.since = Some(now);
                actions.push((id, Action::Claim { first: true }));
            }
            PeerState::Busy if wanted && retry => {
                link.state = PeerState::Connecting;
                link.since = Some(now);
                actions.push((id, Action::Claim { first: false }));
            }
            PeerState::Incompatible if wanted && refit => {
                link.state = PeerState::Connecting;
                link.since = Some(now);
                actions.push((id, Action::Claim { first: false }));
            }
            PeerState::Busy | PeerState::Incompatible if on_play && quiet => {
                tracing::info!(%id, "silent; no longer waiting for a receiver");
                link.state = PeerState::Idle;
                link.since = Some(now);
            }
            PeerState::Connected if on_play && quiet => {
                tracing::info!(
                    %id,
                    "silent for {}; letting the peer's receiver go",
                    humanize(release_after)
                );
                link.state = PeerState::Idle;
                link.since = Some(now);
                engine.pause_sender(id, true);
                actions.push((id, Action::Release(to)));
            }
            _ => {}
        }
    }
    seen.playing = status
        .iter()
        .filter(|(_, s)| s.playing)
        .map(|(&id, _)| id)
        .collect();
    seen.default = default;
    actions
}

/// "10 min", "45 s".
fn humanize(d: Duration) -> String {
    match d.as_secs() {
        s if s >= 60 && s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

/// Connects a free receiver of route `id`'s peer to it and resumes the
/// sender, which fades in; if the peer has none, the route waits for one,
/// and if none fits, for the peer's receivers to change.
fn claim(node: &Node, id: SenderId, first: bool) {
    let (from, channels) = {
        let conns = node.connections.lock().unwrap();
        let link = conns.links.get(&id);
        (
            link.and_then(|l| l.receiver.clone()),
            link.map(|l| l.layout().len()),
        )
    };
    let (Some(from), Some(channels), Some(mut dest)) = (from, channels, destination_of(node, id))
    else {
        return;
    };
    let ours: Vec<Uuid> = node
        .connections
        .lock()
        .unwrap()
        .senders
        .keys()
        .map(|&n| uuid_of(node, PortId::Sender(n)))
        .collect();
    let peer = peer::Peer {
        api: from.api.clone(),
        ip: dest.ip(),
        name: None,
        node: from.node.clone(),
    };
    let claimed = peer::find_receiver(&peer, &ours, false, channels).and_then(|t| {
        dest.set_port(t.port);
        let to = PeerReceiver {
            id: t.receiver,
            ..from.clone()
        };
        retarget(node, id, to.clone(), dest)?;
        attach(node, id, &to)
    });
    let mut unfit = None;
    let ok = match claimed {
        Ok(true) => true,
        Ok(false) => {
            tracing::debug!(%id, "another sender took the free receiver meanwhile");
            false
        }
        Err((StatusCode::UNPROCESSABLE_ENTITY, e)) => {
            unfit = Some(e);
            false
        }
        Err((_, e)) => {
            tracing::debug!(%id, error = %e, "claiming a receiver of the peer");
            false
        }
    };
    // What discovery says about the peer's receivers now, before the lock.
    let counter = from.node.as_ref().and_then(|n| {
        let nodes = node.discovery.as_ref()?.nodes();
        nodes
            .into_iter()
            .find(|d| d.id.as_ref() == Some(n))?
            .receivers
    });
    let mut conns = node.connections.lock().unwrap();
    let Some(link) = conns.links.get_mut(&id) else {
        return;
    };
    link.since = Some(Instant::now());
    link.unfit_at = counter.filter(|_| unfit.is_some());
    link.state = if ok {
        link.displaced = false;
        PeerState::Connected
    } else if unfit.is_some() {
        PeerState::Incompatible
    } else {
        PeerState::Busy
    };
    if ok {
        tracing::info!(%id, "claimed a receiver of the peer; sending");
    } else if let Some(e) = unfit {
        if first {
            tracing::warn!(%id, error = %e, "none of the peer's receivers can play the stream; waiting for one that can");
        }
    } else if first {
        tracing::info!(%id, "the peer has no free receiver; waiting for one");
    }
    if let Some(engine) = &node.engine {
        engine.lock().unwrap().pause_sender(id, !ok);
    }
}

/// Disconnects the peer's receiver `to` from route `id`'s sender, unless
/// it plays another sender by now. If the peer can't be reached, the peer
/// watch tries again.
pub(super) fn release(node: &Node, id: SenderId, to: &PeerReceiver) {
    let uuid = uuid_of(node, PortId::Sender(id));
    if let Err((_, e)) = peer::release(to, uuid) {
        tracing::warn!(%id, error = %e, "letting the peer's receiver go");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(humanize(Duration::from_secs(600)), "10 min");
        assert_eq!(humanize(Duration::from_secs(45)), "45 s");
        assert_eq!(humanize(Duration::from_secs(90)), "90 s");
    }
}
