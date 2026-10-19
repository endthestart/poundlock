//! The node's action loop: ask the controller for work, run it, report the
//! outcome, and keep the periodic upkeep going.
use super::{NodeClient, NodeCredential};
use crate::domain::NodeAction;
use anyhow::Result;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// The machine-local side of the loop. The loop owns the controller
/// conversation and its pacing; the host owns everything that touches this
/// machine, so the pacing can be exercised against a fake host.
pub(super) trait Host {
    /// Run one controller action to completion.
    async fn execute(&mut self, action: &NodeAction) -> Result<Value>;
    /// Called after a poll that returned no action, with nothing in flight.
    async fn idle(&mut self, api: &NodeClient, credential: &NodeCredential);
    /// Upkeep that has to keep its own cadence: model tick and status report,
    /// sleep-inhibitor expiry.
    async fn maintain(&mut self);
}

/// Polls until the process ends. Each poll asks the controller to hold it open
/// for `interval`, so an action queued while the node is idle arrives at once
/// and upkeep still runs about once per `interval`.
///
/// A controller that predates that option answers an empty poll immediately.
/// Polling again straight away would be a busy loop, so an empty reply that
/// came back in under half the requested wait is treated as one, and the node
/// sleeps `interval` before the next poll, as it always did. A reply after an
/// action, or one that was really held, is followed by the next poll at once.
pub(super) async fn serve(
    api: &NodeClient,
    credential: &NodeCredential,
    interval: Duration,
    host: &mut impl Host,
) -> ! {
    loop {
        let asked = Instant::now();
        let polled = api.poll(credential, interval).await;
        let waited = asked.elapsed();
        let mut rest = Duration::ZERO;
        match polled {
            Ok(Some(action)) => {
                let (success, payload) = match host.execute(&action).await {
                    Ok(value) => (true, value),
                    Err(error) => (false, json!({"message":error.to_string()})),
                };
                if let Err(error) = api.complete(credential, &action.id, success, payload).await {
                    tracing::error!(action_id=%action.id, %error, "could not complete node action");
                }
            }
            Ok(None) => {
                host.idle(api, credential).await;
                if waited < interval / 2 {
                    rest = interval;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "node action poll failed");
                rest = interval;
            }
        }
        host.maintain().await;
        if !rest.is_zero() {
            tokio::time::sleep(rest).await;
        }
    }
}

#[cfg(test)]
#[path = "poll_loop_tests.rs"]
mod tests;
