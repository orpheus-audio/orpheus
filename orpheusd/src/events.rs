//! Snapshots pushed over a WebSocket (`/x-orpheus/v1/events`), so that
//! clients need not poll.
//!
//! Each message is one topic's full state, as its GET returns it:
//! `{"topic": "status", "data": {…}}`. A client gets every topic it asked
//! for when it connects, then a topic again whenever it changed. Changes
//! are looked for every 250 ms, the status and the outputs, which are
//! dearer to put together and change all the time, every second. A slow
//! client skips states, never gets old ones.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use utoipa::ToSchema;

use crate::api::{self, Node};

/// How often changes are looked for.
const TICK: Duration = Duration::from_millis(250);
/// Ticks between looks at the status and the outputs.
const SLOW_TICKS: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Topic {
    /// As `GET /x-orpheus/v1/status`.
    Status,
    /// As `GET /x-orpheus/v1/nodes`; `null` while mDNS is not running.
    Nodes,
    /// As `GET /x-orpheus/v1/outputs`.
    Outputs,
    /// As `GET /x-orpheus/v1/settings`.
    Settings,
    /// As `GET /x-orpheus/v1/routes`.
    Routes,
    /// As `GET /x-orpheus/v1/sources`.
    Sources,
}

impl Topic {
    pub const ALL: [Topic; 6] = [
        Topic::Status,
        Topic::Nodes,
        Topic::Outputs,
        Topic::Settings,
        Topic::Routes,
        Topic::Sources,
    ];

    fn slow(self) -> bool {
        matches!(self, Topic::Status | Topic::Outputs | Topic::Sources)
    }

    /// The message for this topic's current state.
    fn message(self, node: &Node) -> Arc<str> {
        let data = api::snapshot(node, self);
        serde_json::json!({ "topic": self, "data": data })
            .to_string()
            .into()
    }
}

type Snapshots = BTreeMap<Topic, Arc<str>>;

/// The latest message of each topic, while anyone listens.
pub struct Events {
    latest: watch::Sender<Arc<Snapshots>>,
}

impl Default for Events {
    fn default() -> Self {
        Self {
            latest: watch::Sender::new(Arc::default()),
        }
    }
}

/// Looks for changes while clients are connected.
pub async fn watch(node: Arc<Node>) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    for n in (0..SLOW_TICKS).cycle() {
        tick.tick().await;
        if node.events.latest.receiver_count() == 0 {
            continue;
        }
        let slow = n == 0;
        let node = node.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let fresh: Vec<(Topic, Arc<str>)> = Topic::ALL
                .into_iter()
                .filter(|t| slow || !t.slow())
                .map(|t| (t, t.message(&node)))
                .collect();
            node.events.latest.send_if_modified(|latest| {
                let changed: Vec<_> = fresh
                    .into_iter()
                    .filter(|(t, m)| latest.get(t) != Some(m))
                    .collect();
                if changed.is_empty() {
                    return false;
                }
                let next = Arc::make_mut(latest);
                next.extend(changed);
                true
            });
        })
        .await;
    }
}

/// Sends `topics` over `socket` until the client goes away.
pub async fn serve(mut socket: WebSocket, node: Arc<Node>, topics: Vec<Topic>) {
    let mut latest = node.events.latest.subscribe();
    // Fresh state first: nobody may have been listening so far. It goes to
    // the other clients too, so that nobody gets an older state later.
    let fresh = {
        let (node, topics) = (node.clone(), topics.clone());
        tokio::task::spawn_blocking(move || {
            topics
                .into_iter()
                .map(|t| (t, t.message(&node)))
                .collect::<Snapshots>()
        })
        .await
        .unwrap_or_default()
    };
    node.events
        .latest
        .send_modify(|latest| Arc::make_mut(latest).extend(fresh));
    let mut sent = Snapshots::new();
    loop {
        let snapshots = latest.borrow_and_update().clone();
        if send_changed(&mut socket, &topics, &snapshots, &mut sent)
            .await
            .is_err()
        {
            return;
        }
        tokio::select! {
            received = socket.recv() => match received {
                // Pings are answered by the socket itself.
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
            changed = latest.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    }
}

/// Sends the topics whose state differs from what was sent.
async fn send_changed(
    socket: &mut WebSocket,
    topics: &[Topic],
    snapshots: &Snapshots,
    sent: &mut Snapshots,
) -> Result<(), axum::Error> {
    for topic in topics {
        let Some(message) = snapshots.get(topic) else {
            continue;
        };
        if sent.get(topic) != Some(message) {
            socket.send(Message::Text(message.as_ref().into())).await?;
            sent.insert(*topic, message.clone());
        }
    }
    Ok(())
}
