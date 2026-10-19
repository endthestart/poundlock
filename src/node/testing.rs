//! A stand-in node for controller tests: the real poll loop and controller
//! client, over a runtime that answers every action at once.
use super::network::NetworkPolicy;
use super::poll_loop::{Host, serve};
use super::{NodeClient, NodeCredential};
use crate::domain::NodeAction;
use anyhow::Result;
use serde_json::{Value, json};
use std::time::Duration;

struct AlwaysReady;

impl Host for AlwaysReady {
    async fn execute(&mut self, _action: &NodeAction) -> Result<Value> {
        Ok(json!({"adapter":"noop","status":"ready"}))
    }
    async fn idle(&mut self, _api: &NodeClient, _credential: &NodeCredential) {}
    async fn maintain(&mut self) {}
}

/// Runs a node against `controller` until the task is dropped.
pub(crate) async fn serve_always_ready(
    controller: &str,
    node_id: &str,
    credential: &str,
    interval: Duration,
) -> ! {
    let api = NodeClient::new(controller.into(), &NetworkPolicy::default()).unwrap();
    let credential = NodeCredential {
        node_id: node_id.into(),
        credential: credential.into(),
    };
    serve(&api, &credential, interval, &mut AlwaysReady).await
}
