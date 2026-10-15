//! The IS-08 Channel Mapping API (`/x-nmos/channelmapping/v1.0/…`).
//!
//! Each receiver is one input (`rx-<id>`), the stream it plays, and one
//! output (`out-<id>`), what it plays on its output. An output takes only
//! its own receiver's input: a map picks, per output channel, a channel of
//! the stream or none (muted). Inputs and outputs come and go with the
//! receivers.
//!
//! The maps are kept here per receiver, also while the receiver plays
//! nothing, and handed to the engine whenever the receiver runs.
//! Activations are immediate or scheduled, as in IS-05; each gives the
//! NMOS device a new version.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use orpheus_core::ChannelMap;
use orpheus_engine::ReceiverId;
use orpheus_nmos::is04::channel_symbol;
use orpheus_nmos::is05::{Activation, Mode};
use orpheus_nmos::{Uuid, resource_id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::AbortHandle;

use crate::api::{Fail, Node};
use crate::nmos::{FORMAT, error, now};

pub const API_VERSION: &str = "v1.0";
const BASE: &str = "/x-nmos/channelmapping/v1.0";
/// The device control that announces this API.
pub const CONTROL: &str = "urn:x-nmos:control:cm-ctrl/v1.0";

/// One channel of an output in a map: the input channel routed to it,
/// both `null` for none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    input: Option<String>,
    channel_index: Option<usize>,
}

/// Map entries by output and channel index.
type Entries = BTreeMap<String, BTreeMap<String, Entry>>;

/// A scheduled activation.
struct Scheduled {
    activation: Activation,
    action: Entries,
    task: AbortHandle,
}

/// The channel maps of the receivers and the activations.
pub struct Mapping {
    maps: BTreeMap<ReceiverId, ChannelMap>,
    /// The last activation carried out.
    last: Activation,
    pending: BTreeMap<String, Scheduled>,
    /// Makes activation IDs unique across restarts.
    started: u64,
    count: u64,
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            maps: BTreeMap::new(),
            last: Activation::default(),
            pending: BTreeMap::new(),
            started: now().tai_nanos() / 1_000_000_000,
            count: 0,
        }
    }
}

impl Mapping {
    /// Receiver `id`'s map; every channel on itself unless changed.
    pub fn map(&self, id: ReceiverId) -> ChannelMap {
        self.maps
            .get(&id)
            .copied()
            .unwrap_or(ChannelMap::identity(channels()))
    }

    /// Drops the map of a receiver that is gone.
    pub fn forget(&mut self, id: ReceiverId) {
        self.maps.remove(&id);
    }

    fn next_id(&mut self) -> String {
        self.count += 1;
        format!("{}-{}", self.started, self.count)
    }
}

fn channels() -> usize {
    FORMAT.channels as usize
}

fn input_id(id: ReceiverId) -> String {
    format!("rx-{id}")
}

fn output_id(id: ReceiverId) -> String {
    format!("out-{id}")
}

/// The receiver of input or output `name`, if it is one of `ids`.
fn named(ids: &[ReceiverId], prefix: &str, name: &str) -> Option<ReceiverId> {
    let id: ReceiverId = name.strip_prefix(prefix)?.parse().ok()?;
    ids.contains(&id).then_some(id)
}

/// The receiver of input or output `name`, if it exists.
fn receiver_of(node: &Node, prefix: &str, name: &str) -> Option<ReceiverId> {
    named(&receivers(node), prefix, name)
}

fn labels() -> Value {
    let n = channels();
    (0..n)
        .map(|i| {
            let label =
                channel_symbol(n, i).map_or_else(|| format!("Channel {}", i + 1), str::to_owned);
            json!({ "label": label })
        })
        .collect()
}

fn input(node: &Node, id: ReceiverId) -> Value {
    json!({
        "properties": {
            "name": format!("Receiver {id}"),
            "description": format!("The stream receiver {id} plays"),
        },
        // The receiver rather than the sender's source, which this node's
        // Node API doesn't list.
        "parent": { "id": crate::connection::receiver_uuid(node, id), "type": "receiver" },
        "channels": labels(),
        "caps": { "reordering": true, "block_size": 1 },
    })
}

fn output(id: ReceiverId) -> Value {
    json!({
        "properties": {
            "name": format!("Receiver {id} output"),
            "description": format!("What receiver {id} plays on its output"),
        },
        "sourceid": null,
        "channels": labels(),
        // Its own receiver's stream, or nothing.
        "caps": { "routable_inputs": [input_id(id), null] },
    })
}

/// The map of receiver `id` as entries of its output.
fn entries(id: ReceiverId, map: ChannelMap) -> BTreeMap<String, Entry> {
    map.iter()
        .enumerate()
        .map(|(out, from)| {
            let entry = Entry {
                input: from.map(|_| input_id(id)),
                channel_index: from.map(usize::from),
            };
            (out.to_string(), entry)
        })
        .collect()
}

fn receivers(node: &Node) -> Vec<ReceiverId> {
    node.connections.lock().unwrap().receiver_ids()
}

pub fn router() -> Router<Arc<Node>> {
    let list = |items: &'static [&'static str]| get(move || async move { Json(items) });
    Router::new()
        .route("/x-nmos/channelmapping", list(&["v1.0/"]))
        .route(BASE, list(&["inputs/", "outputs/", "map/", "io/"]))
        .route(&format!("{BASE}/inputs"), get(inputs))
        .route(&format!("{BASE}/inputs/{{id}}"), get(input_index))
        .route(&format!("{BASE}/inputs/{{id}}/{{field}}"), get(input_field))
        .route(&format!("{BASE}/outputs"), get(outputs))
        .route(&format!("{BASE}/outputs/{{id}}"), get(output_index))
        .route(
            &format!("{BASE}/outputs/{{id}}/{{field}}"),
            get(output_field),
        )
        .route(&format!("{BASE}/io"), get(io))
        .route(&format!("{BASE}/map"), list(&["activations/", "active/"]))
        .route(&format!("{BASE}/map/active"), get(active))
        .route(&format!("{BASE}/map/active/{{id}}"), get(active_output))
        .route(
            &format!("{BASE}/map/activations"),
            get(activations).post(activate),
        )
        .route(
            &format!("{BASE}/map/activations/{{id}}"),
            get(activation).merge(delete(cancel)),
        )
}

async fn inputs(State(node): State<Arc<Node>>) -> Json<Vec<String>> {
    Json(
        receivers(&node)
            .into_iter()
            .map(|id| format!("{}/", input_id(id)))
            .collect(),
    )
}

async fn outputs(State(node): State<Arc<Node>>) -> Json<Vec<String>> {
    Json(
        receivers(&node)
            .into_iter()
            .map(|id| format!("{}/", output_id(id)))
            .collect(),
    )
}

async fn input_index(State(node): State<Arc<Node>>, Path(name): Path<String>) -> Response {
    match receiver_of(&node, "rx-", &name) {
        Some(_) => Json(["properties/", "parent/", "channels/", "caps/"]).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("no input {name}")),
    }
}

async fn output_index(State(node): State<Arc<Node>>, Path(name): Path<String>) -> Response {
    match receiver_of(&node, "out-", &name) {
        Some(_) => Json(["properties/", "sourceid/", "channels/", "caps/"]).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("no output {name}")),
    }
}

async fn input_field(
    State(node): State<Arc<Node>>,
    Path((name, field)): Path<(String, String)>,
) -> Response {
    let Some(id) = receiver_of(&node, "rx-", &name) else {
        return error(StatusCode::NOT_FOUND, format!("no input {name}"));
    };
    match input(&node, id).get(&field) {
        Some(v) => Json(v.clone()).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("no {field} of an input")),
    }
}

async fn output_field(
    State(node): State<Arc<Node>>,
    Path((name, field)): Path<(String, String)>,
) -> Response {
    let Some(id) = receiver_of(&node, "out-", &name) else {
        return error(StatusCode::NOT_FOUND, format!("no output {name}"));
    };
    match output(id).get(&field) {
        Some(v) => Json(v.clone()).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("no {field} of an output")),
    }
}

async fn io(State(node): State<Arc<Node>>) -> Json<Value> {
    let ids = receivers(&node);
    let inputs: serde_json::Map<String, Value> = ids
        .iter()
        .map(|&id| (input_id(id), input(&node, id)))
        .collect();
    let outputs: serde_json::Map<String, Value> = ids
        .iter()
        .map(|&id| {
            let mut o = output(id);
            // The /io view names it differently.
            let source = o.as_object_mut().and_then(|o| o.remove("sourceid"));
            o["source_id"] = source.unwrap_or(Value::Null);
            (output_id(id), o)
        })
        .collect();
    Json(json!({ "inputs": inputs, "outputs": outputs }))
}

/// The active map of the receivers `ids`.
fn active_map(node: &Node, ids: &[ReceiverId]) -> Value {
    let m = node.mapping.lock().unwrap();
    let map: BTreeMap<String, _> = ids
        .iter()
        .map(|&id| (output_id(id), entries(id, m.map(id))))
        .collect();
    json!({ "activation": m.last, "map": map })
}

async fn active(State(node): State<Arc<Node>>) -> Json<Value> {
    let ids = receivers(&node);
    Json(active_map(&node, &ids))
}

async fn active_output(State(node): State<Arc<Node>>, Path(name): Path<String>) -> Response {
    match receiver_of(&node, "out-", &name) {
        Some(id) => Json(active_map(&node, &[id])).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("no output {name}")),
    }
}

fn scheduled(s: &Scheduled) -> Value {
    json!({ "activation": s.activation, "action": s.action })
}

async fn activations(State(node): State<Arc<Node>>) -> Json<Value> {
    let m = node.mapping.lock().unwrap();
    let all: serde_json::Map<String, Value> = m
        .pending
        .iter()
        .map(|(id, s)| (id.clone(), scheduled(s)))
        .collect();
    Json(Value::Object(all))
}

async fn activation(State(node): State<Arc<Node>>, Path(id): Path<String>) -> Response {
    let m = node.mapping.lock().unwrap();
    match m.pending.get(&id) {
        Some(s) => Json(scheduled(s)).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("no activation {id} pending")),
    }
}

async fn cancel(State(node): State<Arc<Node>>, Path(id): Path<String>) -> Response {
    let mut m = node.mapping.lock().unwrap();
    match m.pending.remove(&id) {
        Some(s) => {
            s.task.abort();
            tracing::info!(id, "channel map activation cancelled");
            StatusCode::NO_CONTENT.into_response()
        }
        None => error(StatusCode::NOT_FOUND, format!("no activation {id} pending")),
    }
}

#[derive(Deserialize)]
struct Request {
    activation: Activation,
    action: Entries,
}

async fn activate(
    State(node): State<Arc<Node>>,
    body: Result<Json<Request>, JsonRejection>,
) -> Response {
    let req = match body {
        Ok(Json(r)) => r,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let result = tokio::task::spawn_blocking(move || post(&node, req))
        .await
        .unwrap_or_else(|e| Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())));
    match result {
        Ok((code, v)) => (code, Json(v)).into_response(),
        Err((code, msg)) => error(code, msg),
    }
}

/// The maps `action` makes of the current ones, by receiver; `ids` are
/// the receivers.
fn changes(
    ids: &[ReceiverId],
    m: &Mapping,
    action: &Entries,
) -> Result<BTreeMap<ReceiverId, ChannelMap>, Fail> {
    let bad = |msg: String| (StatusCode::BAD_REQUEST, msg);
    // An empty one would lock nothing, so any number could be pending.
    if action.is_empty() {
        return Err(bad("an activation needs at least one output".into()));
    }
    let n = channels();
    let mut maps = BTreeMap::new();
    for (name, chans) in action {
        let id = named(ids, "out-", name).ok_or_else(|| bad(format!("no output {name}")))?;
        let mut from: Vec<Option<u8>> = m.map(id).iter().collect();
        for (index, entry) in chans {
            let out: usize = index
                .parse()
                .ok()
                .filter(|&i| i < n)
                .ok_or_else(|| bad(format!("{name} has no channel {index}")))?;
            from[out] = match (&entry.input, entry.channel_index) {
                (None, None) => None,
                (Some(input), Some(i)) => {
                    if *input != input_id(id) {
                        return Err(bad(format!(
                            "{name} can only take {} (see its routable_inputs), not {input}",
                            input_id(id)
                        )));
                    }
                    if i >= n {
                        return Err(bad(format!("{input} has no channel {i}")));
                    }
                    Some(i as u8)
                }
                _ => {
                    return Err(bad(format!(
                        "{name} channel {index}: input and channel_index must both be null or both be set"
                    )));
                }
            };
        }
        let map = ChannelMap::new(&from).expect("channels checked");
        maps.insert(id, map);
    }
    Ok(maps)
}

fn post(node: &Arc<Node>, req: Request) -> Result<(StatusCode, Value), Fail> {
    let mode = req
        .activation
        .mode
        .ok_or((StatusCode::BAD_REQUEST, "an activation needs a mode".into()))?;
    let ids = receivers(node);
    let mut m = node.mapping.lock().unwrap();
    // Checked now, also for scheduled activations, so that a bad one is
    // refused at once.
    changes(&ids, &m, &req.action)?;
    if let Some(name) = req
        .action
        .keys()
        .find(|name| m.pending.values().any(|s| s.action.contains_key(*name)))
    {
        return Err((
            StatusCode::LOCKED,
            format!("an activation is scheduled for {name}; cancel it first"),
        ));
    }
    let id = m.next_id();
    let now = now();
    if mode == Mode::ActivateImmediate {
        drop(m);
        let done = Activation {
            mode: Some(mode),
            requested_time: None,
            activation_time: Some(now),
        };
        apply(node, &req.action, done)?;
        tracing::info!(id, action = ?req.action, "channel map activated");
        return Ok((
            StatusCode::OK,
            json!({ id: { "activation": done, "action": req.action } }),
        ));
    }
    let requested = req.activation.requested_time.ok_or((
        StatusCode::BAD_REQUEST,
        "a scheduled activation needs requested_time".into(),
    ))?;
    let at = match mode {
        Mode::ActivateScheduledAbsolute => requested,
        _ => orpheus_nmos::is04::Version::from_tai_nanos(
            now.tai_nanos().saturating_add(requested.tai_nanos()),
        ),
    };
    let activation = Activation {
        mode: Some(mode),
        requested_time: Some(requested),
        activation_time: Some(at),
    };
    let wait = Duration::from_nanos(at.tai_nanos().saturating_sub(now.tai_nanos()));
    let task = tokio::runtime::Handle::current().spawn({
        let node = node.clone();
        let id = id.clone();
        async move {
            tokio::time::sleep(wait).await;
            let _ = tokio::task::spawn_blocking(move || fire(&node, &id)).await;
        }
    });
    let s = Scheduled {
        activation,
        action: req.action,
        task: task.abort_handle(),
    };
    let reply = json!({ &id: scheduled(&s) });
    tracing::info!(id, at = ?at, "channel map activation scheduled");
    m.pending.insert(id, s);
    Ok((StatusCode::ACCEPTED, reply))
}

/// Carries out scheduled activation `id`, unless it was cancelled.
fn fire(node: &Node, id: &str) {
    let Some(s) = node.mapping.lock().unwrap().pending.remove(id) else {
        return;
    };
    // Outputs whose receiver went meanwhile are left out.
    let mut action = s.action;
    action.retain(|name, _| receiver_of(node, "out-", name).is_some());
    match apply(node, &action, s.activation) {
        Ok(()) => tracing::info!(id, "scheduled channel map activated"),
        Err((_, e)) => tracing::warn!(id, error = %e, "scheduled channel map activation"),
    }
}

/// Makes the maps of `action` active and hands them to the receivers that
/// run.
fn apply(node: &Node, action: &Entries, done: Activation) -> Result<(), Fail> {
    let conns = node.connections.lock().unwrap();
    let engine = node.engine.as_ref().map(|e| e.lock().unwrap());
    let mut m = node.mapping.lock().unwrap();
    let maps = changes(&conns.receiver_ids(), &m, action)?;
    for (id, map) in maps {
        if let Some(engine) = &engine {
            engine.set_receiver_map(id, map);
        }
        m.maps.insert(id, map);
    }
    m.last = done;
    drop(m);
    drop(engine);
    drop(conns);
    let device: Uuid = resource_id(node.id, "device");
    node.nmos.lock().unwrap().touch(device);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(v: Value) -> Entries {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn activations_change_only_the_channels_named() {
        let mut m = Mapping::default();
        let swap = action(json!({"out-1": {
            "0": {"input": "rx-1", "channel_index": 1},
            "1": {"input": "rx-1", "channel_index": 0},
        }}));
        let maps = changes(&[0, 1], &m, &swap).unwrap();
        assert_eq!(maps[&1], ChannelMap::new(&[Some(1), Some(0)]).unwrap());
        assert!(!maps.contains_key(&0));
        m.maps.extend(maps);
        let mute = action(json!({"out-1": {"1": {"input": null, "channel_index": null}}}));
        let maps = changes(&[0, 1], &m, &mute).unwrap();
        assert_eq!(maps[&1], ChannelMap::new(&[Some(1), None]).unwrap());
        assert_eq!(
            entries(1, maps[&1])["1"],
            Entry {
                input: None,
                channel_index: None
            }
        );
    }

    #[test]
    fn activations_outside_the_constraints_are_refused() {
        let m = Mapping::default();
        let refused = |v: Value| {
            let e = changes(&[0, 1], &m, &action(v)).unwrap_err();
            assert_eq!(e.0, StatusCode::BAD_REQUEST);
            e.1
        };
        let other = refused(json!({"out-0": {"0": {"input": "rx-1", "channel_index": 0}}}));
        assert!(other.contains("only take rx-0"), "{other}");
        refused(json!({"out-0": {"0": {"input": "rx-0", "channel_index": null}}}));
        refused(json!({"out-0": {"0": {"input": "rx-0", "channel_index": 2}}}));
        refused(json!({"out-0": {"2": {"input": null, "channel_index": null}}}));
        refused(json!({}));
        refused(json!({"out-0": {"x": {"input": null, "channel_index": null}}}));
        refused(json!({"out-7": {}}));
    }
}
