//! HTTP API. For now only `/x-orpheus/v1/status`; IS-04 and IS-05 get mounted here later.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use orpheus_core::DeviceId;
use orpheus_engine::PortStats;
use serde::Serialize;

pub struct NodeInfo {
    pub id: DeviceId,
    pub name: String,
    pub loopback: Option<Arc<PortStats>>,
}

#[derive(Serialize)]
struct Status {
    id: String,
    name: String,
    version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    loopback: Option<StreamStatus>,
}

#[derive(Serialize)]
struct StreamStatus {
    underruns: u64,
    overruns: u64,
    resyncs: u64,
    level_frames: u64,
}

pub fn router(node: Arc<NodeInfo>) -> Router {
    Router::new()
        .route("/x-orpheus/v1/status", get(status))
        .with_state(node)
}

async fn status(State(node): State<Arc<NodeInfo>>) -> Json<Status> {
    Json(Status {
        id: node.id.to_string(),
        name: node.name.clone(),
        version: env!("CARGO_PKG_VERSION"),
        loopback: node.loopback.as_ref().map(|stats| {
            let s = stats.snapshot();
            StreamStatus {
                underruns: s.underruns,
                overruns: s.overruns,
                resyncs: s.resyncs,
                level_frames: s.level_frames,
            }
        }),
    })
}
