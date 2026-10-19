//! Pacing of the node loop against stand-in controllers over real HTTP. The
//! interval is the configuration minimum, one second, because `wait_seconds`
//! is whole seconds; each test therefore runs a few real seconds.
use super::*;
use crate::node::network::NetworkPolicy;
use crate::node::{Local, NodeConfig, NodeOptions, inhibitor, ollama};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::process::Command;

const INTERVAL: Duration = Duration::from_secs(1);

/// How a stand-in controller answers a poll that has no action to give.
#[derive(Clone, Copy)]
enum Empty {
    /// At once, ignoring `wait_seconds`: a controller that predates it.
    AtOnce,
    /// After `wait_seconds`, as the current controller does.
    AfterWait,
    /// With an error.
    Fails,
}

#[derive(Default)]
struct Seen {
    polls: Mutex<Vec<(Instant, Value)>>,
    completed: Mutex<Vec<(Instant, String, Value)>>,
    queued: Mutex<VecDeque<Value>>,
}

impl Seen {
    fn poll_times(&self) -> Vec<Instant> {
        self.polls
            .lock()
            .unwrap()
            .iter()
            .map(|(at, _)| *at)
            .collect()
    }
}

async fn controller(empty: Empty, actions: &[&str]) -> (String, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    for id in actions {
        seen.queued.lock().unwrap().push_back(json!({
            "id":id,"node_id":"node","job_id":null,"kind":"ensure",
            "payload":{},"status":"running","result":null
        }));
    }
    let app = Router::new()
        .route(
            "/node/v1/nodes/{node}/actions/poll",
            post(
                move |State(seen): State<Arc<Seen>>, Json(body): Json<Value>| async move {
                    let wait = body["wait_seconds"].as_u64().unwrap_or(0);
                    seen.polls.lock().unwrap().push((Instant::now(), body));
                    let next = seen.queued.lock().unwrap().pop_front();
                    if let Some(action) = next {
                        return Json(action).into_response();
                    }
                    match empty {
                        Empty::AtOnce => {}
                        Empty::AfterWait => tokio::time::sleep(Duration::from_secs(wait)).await,
                        Empty::Fails => {
                            return (StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
                                .into_response();
                        }
                    }
                    Json(Value::Null).into_response()
                },
            ),
        )
        .route(
            "/node/v1/nodes/{node}/actions/{action}/complete",
            post(
                |State(seen): State<Arc<Seen>>,
                 Path((_, action)): Path<(String, String)>,
                 Json(body): Json<Value>| async move {
                    seen.completed
                        .lock()
                        .unwrap()
                        .push((Instant::now(), action, body));
                    Json(json!({"accepted":true})) as Json<Value>
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

fn client(url: &str) -> NodeClient {
    NodeClient::new(url.into(), &NetworkPolicy::default()).unwrap()
}

fn credential() -> NodeCredential {
    NodeCredential {
        node_id: "node".into(),
        credential: "secret".into(),
    }
}

#[derive(Default)]
struct Recording {
    executed: Vec<String>,
    maintained: Vec<Instant>,
}

impl Host for Recording {
    async fn execute(&mut self, action: &NodeAction) -> Result<Value> {
        self.executed.push(action.id.clone());
        Ok(json!({"status":"ready"}))
    }
    async fn idle(&mut self, _api: &NodeClient, _credential: &NodeCredential) {}
    async fn maintain(&mut self) {
        self.maintained.push(Instant::now());
    }
}

/// Runs the loop against `url` for `duration`, then stops it.
async fn run_for(url: &str, host: &mut impl Host, duration: Duration) {
    let api = client(url);
    let _ = tokio::time::timeout(duration, serve(&api, &credential(), INTERVAL, host)).await;
}

fn gaps(times: &[Instant]) -> Vec<Duration> {
    times.windows(2).map(|pair| pair[1] - pair[0]).collect()
}

#[tokio::test]
async fn a_controller_that_ignores_wait_seconds_is_polled_at_the_interval() {
    let (url, seen) = controller(Empty::AtOnce, &[]).await;
    let mut host = Recording::default();
    run_for(&url, &mut host, Duration::from_millis(3500)).await;
    let polls = seen.poll_times();
    // One poll at 0, 1, 2 and 3 seconds; a busy loop would make thousands.
    assert!((3..=5).contains(&polls.len()), "{} polls", polls.len());
    for gap in gaps(&polls) {
        assert!(gap >= INTERVAL, "polled again after only {gap:?}");
    }
    assert!(
        seen.polls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, body)| body["wait_seconds"] == 1),
        "the node still asks, so it benefits once the controller is upgraded"
    );
    assert_eq!(host.maintained.len(), polls.len());
}

#[tokio::test]
async fn a_failing_controller_is_polled_at_the_interval() {
    let (url, seen) = controller(Empty::Fails, &[]).await;
    let mut host = Recording::default();
    run_for(&url, &mut host, Duration::from_millis(3500)).await;
    let polls = seen.poll_times();
    assert!((3..=5).contains(&polls.len()), "{} polls", polls.len());
    assert_eq!(host.maintained.len(), polls.len());
}

#[tokio::test]
async fn the_next_poll_follows_an_action_at_once() {
    let (url, seen) = controller(Empty::AtOnce, &["first", "second"]).await;
    let mut host = Recording::default();
    let started = Instant::now();
    run_for(&url, &mut host, Duration::from_millis(900)).await;
    assert_eq!(host.executed, ["first", "second"]);
    let completed: Vec<_> = seen
        .completed
        .lock()
        .unwrap()
        .iter()
        .map(|(at, id, body)| (*at, id.clone(), body["success"].clone()))
        .collect();
    assert_eq!(
        completed
            .iter()
            .map(|(_, id, success)| (id.as_str(), success == &json!(true)))
            .collect::<Vec<_>>(),
        [("first", true), ("second", true)]
    );
    // Both actions and the poll after them were served well inside one
    // interval; a sleep after each action would push the second past a second.
    let polls = seen.poll_times();
    assert_eq!(polls.len(), 3, "action, action, then an empty poll");
    assert!(completed[1].0 - started < Duration::from_millis(800));
    assert!(polls[2] - started < Duration::from_millis(800));
}

#[tokio::test]
async fn held_polls_follow_each_other_and_upkeep_keeps_its_cadence() {
    let (url, seen) = controller(Empty::AfterWait, &[]).await;
    let mut host = Recording::default();
    run_for(&url, &mut host, Duration::from_millis(3500)).await;
    // Each poll was held for the interval and the next began at once: no
    // second interval spent sleeping on top of the hold.
    let polls = seen.poll_times();
    assert!((3..=5).contains(&polls.len()), "{} polls", polls.len());
    for gap in gaps(&polls) {
        assert!(
            gap >= INTERVAL && gap < Duration::from_millis(1500),
            "poll gap {gap:?}"
        );
    }
    // Upkeep ran once per hold, so about once per interval.
    assert!(
        (3..=4).contains(&host.maintained.len()),
        "{} upkeeps",
        host.maintained.len()
    );
    for gap in gaps(&host.maintained) {
        assert!(gap < Duration::from_millis(1500), "upkeep gap {gap:?}");
    }
}

#[tokio::test]
async fn model_tick_and_inhibitor_expiry_run_on_schedule_while_a_poll_is_held() {
    let (url, _seen) = controller(Empty::AfterWait, &[]).await;
    let directory = tempfile::tempdir().unwrap();
    let config: NodeConfig = toml::from_str(
        "controller = 'http://127.0.0.1:1'\nname = 'test'\nauto_update = false\n\
         heartbeat_seconds = 1\nlease_timeout_seconds = 2\n[runtimes.always-ready]\n",
    )
    .unwrap();
    let options = NodeOptions {
        controller: config.controller.clone(),
        name: config.name.clone(),
        enrollment_token: None,
        credential_file: directory.path().join("credential.json"),
        config_file: directory.path().join("node.toml"),
        data_dir: directory.path().to_path_buf(),
    };
    let models = ollama::Manager::open(config.ollama.clone(), directory.path()).unwrap();
    let (models_tx, models_rx) = tokio::sync::watch::channel(models.report());
    assert_eq!(models_rx.borrow()["status"], "checking");
    let mut inhibitors = inhibitor::Inhibitors::new();
    inhibitors.insert(
        "lease".into(),
        inhibitor::held(Command::new("sleep").arg("60").spawn().unwrap()),
    );
    let mut host = Local {
        config: &config,
        options: &options,
        inhibitors,
        models,
        models_tx,
        last_update_check: Instant::now(),
    };

    // After one hold the model observer has run, but the lease has not yet
    // gone its two seconds without a renewal.
    run_for(&url, &mut host, Duration::from_millis(1400)).await;
    assert_ne!(models_rx.borrow()["status"], "checking");
    assert!(host.inhibitors.contains_key("lease"));
    // The next upkeep, one hold later, expires it.
    run_for(&url, &mut host, Duration::from_millis(1400)).await;
    assert!(host.inhibitors.is_empty(), "the stale inhibitor lingered");
}
