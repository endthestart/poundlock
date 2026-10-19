//! In-process wakeups for node action traffic, so a waiter learns of a change
//! when it happens instead of finding it on its next look at the database.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::Notify;

/// Wakeups for one node. Both use `notify_waiters`, which does not store a
/// permit: take `notified()` before reading the state it announces, and a
/// change made in between is not missed.
#[derive(Default)]
pub(crate) struct NodeSignal {
    /// An action is waiting for the node to pick up, or the node's
    /// credentials changed and a held poll should re-check them.
    pub(crate) queued: Notify,
    /// An action of the node completed or failed.
    pub(crate) settled: Notify,
}

/// One `NodeSignal` per node, created on first use. Entries are never
/// removed: a node id is only reachable after enrollment or authentication.
#[derive(Default)]
pub(super) struct NodeSignals(Mutex<HashMap<String, Arc<NodeSignal>>>);

impl NodeSignals {
    pub(super) fn node(&self, node_id: &str) -> Arc<NodeSignal> {
        // The map is never left half-updated, so a poisoned lock is still good.
        let mut nodes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        nodes.entry(node_id.to_owned()).or_default().clone()
    }
}
