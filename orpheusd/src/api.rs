//! HTTP API. For now only `/x-orpheus/v1/status`; IS-04 and IS-05 get mounted here later.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use orpheus_core::DeviceId;
use serde::Serialize;

pub struct NodeInfo {
    pub id: DeviceId,
    pub name: String,
}

#[derive(Serialize)]
struct Status {
    id: String,
    name: String,
    version: &'static str,
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
    })
}
